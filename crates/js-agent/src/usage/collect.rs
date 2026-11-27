//! The worker's side of the usage: when to sample, what to feed the binner, what to hand the
//! session.
//!
//! The worker calls [`Collector::due`] after every step, which reads the clock and nothing
//! else, and [`Collector::run`] when it says so. A run reads the cgroup files of the node and
//! of each running lease on the worker's thread (never the session's), cuts the leases'
//! counters into minutes and publishes the result in the [`Outbox`] for the next heartbeat.
//!
//! - **The window** is the session's heartbeat interval, given to the outbox when the
//!   session registers. Until then nothing is sampled. A later session may have another
//!   interval: the due time and the binner's gap rule follow the outbox at every run.
//! - **Dropped minutes** (too old to send, or refused by the server) are not on the wire;
//!   a run that finds new ones returns one line with the count.
//! - **Adopted runs** (taken over from an earlier process) are not cut into minutes: the
//!   minutes before the takeover are lost, and the server needs a lease's early minutes to
//!   learn from it. They are named in the `clocks`, so the server knows how long they ran.
//! - **A run that ended** since the last run has its open minute closed, and is reported
//!   once more as a minute (never as a covered lease).
//! - **A whole-host reading** (a Mac's, [`NodeSampler::covers_every_lease`]) has no lease of
//!   its own to read: it covers every lease that was running at the last reading published and
//!   still is, since the host's figures hold all of what they used. The server then charges
//!   them by that measurement, not by their bookings on top of it.
//! - **`seq`** counts the samples of this process from 1; a new process starts again at 1,
//!   which the server reads as a new series.
//! - **The seconds** (with `--host-second-samples`): [`Collector::step`], which the worker calls
//!   after every step in place of `due`, reads the whole host's `/proc/stat` and `/proc/pressure/cpu`
//!   (through [`super::host_cpu`], not the actions group's files; on a Mac the host's CPU load,
//!   [`super::HostSampler`])
//!   when at least a second has passed since the last such read, and keeps the busy CPU and the
//!   CPU pressure of each interval (`js_agent_usage::Seconds`), the newest 16 of each. A run
//!   publishes them in its reading and starts again; they are sent once, so a beat that fails
//!   loses them. A host without a readable `/proc/pressure/cpu` gives busy values alone.
//! - **A node that cannot be read** gives no load for that window. The reason is returned
//!   once for as long as it lasts.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use js_agent_usage::{SecondSample, Seconds};
use js_agent_wire::limits::{MAX_LOAD_MINUTES, MAX_LOAD_SECONDS};

use super::binner::Binner;
use super::clock::UsageClock;
use super::outbox::{Outbox, Reading};
use super::sampler::{LeaseCgroup, NodeSampler, SampleError, Sampler};
use crate::facts::FileReader;

/// A lease that is running, as far as its usage is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningLease {
    /// The lease that is running.
    pub lease_id: String,
    /// The container's control group directory, when the runtime could say.
    pub cgroup: Option<PathBuf>,
    /// When the container started, on the agent's monotonic clock in microseconds, to the
    /// millisecond. `None` for an adopted run that started before the clock began.
    pub started_usec: Option<u64>,
    /// The run was taken over from an earlier process of the agent, so the minutes before
    /// the takeover were never seen.
    pub adopted: bool,
}

/// Samples the node on the worker's thread. See the [module](self).
pub struct Collector {
    sampler: Box<dyn NodeSampler>,
    clock: Box<dyn UsageClock>,
    outbox: Arc<Outbox>,
    binner: Option<Binner>,
    seq: u64,
    last: Option<u64>,
    tracked: BTreeSet<String>,
    /// The leases running at the last reading that was published.
    read: BTreeSet<String>,
    unreadable: Option<SampleError>,
    /// How many dropped minutes the operator has been told of.
    told_dropped: u64,
    /// The per-second series of the whole host, when the outbox says
    /// ([`Outbox::host_seconds`]).
    seconds: Option<Seconds>,
}

impl Collector {
    /// A collector of the control group `actions_dir`, reading it with `files`, timed by
    /// `clock` and publishing in `outbox`, which also says whether it takes the per-second
    /// series.
    pub fn new(
        files: Box<dyn FileReader>,
        actions_dir: &str,
        clock: Box<dyn UsageClock>,
        outbox: Arc<Outbox>,
    ) -> Self {
        Self::with_sampler(Box::new(Sampler::new(files, actions_dir)), clock, outbox)
    }

    /// A collector of what `sampler` reads (the cgroup tree on Linux, the host's own counters
    /// on a Mac), timed by `clock` and publishing in `outbox`, which also says whether it
    /// takes the per-second series.
    pub fn with_sampler(
        sampler: Box<dyn NodeSampler>,
        clock: Box<dyn UsageClock>,
        outbox: Arc<Outbox>,
    ) -> Self {
        Self {
            seconds: outbox
                .host_seconds()
                .then(|| Seconds::new(MAX_LOAD_SECONDS)),
            sampler,
            clock,
            outbox,
            binner: None,
            seq: 0,
            last: None,
            tracked: BTreeSet::new(),
            read: BTreeSet::new(),
            unreadable: None,
            told_dropped: 0,
        }
    }

    /// What the worker calls after every step: takes the per-second sample when it is due,
    /// then says as [`Collector::due`] does. It reads the clock once more than `due` does,
    /// and the files only when a second has passed since the last sample.
    pub fn step(&mut self) -> bool {
        self.second();
        self.due()
    }

    /// One per-second sample of the whole host, when the series is on and a second has passed.
    /// A host whose `/proc/stat` cannot be read gives none (nothing is invented) and is tried
    /// again at the next step; one without a readable `/proc/pressure/cpu` gives busy values
    /// alone.
    fn second(&mut self) {
        let Some(seconds) = self.seconds.as_mut() else {
            return;
        };
        let now = self.clock.now_usec();
        if !seconds.due(now) {
            return;
        }
        let Some(host) = self.sampler.host_counters() else {
            return;
        };
        seconds.observe(SecondSample {
            mono_usec: now,
            cpu_usage_usec: host.busy_usec,
            pressure_some_usec: host.pressure_some_usec,
        });
    }

    /// Whether a window has passed since the last run. False until the session has set the
    /// window.
    pub fn due(&self) -> bool {
        let window = self.outbox.window_usec();
        let since = self
            .last
            .map(|last| self.clock.now_usec().saturating_sub(last));
        window > 0 && since.is_none_or(|since| since >= window)
    }

    /// Samples the node and `running`, and publishes the reading. Returns the lines the
    /// operator should read: the reason the node could not be read, once, and how many
    /// minutes were dropped since the last run, when any were.
    pub fn run(&mut self, running: &[RunningLease]) -> Vec<String> {
        let mut lines = self.sample(running);
        lines.extend(self.drops());
        lines
    }

    /// One line when minutes were dropped since the last line: they were too old to send
    /// or the server refused the beat that carried them. The agent's wire has no field for
    /// them, so the log is where the operator learns of the loss.
    fn drops(&mut self) -> Option<String> {
        let aged = self
            .binner
            .as_ref()
            .map_or(0, |binner| binner.counters().aged_out);
        let total = self.outbox.dropped() + aged;
        let new = total.saturating_sub(self.told_dropped);
        self.told_dropped = total;
        (new > 0).then(|| {
            format!(
                "usage: dropped {new} minute(s) too old to send or refused by the server \
                 ({total} since start)"
            )
        })
    }

    fn sample(&mut self, running: &[RunningLease]) -> Vec<String> {
        let window = self.outbox.window_usec();
        let now = self.clock.now_usec();
        self.last = Some(now);
        let binner = self.binner.get_or_insert_with(|| Binner::new(window));
        // The window is the session's and a later session may have another.
        binner.set_window(window);
        binner.requeue(self.outbox.reclaim(now));
        let alive: BTreeSet<&str> = running
            .iter()
            .filter(|lease| !lease.adopted)
            .map(|lease| lease.lease_id.as_str())
            .collect();
        for ended in self
            .tracked
            .iter()
            .filter(|id| !alive.contains(id.as_str()))
        {
            binner.lease_ended(ended);
        }
        self.tracked = alive.iter().map(|id| (*id).to_owned()).collect();
        // A whole-host reading holds every lease that ran since the last one.
        let whole: Vec<String> = alive
            .iter()
            .filter(|id| self.read.contains(**id))
            .map(|id| (*id).to_owned())
            .collect();
        let watched: Vec<LeaseCgroup> = running
            .iter()
            .filter(|lease| !lease.adopted)
            .filter_map(|lease| {
                Some(LeaseCgroup {
                    lease_id: lease.lease_id.clone(),
                    dir: lease.cgroup.as_ref()?.to_str()?.to_owned(),
                    started_usec: lease.started_usec?,
                })
            })
            .collect();
        let mut sample = match self.sampler.sample(&*self.clock, &watched) {
            Ok(sample) => sample,
            Err(why) => return self.cannot_read(why),
        };
        self.unreadable = None;
        let observed = binner.observe(&sample).covered;
        let covered = if self.sampler.covers_every_lease() {
            whole
        } else {
            observed
        };
        self.read = self.tracked.clone();
        let minutes = binner.take(MAX_LOAD_MINUTES, sample.mono_usec);
        sample.leases.clear();
        self.seq += 1;
        self.outbox.publish(Reading {
            seq: self.seq,
            clocks: clocks(running),
            covered: covered.into_iter().take(MAX_LEASES).collect(),
            minutes,
            seconds: self.seconds.as_mut().map(Seconds::take).unwrap_or_default(),
            sample,
        });
        Vec::new()
    }

    fn cannot_read(&mut self, why: SampleError) -> Vec<String> {
        match self.unreadable.replace(why) {
            Some(before) if before == why => Vec::new(),
            _ => vec![format!("usage: no load while {why}")],
        }
    }
}

/// The most leases named in the `covered` and `clocks` lists of a load.
const MAX_LEASES: usize = js_agent_wire::limits::MAX_LOAD_LEASES;

/// The start of each running lease that has one, oldest first, at most [`MAX_LEASES`].
fn clocks(running: &[RunningLease]) -> Vec<(String, u64)> {
    let mut started: Vec<(String, u64)> = running
        .iter()
        .filter_map(|lease| Some((lease.lease_id.clone(), lease.started_usec?)))
        .collect();
    started.sort_by_key(|(_, at)| *at);
    started.truncate(MAX_LEASES);
    started
}
