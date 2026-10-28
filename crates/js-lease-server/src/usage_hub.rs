//! What the server keeps between heartbeats to feed the usage seam, and when it asks the
//! sink to write and prune.
//!
//! [`Usage`] owns the sink and the key source. After the registry accepted a heartbeat the
//! agent route calls [`Usage::on_beat`] on the pool for blocking calls: the node's load is
//! checked and given to the sink, the lease minutes the sink had not seen are filed per
//! lease, and the results the beat carried are read for runs that ended cleanly. The
//! reply to the heartbeat is never touched: a load that is bad costs the usage data of
//! that beat and nothing else.
//!
//! **Clean runs.** A result the registry acknowledged `ACCEPTED` with the outcome
//! `succeeded` or `failed` is a run that ended cleanly; any other outcome (killed, timed
//! out, out of memory, failed to start) discards what was measured for its lease. The
//! [`KeySource`] names the work a clean run did; with none, the run is counted
//! (`runs_without_key`) and nothing is learned. The final partial minute of a lease rides
//! on a later heartbeat, so a clean run is handed to the sink [`GRACE_BEATS`] heartbeats
//! of its host after the one that carried its acknowledgement.
//!
//! **Locks.** The one mutex here is a leaf: nothing is called while it is held but the
//! key source (which must answer from memory and not call back into the server), and it
//! is never held across a call to the sink. The registry's lock is not held at all when
//! these methods run.
//!
//! **Time.** [`Usage::tick`] runs at every tick of the server and acts by the wall
//! minute of the server's clock: the sink is flushed every [`FLUSH_EVERY_MINUTES`]
//! minutes, and pruned at the first tick and at each change of day. Each flush logs one
//! `node minute` line for each node minute the flush closed, and one warning for each
//! node and minute that had readings dropped.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_agent_wire::{Ack, Heartbeat, NodeLoad, Outcome, ResultAck};
use js_lease::Clock;
use js_types::{HostId, LeaseId};
use tracing::{error, info, warn};

use crate::adapt::SharedClock;
use crate::usage::{
    DropReason, HostLimits, KeyProfile, KeySource, MinuteIn, NodeRow, Observed, ProfileCurves,
    ProfileKey, RunEnd, RunMinute, RunSeries, SinkCounters, UsageFault, UsageSink,
};
use crate::usage_map::{host_of, lease_of, load_in};
use crate::util::blocking;

/// Heartbeats of a host after the one that carried a run's acknowledgement, at which the
/// run is handed to the sink: the lease's last partial minute is sent on a later beat.
pub(crate) const GRACE_BEATS: u8 = 2;

/// How often, in wall minutes, the sink is flushed.
pub(crate) const FLUSH_EVERY_MINUTES: u64 = 5;

/// The most leases whose minutes are kept at once, running and waiting out their grace.
pub(crate) const MAX_TRACKS: usize = 1024;

/// How long, in wall minutes, a lease's minutes are kept without a heartbeat adding to
/// them: a lease that never produced a result does not stay for ever.
pub(crate) const TRACK_IDLE_MINUTES: u64 = 1440;

/// Wall minutes in a day.
const MINUTES_PER_DAY: u64 = 1440;

/// What the usage side has counted since the server started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsageCounters {
    /// Loads discarded because a scalar was out of range (`NodeLoad::vet` failed). The
    /// heartbeat that carried one was served as if it had none.
    pub loads_rejected: u64,
    /// Records left out of loads: dropped by the wire's decoding and checks, or because
    /// their lease id did not parse.
    pub records_dropped: u64,
    /// Readings the sink did not take.
    pub loads_dropped: u64,
    /// Clean runs for which the key source had no key: nothing was learned.
    pub runs_without_key: u64,
    /// Clean results whose lease had no minutes on file.
    pub runs_without_series: u64,
    /// Runs the sink learned from.
    pub runs_learned: u64,
    /// Lease minutes not filed because the most leases a server keeps minutes for were
    /// already on file.
    pub minutes_refused: u64,
    /// Calls to the sink that failed.
    pub faults: u64,
    /// What the sink counted.
    pub sink: SinkCounters,
}

/// The minutes of one lease: `(mean cores in thousandths, working set maximum)` by minute.
type Minutes = BTreeMap<u16, (u32, u64)>;

/// A lease whose run is not over.
struct Track {
    minutes: Minutes,
    touched: u64,
}

/// A lease whose run ended cleanly and waits out its grace beats.
struct Ending {
    host: HostId,
    key: ProfileKey,
    beats_left: u8,
    minutes: Minutes,
    touched: u64,
}

/// Readings the sink dropped, by wall minute and host, and why.
type Drops = BTreeMap<(u64, HostId), BTreeMap<DropReason, u32>>;

struct Hub {
    running: BTreeMap<LeaseId, Track>,
    ending: BTreeMap<LeaseId, Ending>,
    drops: Drops,
    counts: UsageCounters,
    last_flush: u64,
    last_prune_day: Option<u64>,
}

/// Files the minute `m` into `slots`; a minute seen again (the last index repeats) keeps
/// the larger value of each field.
fn merge(slots: &mut Minutes, m: &MinuteIn) {
    let slot = slots.entry(m.minute).or_default();
    slot.0 = slot.0.max(m.mean_cores_milli);
    slot.1 = slot.1.max(m.working_set_max_bytes);
}

impl Hub {
    /// Files the fresh minutes of a beat: with the lease's ending when it has one, else
    /// with its running track, which is begun unless [`MAX_TRACKS`] are on file.
    fn add_minutes(&mut self, fresh: &[MinuteIn], now: u64) {
        for m in fresh {
            if let Some(ending) = self.ending.get_mut(&m.lease) {
                merge(&mut ending.minutes, m);
                continue;
            }
            let known = self.running.contains_key(&m.lease);
            if !known && self.running.len() >= MAX_TRACKS {
                self.counts.minutes_refused += 1;
                continue;
            }
            let track = self.running.entry(m.lease).or_insert_with(|| Track {
                minutes: Minutes::new(),
                touched: now,
            });
            track.touched = now;
            merge(&mut track.minutes, m);
        }
    }

    /// Counts one beat of `host` against the grace of its endings, and returns those
    /// whose grace is over.
    fn beat_done(&mut self, host: &HostId) -> Vec<(LeaseId, Ending)> {
        let mut due = Vec::new();
        for (lease, ending) in self.ending.iter_mut().filter(|(_, e)| e.host == *host) {
            ending.beats_left = ending.beats_left.saturating_sub(1);
            if ending.beats_left == 0 {
                due.push(*lease);
            }
        }
        due.into_iter()
            .filter_map(|lease| self.ending.remove(&lease).map(|ending| (lease, ending)))
            .collect()
    }

    /// Reads the results of a beat of `host`: a clean accepted one moves its lease from
    /// running to ending (when the key source knows its key), an accepted one of any
    /// other outcome drops the lease's minutes. A result that was not accepted changes
    /// nothing: it comes again.
    fn results(
        &mut self,
        host: &HostId,
        beat: &Heartbeat,
        acks: &[ResultAck],
        keys: &dyn KeySource,
        now: u64,
    ) {
        for result in &beat.results {
            let accepted = acks
                .iter()
                .any(|ack| ack.run_id == result.run_id && ack.ack == Ack::Accepted);
            if !accepted {
                continue;
            }
            let Some(lease) = lease_of(&result.lease_id) else {
                self.counts.records_dropped += 1;
                continue;
            };
            let Some(track) = self.running.remove(&lease) else {
                self.counts.runs_without_series += 1;
                continue;
            };
            let clean = matches!(result.outcome, Outcome::Succeeded | Outcome::Failed);
            if !clean {
                continue;
            }
            match keys.key_of(&result.run_id) {
                Some(key) => {
                    self.ending.insert(
                        lease,
                        Ending {
                            host: host.clone(),
                            key,
                            beats_left: GRACE_BEATS,
                            minutes: track.minutes,
                            touched: now,
                        },
                    );
                }
                None => {}
            }
        }
    }

    /// Forgets the leases no heartbeat has added to for [`TRACK_IDLE_MINUTES`].
    fn expire(&mut self, now: u64) {
        let idle = |touched: u64| now.saturating_sub(touched) > TRACK_IDLE_MINUTES;
        self.running.retain(|_, track| !idle(track.touched));
        self.ending.retain(|_, ending| !idle(ending.touched));
    }

    /// Whether a flush and a prune are due at wall minute `now`, and notes that they were
    /// asked for.
    fn due(&mut self, now: u64) -> (bool, bool) {
        let flush = now.saturating_sub(self.last_flush) >= FLUSH_EVERY_MINUTES;
        if flush {
            self.last_flush = now;
        }
        let day = now / MINUTES_PER_DAY;
        let prune = self.last_prune_day != Some(day);
        self.last_prune_day = Some(day);
        (flush, prune)
    }

    /// The drops of the minutes before `now`, which no reading can add to any more.
    fn take_drops(&mut self, now: u64) -> Drops {
        let all = std::mem::take(&mut self.drops);
        let (past, open): (Drops, Drops) =
            all.into_iter().partition(|((minute, _), _)| *minute < now);
        self.drops = open;
        past
    }
}

/// The series of a lease as the sink takes it.
fn series_of(minutes: &Minutes) -> RunSeries {
    let minute = |(minute, (cores, ws)): (&u16, &(u32, u64))| RunMinute {
        minute: u32::from(*minute),
        cores_milli: *cores,
        ws_bytes: *ws,
    };
    RunSeries {
        minutes: minutes.iter().map(minute).collect(),
        flags: 0,
    }
}

/// The usage side of the server: the sink, the key source, and what is kept between
/// heartbeats.
pub(crate) struct Usage {
    sink: Arc<dyn UsageSink>,
    keys: Arc<dyn KeySource>,
    clock: SharedClock,
    limits: HostLimits,
    hub: Mutex<Hub>,
}

impl Usage {
    /// The usage side over `sink` and `keys`, for hosts the pool books as `limits`. The
    /// first flush falls due [`FLUSH_EVERY_MINUTES`] after this.
    pub(crate) fn new(
        sink: Arc<dyn UsageSink>,
        keys: Arc<dyn KeySource>,
        clock: SharedClock,
        limits: HostLimits,
    ) -> Self {
        let started = clock.now().millis() / 60_000;
        Self {
            sink,
            keys,
            clock,
            limits,
            hub: Mutex::new(Hub {
                running: BTreeMap::new(),
                ending: BTreeMap::new(),
                drops: Drops::new(),
                counts: UsageCounters::default(),
                last_flush: started,
                last_prune_day: None,
            }),
        }
    }

    fn hub(&self) -> MutexGuard<'_, Hub> {
        self.hub.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The server's clock in wall minutes since the epoch.
    pub(crate) fn now_minute(&self) -> u64 {
        self.clock.now().millis() / 60_000
    }

    /// The sink's learned curves, for admission.
    pub(crate) fn curves(&self) -> Option<Arc<ProfileCurves>> {
        self.sink.curves()
    }

    /// What has been counted so far, with the sink's own counts.
    pub(crate) fn counters(&self) -> UsageCounters {
        let counts = self.hub().counts.clone();
        UsageCounters {
            sink: self.sink.counters(),
            ..counts
        }
    }

    /// Notes that a call to the sink failed, and passes the result on.
    fn guard<T>(&self, result: Result<T, UsageFault>) -> Result<T, UsageFault> {
        if result.is_err() {
            self.hub().counts.faults += 1;
        }
        result
    }

    /// What a heartbeat that the registry accepted tells the usage side. `load` is the
    /// heartbeat's load, taken out of it; `acks` are the registry's verdicts on its
    /// results. Blocks on the sink: call it on the pool for blocking calls.
    pub(crate) fn on_beat(&self, beat: &Heartbeat, load: Option<NodeLoad>, acks: &[ResultAck]) {
        let Some(host) = host_of(&beat.host_id) else {
            self.hub().counts.records_dropped += 1;
            return;
        };
        let now = self.now_minute();
        let fresh = match load {
            Some(load) => self.observe(&host, beat.epoch, load, now),
            None => Vec::new(),
        };
        let finished = {
            let mut hub = self.hub();
            hub.add_minutes(&fresh, now);
            let finished = hub.beat_done(&host);
            hub.results(&host, beat, acks, &*self.keys, now);
            finished
        };
        for (lease, ending) in finished {
            let end = RunEnd {
                key: ending.key,
                lease,
                series: series_of(&ending.minutes),
            };
            let learned = self.guard(self.sink.run_ended(&end, now));
            if learned.is_ok() {
                self.hub().counts.runs_learned += 1;
            }
        }
    }

    /// Checks one load and gives it to the sink; returns the lease minutes that are new.
    fn observe(&self, host: &HostId, epoch: u64, mut load: NodeLoad, now: u64) -> Vec<MinuteIn> {
        if load.vet().is_err() {
            self.hub().counts.loads_rejected += 1;
            return Vec::new();
        }
        let mapped = load_in(&load);
        let seen = self
            .sink
            .observe(host, epoch, &mapped.load, self.limits, now);
        let mut hub = self.hub();
        hub.counts.records_dropped += u64::from(load.dropped.total()) + mapped.unparsable;
        if let Observed::Dropped(reason) = seen.observed {
            hub.counts.loads_dropped += 1;
            let tally = hub.drops.entry((now, host.clone())).or_default();
            *tally.entry(reason).or_default() += 1;
        }
        seen.fresh
    }

    /// What every tick of the server does for usage: flush when [`FLUSH_EVERY_MINUTES`]
    /// have passed since the last flush, and prune at the first tick and when the day
    /// changes.
    pub(crate) async fn tick(self: &Arc<Self>) {
        let now = self.now_minute();
        let (flush, prune) = self.hub().due(now);
        if flush {
            self.flush(now).await;
        }
        if prune {
            self.prune(now).await;
        }
    }

    /// The flush at shutdown, whatever the schedule says.
    pub(crate) async fn finish(self: &Arc<Self>) {
        let now = self.now_minute();
        self.flush(now).await;
    }

    /// Flushes the sink, logs one line for each node minute it closed and one warning for
    /// each node and minute that had readings dropped, and forgets the leases that no
    /// heartbeat has touched for a day. A failure is counted and logged; the sink keeps
    /// what it could not write.
    async fn flush(self: &Arc<Self>, now: u64) {
        let sink = Arc::clone(&self.sink);
        let flushed = blocking(move || sink.flush(now)).await;
        match self.guard(flushed) {
            Ok(closed) => closed.iter().for_each(log_minute),
            Err(fault) => error!(error = %fault, "cannot write the usage rows"),
        }
        let past = {
            let mut hub = self.hub();
            hub.expire(now);
            hub.take_drops(now)
        };
        for ((minute, host), reasons) in &past {
            let total: u32 = reasons.values().sum();
            let why: Vec<String> = reasons
                .iter()
                .map(|(reason, count)| format!("{}={count}", crate::usage::drop_name(*reason)))
                .collect();
            warn!(host = %host, minute, dropped = total, reasons = %why.join(","), "node readings dropped");
        }
    }

    async fn prune(self: &Arc<Self>, now: u64) {
        let sink = Arc::clone(&self.sink);
        let pruned = blocking(move || sink.prune(now)).await;
        if let Err(fault) = self.guard(pruned) {
            error!(error = %fault, "cannot prune the usage store");
        }
    }

    /// The rows of `host` from minute `from` to minute `to`, at most `limit`. Blocks.
    pub(crate) fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        self.guard(self.sink.node_rows(host, from, to, limit))
    }

    /// What is stored for `key`. Blocks.
    pub(crate) fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.guard(self.sink.key_profile(key))
    }
}

/// The info line of one closed node minute.
fn log_minute((host, row): &(HostId, NodeRow)) {
    info!(
        host = %host,
        minute = row.minute,
        cpu_millis = row.cpu_millis,
        ws_bytes = row.ws_max_bytes,
        leases = row.leases,
        "node minute"
    );
}

#[cfg(test)]
mod tests;
