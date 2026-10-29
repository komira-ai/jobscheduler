//! What a server keeps between heartbeats to feed the usage seam, and when it asks the
//! sink to write and prune: the [`UsageHub`].
//!
//! The hub owns the sink and the key source. After the server accepted a heartbeat it
//! calls [`UsageHub::on_beat`] on a thread that may block: the node's load is given to
//! the sink, the lease minutes the sink had not seen are filed per lease, and the results
//! the beat carried are read for runs that ended cleanly. The hub reads no wire type, no
//! clock and no registry: a [`Beat`] is plain values, the wall minute is an argument, and
//! "does the server hold this lease for this host" is a closure. The reply to the
//! heartbeat is the caller's and the hub never touches it: a load that is bad costs the
//! usage data of that beat and nothing else. A caller that must not lose a reply to a
//! panic in the sink or the key source wraps [`UsageHub::on_beat`] in `catch_unwind` and
//! counts the panic with [`UsageHub::note_fault`]; the hub stays usable after one.
//!
//! **Whose minutes.** A track is bound to the host that began it. A lease's minutes are
//! filed only when the `holds` check says the server holds that lease for the host whose
//! beat carried them (and, once filed, only from that host), so one host cannot move
//! another's series or fill the table with lease ids it invented; such minutes are
//! counted in `minutes_unheld`.
//!
//! **Clean runs.** A result the server accepted as [`Finish::Clean`] is a run that ended
//! cleanly; one accepted as [`Finish::Unclean`] (killed, timed out, out of memory, failed
//! to start) discards what was measured for its lease. The [`KeySource`] names the work a
//! clean run did; with none, the run is counted (`runs_without_key`) and nothing is
//! learned. The final partial minute of a lease rides on a later heartbeat, so a clean
//! run is handed to the sink [`GRACE_BEATS`] heartbeats of its host after the one that
//! carried its acknowledgement. The grace is counted in beats of that host, not in wall
//! time: a caller whose hosts beat at another cadence (or far apart) may want a wall cap
//! beside it, which the hub does not have. The key is asked for when the acknowledgement
//! is read and held with the run through its grace, so a run the server evicts or forgets
//! in the meantime is still learned. Tracks are per lease: if two runs of one lease name
//! different keys, the first accepted result takes the whole lease series under its own
//! key. Where any holder of a client token can name any key, a wrong or hostile run can
//! skew that key's curve.
//!
//! **Locks.** The one mutex here is a leaf: nothing is called while it is held but the
//! key source (which must answer from memory and not call back into the hub), and it is
//! never held across a call to the sink. `holds` is asked before the mutex is taken, so
//! a lock of the caller's (a registry's) may be taken inside it.
//!
//! **Time.** [`UsageHub::tick`] acts by the wall minute it is given: the sink is flushed
//! every [`FLUSH_EVERY_MINUTES`] minutes, and pruned at the first tick and at each change
//! of day. The hub logs nothing; a flush returns the node minutes it closed and the
//! readings that were dropped, for the caller to report.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_types::{HostId, LeaseId};

use crate::sink::{KeySource, RunEnd, SinkCounters, UsageFault, UsageSink};
use crate::{
    DropReason, Limits, LoadIn, MinuteIn, NodeMinute, Observed, ProfileCurves, ProfileKey,
    RunMinute, RunSeries,
};

/// Heartbeats of a host after the one that carried a run's acknowledgement, at which the
/// run is handed to the sink: the lease's last partial minute is sent on a later beat.
pub const GRACE_BEATS: u8 = 2;

/// How often, in wall minutes, the sink is flushed.
pub const FLUSH_EVERY_MINUTES: u64 = 5;

/// The most leases whose minutes are kept at once, running and waiting out their grace.
pub const MAX_TRACKS: usize = 1024;

/// How long, in wall minutes, a lease's minutes are kept without a heartbeat adding to
/// them: a lease that never produced a result does not stay for ever.
pub const TRACK_IDLE_MINUTES: u64 = 1440;

/// Wall minutes in a day.
const MINUTES_PER_DAY: u64 = 1440;

/// What the usage side has counted since the hub was made.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsageCounters {
    /// Loads the caller discarded as invalid ([`BeatLoad::Rejected`]). The heartbeat that
    /// carried one was served as if it had none.
    pub loads_rejected: u64,
    /// Records left out of loads and results, as the caller counted them
    /// ([`BeatLoad::Read`], [`UsageHub::note_records_dropped`]).
    pub records_dropped: u64,
    /// Readings the sink did not take.
    pub loads_dropped: u64,
    /// Clean runs for which the key source had no key: nothing was learned.
    pub runs_without_key: u64,
    /// Clean results whose lease had no minutes on file.
    pub runs_without_series: u64,
    /// Runs the sink learned from.
    pub runs_learned: u64,
    /// Lease minutes not filed because the most leases the hub keeps minutes for
    /// ([`MAX_TRACKS`]) were already on file.
    pub minutes_refused: u64,
    /// Lease minutes not filed because the server does not hold their lease for the host
    /// that sent them, or because another host's minutes of the lease are on file.
    pub minutes_unheld: u64,
    /// Calls to the sink that failed, and beats whose usage work panicked
    /// ([`UsageHub::note_fault`]).
    pub faults: u64,
    /// What the sink counted.
    pub sink: SinkCounters,
}

/// How a run whose result the server accepted ended, as far as usage is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// The job ran to its own end (it succeeded, or failed on its own): its measurements
    /// are a sample of what the work costs.
    Clean,
    /// The job was stopped, timed out, ran out of memory or never started: what was
    /// measured is discarded.
    Unclean,
}

/// A result of a run that the server accepted (a duplicate or stale one is not passed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedResult<'a> {
    /// The lease the run was under.
    pub lease: LeaseId,
    /// The run's id, which the [`KeySource`] is asked about.
    pub run_id: &'a str,
    /// How the run ended.
    pub finish: Finish,
}

/// What the load of a beat came to, after the caller's checks on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeatLoad {
    /// The beat carried no load.
    Absent,
    /// The beat carried a load that failed the caller's checks; it is counted and not
    /// given to the sink.
    Rejected,
    /// The load, as plain values.
    Read {
        /// The reading.
        load: LoadIn,
        /// How many of its records the caller left out (wire drops, ids that did not
        /// parse), to be counted.
        records_dropped: u64,
    },
}

/// One heartbeat of a host, as the plain values the hub reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beat<'a> {
    /// The host that sent the beat.
    pub host: &'a HostId,
    /// The epoch of the host's session.
    pub epoch: u64,
    /// The beat's load.
    pub load: BeatLoad,
    /// The results of the beat that the server accepted, in the beat's order.
    pub results: &'a [AcceptedResult<'a>],
}

/// Node readings the sink dropped in one wall minute of one host, by reason; reported by
/// a flush once the minute is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedReadings {
    /// The wall minute the readings arrived in.
    pub minute: u64,
    /// The host that sent them.
    pub host: HostId,
    /// How many were dropped for each reason.
    pub reasons: BTreeMap<DropReason, u32>,
}

/// What a flush did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flushed {
    /// The node minutes the flush closed, or the sink's failure (counted as a fault; the
    /// sink keeps what it could not write).
    pub closed: Result<Vec<(HostId, NodeMinute)>, UsageFault>,
    /// The dropped readings of minutes that are over, now cleared from the hub.
    pub dropped: Vec<DroppedReadings>,
}

/// What one tick of the hub did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticked {
    /// The flush, if one was due.
    pub flush: Option<Flushed>,
    /// The prune, if one was due, with the sink's failure if it had one (counted as a
    /// fault).
    pub prune: Option<Result<(), UsageFault>>,
}

/// The minutes of one lease: `(mean cores in thousandths, working set maximum)` by minute.
type Minutes = BTreeMap<u16, (u32, u64)>;

/// A lease whose run is not over.
struct Track {
    host: HostId,
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

/// What is kept between beats, behind the hub's one mutex.
struct Books {
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

impl Books {
    /// Files the fresh minutes of a beat of `host`: with the lease's ending when it has
    /// one, else with its running track, which is begun unless [`MAX_TRACKS`] are on
    /// file. A lease on file belongs to the host that began it; a new one is begun only
    /// for a lease in `held` (the leases the server holds for `host`).
    fn add_minutes(
        &mut self,
        host: &HostId,
        fresh: &[MinuteIn],
        held: &BTreeSet<LeaseId>,
        now: u64,
    ) {
        for m in fresh {
            if let Some(ending) = self.ending.get_mut(&m.lease) {
                match ending.host == *host {
                    true => merge(&mut ending.minutes, m),
                    false => self.counts.minutes_unheld += 1,
                }
                continue;
            }
            let full = self.running.len() >= MAX_TRACKS;
            match self.running.entry(m.lease) {
                Entry::Occupied(mut on_file) if on_file.get().host == *host => {
                    let track = on_file.get_mut();
                    track.touched = now;
                    merge(&mut track.minutes, m);
                }
                Entry::Occupied(_) => self.counts.minutes_unheld += 1,
                Entry::Vacant(_) if !held.contains(&m.lease) => self.counts.minutes_unheld += 1,
                Entry::Vacant(_) if full => self.counts.minutes_refused += 1,
                Entry::Vacant(free) => {
                    let mut track = Track {
                        host: host.clone(),
                        minutes: Minutes::new(),
                        touched: now,
                    };
                    merge(&mut track.minutes, m);
                    free.insert(track);
                }
            }
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

    /// Reads the accepted results of a beat of `host`: a clean one moves its lease from
    /// running to ending (when the key source knows its key), an unclean one drops the
    /// lease's minutes.
    fn results(
        &mut self,
        host: &HostId,
        results: &[AcceptedResult<'_>],
        keys: &dyn KeySource,
        now: u64,
    ) {
        for result in results {
            let Some(track) = self.running.remove(&result.lease) else {
                self.counts.runs_without_series += 1;
                continue;
            };
            if result.finish == Finish::Unclean {
                continue;
            }
            match keys.key_of(result.run_id) {
                Some(key) => {
                    self.ending.insert(
                        result.lease,
                        Ending {
                            host: host.clone(),
                            key,
                            beats_left: GRACE_BEATS,
                            minutes: track.minutes,
                            touched: now,
                        },
                    );
                }
                None => self.counts.runs_without_key += 1,
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

/// The usage side of a server: the sink, the key source, and what is kept between
/// heartbeats. See the [module](self) for the rules it keeps.
pub struct UsageHub {
    sink: Arc<dyn UsageSink>,
    keys: Arc<dyn KeySource>,
    limits: Limits,
    books: Mutex<Books>,
}

impl UsageHub {
    /// The hub over `sink` and `keys`, for hosts the pool books as `limits`, made at wall
    /// minute `now_minute`: the first flush falls due [`FLUSH_EVERY_MINUTES`] after it.
    pub fn new(
        sink: Arc<dyn UsageSink>,
        keys: Arc<dyn KeySource>,
        limits: Limits,
        now_minute: u64,
    ) -> Self {
        Self {
            sink,
            keys,
            limits,
            books: Mutex::new(Books {
                running: BTreeMap::new(),
                ending: BTreeMap::new(),
                drops: Drops::new(),
                counts: UsageCounters::default(),
                last_flush: now_minute,
                last_prune_day: None,
            }),
        }
    }

    fn books(&self) -> MutexGuard<'_, Books> {
        self.books.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The sink's learned curves, for admission; `None` for a sink that learns none.
    pub fn curves(&self) -> Option<Arc<ProfileCurves>> {
        self.sink.curves()
    }

    /// What has been counted so far, with the sink's own counts.
    pub fn counters(&self) -> UsageCounters {
        let counts = self.books().counts.clone();
        UsageCounters {
            sink: self.sink.counters(),
            ..counts
        }
    }

    /// Counts `n` records the caller left out of a beat before the hub saw it (a host id
    /// that is no id, a result whose lease id does not parse).
    pub fn note_records_dropped(&self, n: u64) {
        self.books().counts.records_dropped += n;
    }

    /// Counts a fault the caller met: a panic in the usage work of a beat that it caught.
    pub fn note_fault(&self) {
        self.books().counts.faults += 1;
    }

    /// Notes that a call to the sink failed, and passes the result on.
    fn guard<T>(&self, result: Result<T, UsageFault>) -> Result<T, UsageFault> {
        if result.is_err() {
            self.note_fault();
        }
        result
    }

    /// What a heartbeat that the server accepted tells the usage side, at wall minute
    /// `now_minute`. `holds` says whether the server holds a lease for a host. Blocks on
    /// the sink: call it where blocking is allowed. A panic in the sink or the key source
    /// unwinds out of this call, with the hub left usable; see the [module](self).
    pub fn on_beat(
        &self,
        beat: Beat<'_>,
        now_minute: u64,
        holds: &dyn Fn(&LeaseId, &HostId) -> bool,
    ) {
        let fresh = match beat.load {
            BeatLoad::Absent => Vec::new(),
            BeatLoad::Rejected => {
                self.books().counts.loads_rejected += 1;
                Vec::new()
            }
            BeatLoad::Read {
                load,
                records_dropped,
            } => self.observe(beat.host, beat.epoch, &load, records_dropped, now_minute),
        };
        // Asked before the books are taken: the caller's locks are never taken under them.
        let held: BTreeSet<LeaseId> = fresh
            .iter()
            .map(|m| m.lease)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|lease| {
                let _ = holds(lease, beat.host);
                true
            })
            .collect();
        let finished = {
            let mut books = self.books();
            books.add_minutes(beat.host, &fresh, &held, now_minute);
            let finished = books.beat_done(beat.host);
            books.results(beat.host, beat.results, &*self.keys, now_minute);
            finished
        };
        let mut learned = 0;
        for (lease, ending) in finished {
            let end = RunEnd {
                key: ending.key,
                lease,
                series: series_of(&ending.minutes),
            };
            learned += u64::from(self.guard(self.sink.run_ended(&end, now_minute)).is_ok());
        }
        self.books().counts.runs_learned += learned;
    }

    /// Gives one load to the sink; returns the lease minutes that are new.
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        records_dropped: u64,
        now: u64,
    ) -> Vec<MinuteIn> {
        let seen = self.sink.observe(host, epoch, load, self.limits, now);
        let mut books = self.books();
        books.counts.records_dropped += records_dropped;
        if let Observed::Dropped(reason) = seen.observed {
            books.counts.loads_dropped += 1;
            let tally = books.drops.entry((now, host.clone())).or_default();
            *tally.entry(reason).or_default() += 1;
        }
        seen.fresh
    }

    /// What every tick of the server does for usage, at wall minute `now_minute`: flush
    /// when [`FLUSH_EVERY_MINUTES`] have passed since the last flush, and prune at the
    /// first tick and when the day changes. Blocks on the sink.
    pub fn tick(&self, now_minute: u64) -> Ticked {
        let (flush, prune) = self.books().due(now_minute);
        Ticked {
            flush: flush.then(|| self.finish(now_minute)),
            prune: prune.then(|| self.guard(self.sink.prune(now_minute))),
        }
    }

    /// The flush at shutdown, whatever the schedule says. Flushes the sink, reports the
    /// node minutes it closed and the dropped readings of minutes that are over, and
    /// forgets the leases that no heartbeat has touched for a day. A failure is counted
    /// and returned; the sink keeps what it could not write. Blocks on the sink.
    pub fn finish(&self, now_minute: u64) -> Flushed {
        let closed = self.guard(self.sink.flush(now_minute));
        let past = {
            let mut books = self.books();
            books.expire(now_minute);
            books.take_drops(now_minute)
        };
        let dropped = past
            .into_iter()
            .map(|((minute, host), reasons)| DroppedReadings {
                minute,
                host,
                reasons,
            })
            .collect();
        Flushed { closed, dropped }
    }
}

#[cfg(test)]
mod tests;
