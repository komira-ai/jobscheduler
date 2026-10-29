//! The load a heartbeat carries: [`LoadReader`] turns one reading of the actions control
//! group (see [`crate::usage`]) into the wire's [`NodeLoad`].
//!
//! # Units
//!
//! The numbers are the sampler's, unchanged and in the units the wire and the scheduler
//! (`js_types::sched::usage::NodeLoad`) both use: CPU time in microseconds of the
//! cgroup's `cpu.stat` `usage_usec` (a counter, not a rate), memory in bytes, per-lease
//! CPU rates in thousandths of a core, times in microseconds on the agent's monotonic
//! clock (the clock the lifecycle stamps a run's start with, whole milliseconds counted as
//! microseconds).
//!
//! # Absent versus zero
//!
//! A reading that cannot be taken gives an error, never a load of zeros: the heartbeat then
//! carries no load, and the session says why once. A session without `--actions-cgroup`
//! has no reader and always sends none.
//!
//! # Delivery
//!
//! The closed minutes a reading carries are taken out of the [`Binner`] with it. Whoever
//! sends the heartbeat reports the outcome with [`LoadReader::settle`]; minutes that did
//! not arrive are given back, and go out on a later heartbeat.

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::limits::{MAX_LOAD_LEASES, MAX_LOAD_MINUTES};
use js_agent_wire::{LeaseClock, LeaseMinute, LoadDrops, NodeLoad};

use crate::facts::FileReader;
use crate::lifecycle::{Clock, RunningLease};
use crate::system::SharedClock;
use crate::usage::{Binner, ClosedMinute, LeaseCgroup, SampleError, Sampler, UsageClock};

/// Microseconds in a millisecond: the lifecycle's clock counts milliseconds.
const USEC_PER_MS: u64 = 1000;

/// The lifecycle's clock read in microseconds, so a reading and a run's start share a base.
struct MillisAsMicros<'a>(&'a SharedClock);

impl UsageClock for MillisAsMicros<'_> {
    fn now_usec(&self) -> u64 {
        self.0.now_ms().saturating_mul(USEC_PER_MS)
    }
}

/// Reads the actions control group and the runs' control groups, and keeps what the next
/// reading needs: the per-lease minutes, the reading's number and the minutes in flight.
pub struct LoadReader<'a> {
    sampler: Sampler<&'a dyn FileReader>,
    clock: SharedClock,
    binner: Binner,
    seq: u64,
    tracked: BTreeSet<String>,
    in_flight: Vec<ClosedMinute>,
}

impl<'a> LoadReader<'a> {
    /// A reader of the control group `actions_dir` (`--actions-cgroup`) through `files`,
    /// stamping readings with `clock`, which must be the clock that stamps the runs' starts.
    pub fn new(files: &'a dyn FileReader, actions_dir: &str, clock: SharedClock) -> Self {
        Self {
            sampler: Sampler::new(files, actions_dir),
            clock,
            binner: Binner::new(1),
            seq: 0,
            tracked: BTreeSet::new(),
            in_flight: Vec::new(),
        }
    }

    /// One reading of the node and of `leases`, as the load of the next heartbeat. `window`
    /// is the time between heartbeats. Readings count from 1.
    ///
    /// A run with no control group or no start time is not sampled (its cost is unknown,
    /// not zero).
    ///
    /// # Errors
    ///
    /// A file of the actions control group cannot be read: no load is made up.
    pub fn read(
        &mut self,
        leases: &[RunningLease],
        window: Duration,
    ) -> Result<NodeLoad, SampleError> {
        // Minutes still in flight were never reported as delivered.
        self.settle(false);
        let window_usec = u64::try_from(window.as_micros()).unwrap_or(u64::MAX);
        self.binner.set_window(window_usec);
        let cgroups: Vec<LeaseCgroup> = leases.iter().filter_map(cgroup_of).collect();
        let sample = self
            .sampler
            .sample(&MillisAsMicros(&self.clock), &cgroups)?;
        let running: BTreeSet<&str> = leases.iter().map(|l| l.lease_id.as_str()).collect();
        let ended: Vec<String> = self
            .tracked
            .iter()
            .filter(|id| !running.contains(id.as_str()))
            .cloned()
            .collect();
        let observed = self.binner.observe(&sample);
        for id in &ended {
            self.binner.lease_ended(id);
            self.tracked.remove(id);
        }
        self.tracked
            .extend(sample.leases.iter().map(|lease| lease.lease_id.clone()));
        let minutes = self.binner.take(MAX_LOAD_MINUTES, sample.mono_usec);
        self.in_flight.clone_from(&minutes);
        self.seq = self.seq.saturating_add(1);
        let mut covered = observed.covered;
        covered.truncate(MAX_LOAD_LEASES);
        let clocks = sample
            .leases
            .iter()
            .take(MAX_LOAD_LEASES)
            .map(|lease| LeaseClock {
                lease_id: lease.lease_id.clone(),
                started_mono_usec: lease.started_usec,
            })
            .collect();
        Ok(NodeLoad {
            seq: self.seq,
            mono_usec: sample.mono_usec,
            cpu_usage_usec: sample.cpu_usage_usec,
            working_set_bytes: sample.working_set_bytes,
            active_file_bytes: sample.active_file_bytes,
            actions_cpu_limited: sample.cpu_limited,
            covered,
            clocks,
            minutes: minutes.into_iter().map(minute).collect(),
            dropped: LoadDrops::default(),
        })
    }

    /// Reports whether the heartbeat that carried the last reading arrived. Minutes that
    /// did not are given back to be sent again; calling it again does nothing.
    pub fn settle(&mut self, delivered: bool) {
        let minutes = std::mem::take(&mut self.in_flight);
        if !delivered {
            self.binner.requeue(minutes);
        }
    }
}

fn cgroup_of(lease: &RunningLease) -> Option<LeaseCgroup> {
    Some(LeaseCgroup {
        lease_id: lease.lease_id.clone(),
        dir: lease.cgroup.as_deref()?.to_str()?.to_owned(),
        started_usec: lease.started_usec?,
    })
}

fn minute(closed: ClosedMinute) -> LeaseMinute {
    LeaseMinute {
        lease_id: closed.lease_id,
        minute: closed.minute,
        cpu_mean_millis: closed.cpu_mean_millis,
        cpu_peak_window_millis: closed.cpu_peak_window_millis,
        ws_max_bytes: closed.ws_max_bytes,
    }
}
