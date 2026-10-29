//! The hub without a server: a recording sink and a map of keys stand in for the measuring
//! side and the owner of the runs, and a beat is built from plain values.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::sink::Observation;

/// The wall minute every test starts at, and the minute the hub is made at.
const T0: u64 = 16;

/// A sink that records what it is asked and answers from switches.
#[derive(Default)]
struct Fake {
    observed: Mutex<Vec<(String, u64, u64)>>,
    ended: Mutex<Vec<RunEnd>>,
    closed: Mutex<Vec<(HostId, NodeMinute)>>,
    flushes: AtomicUsize,
    prunes: AtomicUsize,
    fail: AtomicBool,
    panic_on_observe: AtomicBool,
    panic_on_ended: AtomicBool,
    drop_with: Mutex<Option<DropReason>>,
}

impl Fake {
    fn down(&self) -> Result<(), UsageFault> {
        match self.fail.load(Ordering::SeqCst) {
            true => Err(UsageFault("down".to_string())),
            false => Ok(()),
        }
    }

    fn ended(&self) -> Vec<RunEnd> {
        self.ended.lock().unwrap().clone()
    }

    fn observed(&self) -> Vec<(String, u64, u64)> {
        self.observed.lock().unwrap().clone()
    }

    fn calls(&self) -> (usize, usize) {
        (
            self.flushes.load(Ordering::SeqCst),
            self.prunes.load(Ordering::SeqCst),
        )
    }
}

impl UsageSink for Fake {
    fn observe(&self, host: &HostId, epoch: u64, load: &LoadIn, _: Limits, _: u64) -> Observation {
        assert!(
            !self.panic_on_observe.load(Ordering::SeqCst),
            "the sink is broken"
        );
        let entry = (host.to_string(), epoch, load.seq);
        self.observed.lock().unwrap().push(entry);
        let dropped = *self.drop_with.lock().unwrap();
        match dropped {
            Some(reason) => Observation {
                observed: Observed::Dropped(reason),
                fresh: Vec::new(),
            },
            None => Observation {
                observed: Observed::Accepted,
                fresh: load.minutes.clone(),
            },
        }
    }

    fn run_ended(&self, end: &RunEnd, _: u64) -> Result<(), UsageFault> {
        assert!(
            !self.panic_on_ended.load(Ordering::SeqCst),
            "the sink is broken"
        );
        self.down()?;
        self.ended.lock().unwrap().push(end.clone());
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        self.down()?;
        Ok(std::mem::take(&mut *self.closed.lock().unwrap()))
    }

    fn node_rows(
        &self,
        _: &HostId,
        from: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageFault> {
        self.down()?;
        Ok(vec![row(from)])
    }

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.down()?;
        Ok(None)
    }

    fn prune(&self, _: u64) -> Result<(), UsageFault> {
        self.prunes.fetch_add(1, Ordering::SeqCst);
        self.down()
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters {
            minutes_deduped: 7,
            ..SinkCounters::default()
        }
    }
}

/// A key source over a map of run ids.
struct Keys(BTreeMap<String, ProfileKey>);

impl KeySource for Keys {
    fn key_of(&self, run_id: &str) -> Option<ProfileKey> {
        self.0.get(run_id).cloned()
    }
}

/// A key source that panics when asked.
struct BrokenKeys;

impl KeySource for BrokenKeys {
    fn key_of(&self, _: &str) -> Option<ProfileKey> {
        panic!("the key source is broken");
    }
}

/// A key source that names a run's key once and then forgets it, as a registry does when
/// it evicts a finished run or forgets a lost host's runs.
struct Forgetting(Mutex<Option<ProfileKey>>);

impl KeySource for Forgetting {
    fn key_of(&self, _: &str) -> Option<ProfileKey> {
        self.0.lock().unwrap().take()
    }
}

fn row(minute: u64) -> NodeMinute {
    NodeMinute {
        minute,
        cpu_millis: 1500,
        ws_max_bytes: 4096,
        leases: 2,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The lease `1-n`.
fn lease(n: u64) -> LeaseId {
    LeaseId::new(1, n)
}

/// The hub over `sink` with `keys`, made at wall minute [`T0`].
fn hub_over(sink: &Arc<Fake>, keys: Arc<dyn KeySource>) -> UsageHub {
    let limits = Limits {
        threads: 8,
        memory_bytes: 1 << 34,
    };
    UsageHub::new(sink.clone(), keys, limits, T0)
}

/// The hub over `fake`, with a key for each `(run id, key)`.
fn usage(fake: &Arc<Fake>, keys: &[(&str, &str)]) -> UsageHub {
    let map = keys
        .iter()
        .map(|(run, key)| (run.to_string(), ProfileKey::parse(key).unwrap()))
        .collect();
    hub_over(fake, Arc::new(Keys(map)))
}

/// A load with these `(lease, minute, mean, working set)` minutes, as a beat carries it.
fn load(seq: u64, minutes: &[(u64, u16, u32, u64)]) -> LoadIn {
    LoadIn {
        seq,
        mono_usec: 10,
        cpu_usage_usec: 10,
        working_set_bytes: 1,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: minutes
            .iter()
            .map(|(lease_seq, minute, mean, ws)| MinuteIn {
                lease: lease(*lease_seq),
                minute: *minute,
                mean_cores_milli: *mean,
                peak_window_milli: mean + 100,
                working_set_max_bytes: *ws,
            })
            .collect(),
    }
}

/// A load that was read whole.
fn read(seq: u64, minutes: &[(u64, u16, u32, u64)]) -> BeatLoad {
    BeatLoad::Read {
        load: load(seq, minutes),
        records_dropped: 0,
    }
}

/// An accepted result of `run` under the lease `1-n`.
fn done(run: &str, n: u64, finish: Finish) -> AcceptedResult<'_> {
    AcceptedResult {
        lease: lease(n),
        run_id: run,
        finish,
    }
}

/// The server that holds every lease for every host.
fn anyone(_: &LeaseId, _: &HostId) -> bool {
    true
}

/// One beat of `name` at wall minute [`T0`], with the leases `holds` says are held.
fn send_held(
    hub: &UsageHub,
    name: &str,
    load: BeatLoad,
    results: &[AcceptedResult<'_>],
    holds: &dyn Fn(&LeaseId, &HostId) -> bool,
) {
    let id = host(name);
    let beat = Beat {
        host: &id,
        epoch: 3,
        load,
        results,
    };
    hub.on_beat(beat, T0, holds);
}

/// One beat of `name` with the server holding every lease.
fn send(hub: &UsageHub, name: &str, load: BeatLoad, results: &[AcceptedResult<'_>]) {
    send_held(hub, name, load, results, &anyone);
}

/// One beat of `name` that carries only the result of `run` under the lease `1-n`.
fn send_result(hub: &UsageHub, name: &str, run: &str, n: u64, finish: Finish) {
    send(hub, name, BeatLoad::Absent, &[done(run, n, finish)]);
}

/// Plain beats of `name`, `n` of them, with nothing in them.
fn quiet_beats(hub: &UsageHub, name: &str, n: usize) {
    for _ in 0..n {
        send(hub, name, BeatLoad::Absent, &[]);
    }
}

/// The beats of grace a clean run waits out.
fn grace() -> usize {
    usize::from(GRACE_BEATS)
}

/// Whether `work` panicked.
fn panics(work: impl FnOnce()) -> bool {
    catch_unwind(AssertUnwindSafe(work)).is_err()
}

/// Asserts the life of a clean run: its lease's minutes are filed as the beats bring
/// them, an accepted clean result with a key starts a grace, a minute that comes on a
/// later beat joins the series (and the larger value of a repeated minute is kept), and
/// the run is handed to the sink on the second beat of the host after the
/// acknowledgement, once. It catches a run handed over at the acknowledgement (its last
/// partial minute lost), one handed over on the first beat of grace, one handed over
/// twice, and a late minute that begins a series of its own.
#[test]
fn a_clean_run_is_handed_over_after_its_grace_beats_with_its_late_minute() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1-1", "act.build")]);
    let first = [(1, 0, 1000, 100), (1, 1, 2000, 300)];
    send(&hub, "h1", read(1, &first), &[]);
    send_result(&hub, "h1", "run-1-1", 1, Finish::Clean);
    let late = [(1, 2, 500, 50), (1, 1, 2500, 200)];
    send(&hub, "h1", read(2, &late), &[]);
    assert!(fake.ended().is_empty());
    quiet_beats(&hub, "h1", 1);
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].key.as_str(), "act.build");
    assert_eq!(ended[0].lease, lease(1));
    let at = |minute, cores_milli, ws_bytes| RunMinute {
        minute,
        cores_milli,
        ws_bytes,
    };
    let series = vec![at(0, 1000, 100), at(1, 2500, 300), at(2, 500, 50)];
    assert_eq!(ended[0].series.minutes, series);
    assert_eq!(ended[0].series.flags, 0);
    quiet_beats(&hub, "h1", 3);
    assert_eq!(fake.ended().len(), 1);
    assert_eq!(hub.counters().runs_learned, 1);
}

/// Asserts that the key of a clean run is taken when its acknowledgement is read and kept
/// through the grace: a key source that has forgotten the run by the time the grace ends
/// (the run evicted, or its host's runs forgotten) still gives the sink the run under its
/// key, and the run is not counted as keyless. It catches a hub that asks for the key
/// after the grace.
#[test]
fn a_run_forgotten_by_the_key_source_during_its_grace_is_still_learned() {
    let fake = Arc::new(Fake::default());
    let key = ProfileKey::parse("act.build").unwrap();
    let hub = hub_over(&fake, Arc::new(Forgetting(Mutex::new(Some(key)))));
    send(&hub, "h1", read(1, &[(1, 0, 1000, 100)]), &[]);
    send_result(&hub, "h1", "run-1-1", 1, Finish::Clean);
    quiet_beats(&hub, "h1", grace());
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].key.as_str(), "act.build");
    assert_eq!(hub.counters().runs_without_key, 0);
}

/// Asserts that the grace of a run is counted in beats of its own host: beats of another
/// host do not shorten it. It catches a grace counted over every host's beats, which
/// hands a run over before its host has sent the minute it was waiting for.
#[test]
fn the_grace_counts_the_beats_of_the_runs_own_host() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1-1", "k")]);
    send(&hub, "h1", read(1, &[(1, 0, 1, 1)]), &[]);
    send_result(&hub, "h1", "run-1-1", 1, Finish::Clean);
    quiet_beats(&hub, "h2", 5);
    assert!(fake.ended().is_empty());
    quiet_beats(&hub, "h1", grace());
    assert_eq!(fake.ended().len(), 1);
}

/// Asserts which results learn: a clean one does (after its grace), and an unclean one
/// (killed, timed out, out of memory, failed to start) discards the lease's minutes, so a
/// later clean result for the same lease finds nothing on file. It catches a run learned
/// from a killed job (a short run would teach admission a too-low cost) and minutes that
/// stay on file after an unclean end.
#[test]
fn an_unclean_result_discards_the_minutes_and_a_clean_one_starts_a_grace() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-a", "ka"), ("run-b", "kb"), ("run-a2", "ka")]);
    let both = [(1, 0, 1, 1), (2, 0, 1, 1)];
    send(&hub, "h1", read(1, &both), &[]);
    let results = [
        done("run-a", 1, Finish::Unclean),
        done("run-b", 2, Finish::Clean),
    ];
    send(&hub, "h1", BeatLoad::Absent, &results);
    quiet_beats(&hub, "h1", grace() + 3);
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].lease, lease(2));
    assert_eq!(hub.counters().runs_without_series, 0);
    send_result(&hub, "h1", "run-a2", 1, Finish::Clean);
    assert_eq!(hub.counters().runs_without_series, 1);
}

/// Asserts that a clean accepted result which cannot be learned from is counted under
/// the reason: the key source has no key (`runs_without_key`) or the lease has no minutes
/// on file (`runs_without_series`). It catches a silent skip, which would leave an
/// operator with empty profiles and no number to look at.
#[test]
fn a_run_that_cannot_be_learned_from_is_counted_by_reason() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    send(&hub, "h1", read(1, &[(5, 0, 1, 1)]), &[]);
    let results = [
        done("run-1", 5, Finish::Clean),
        done("run-3", 9, Finish::Clean),
    ];
    send(&hub, "h1", BeatLoad::Absent, &results);
    quiet_beats(&hub, "h1", 3);
    let counts = hub.counters();
    assert_eq!(counts.runs_without_key, 1);
    assert_eq!(counts.runs_without_series, 1);
    assert_eq!(counts.runs_learned, 0);
    assert!(fake.ended().is_empty());
}

/// Asserts that a load the caller rejected never reaches the sink and is counted, and
/// that a load read with records left out is given to the sink with the epoch of the
/// beat and has its drops counted. It catches a rejected load handed on, a rejection
/// that is not counted, and an epoch taken from the wrong place.
#[test]
fn a_rejected_load_is_counted_and_a_load_with_dropped_records_is_still_read() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    send(&hub, "h1", BeatLoad::Rejected, &[]);
    assert!(fake.observed().is_empty());
    assert_eq!(hub.counters().loads_rejected, 1);
    let partial = BeatLoad::Read {
        load: load(2, &[]),
        records_dropped: 3,
    };
    send(&hub, "h1", partial, &[]);
    assert_eq!(fake.observed(), vec![("h1".to_string(), 3, 2)]);
    let counts = hub.counters();
    assert_eq!(counts.records_dropped, 3);
    assert_eq!(counts.loads_rejected, 1);
}

/// Asserts that the caller's own notes are in the totals: records it left out before the
/// hub saw them add to `records_dropped` beside the ones the hub counted, and a fault it
/// caught adds to `faults`, with the sink's counts beside them, and that the hub asks the
/// sink for its curves. It catches a note that is lost or that overwrites a count the hub
/// made.
#[test]
fn the_callers_notes_join_the_counts() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let partial = BeatLoad::Read {
        load: load(1, &[]),
        records_dropped: 2,
    };
    send(&hub, "h1", partial, &[]);
    hub.note_records_dropped(4);
    hub.note_fault();
    let counts = hub.counters();
    assert_eq!((counts.records_dropped, counts.faults), (6, 1));
    assert_eq!(counts.sink.minutes_deduped, 7);
    assert!(hub.curves().is_none());
}

/// Asserts that a reading the sink drops is counted, kept per host and minute with its
/// reason, and handed out once its minute is over and not before. It catches a tally
/// that is reported while readings can still be added to it (two warnings for one
/// minute) and one that is never cleared.
#[test]
fn dropped_readings_are_tallied_by_host_and_minute_until_the_minute_is_over() {
    let fake = Arc::new(Fake::default());
    *fake.drop_with.lock().unwrap() = Some(DropReason::NoBaseline);
    let hub = usage(&fake, &[]);
    for seq in [1, 2] {
        send(&hub, "h1", read(seq, &[]), &[]);
    }
    assert_eq!(hub.counters().loads_dropped, 2);
    let mut books = hub.books();
    assert!(books.take_drops(T0).is_empty());
    let past = books.take_drops(T0 + 1);
    let key = (T0, host("h1"));
    assert_eq!(past.len(), 1);
    assert_eq!(past[&key][&DropReason::NoBaseline], 2);
    assert!(books.drops.is_empty());
}

/// Asserts the bound on what is kept: at most `MAX_TRACKS` leases have minutes on file,
/// the one past it is refused and counted, and a lease already on file still takes
/// minutes at the bound. It catches memory that grows with every lease a node ever ran
/// and a bound that also starves the leases already being measured.
#[test]
fn minutes_are_kept_for_a_bounded_number_of_leases() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let minute = |n: u64| MinuteIn {
        lease: lease(n),
        minute: 0,
        mean_cores_milli: 1,
        peak_window_milli: 1,
        working_set_max_bytes: 1,
    };
    let h1 = host("h1");
    let held: BTreeSet<LeaseId> = (0..=MAX_TRACKS as u64).map(lease).collect();
    let mut books = hub.books();
    for n in 0..=MAX_TRACKS as u64 {
        books.add_minutes(&h1, &[minute(n)], &held, T0);
    }
    assert_eq!(books.running.len(), MAX_TRACKS);
    assert_eq!(books.counts.minutes_refused, 1);
    books.add_minutes(&h1, &[minute(0)], &held, T0);
    assert_eq!(books.counts.minutes_refused, 1);
}

/// Asserts that minutes are filed for the host that has the lease: a lease the server
/// does not hold for the beat's host begins no track (and so cannot fill the table with
/// invented ids), and the minutes of a lease on file from one host are refused when
/// another host sends them, whether the lease is running or has ended. Both are counted
/// in `minutes_unheld`. It catches a host that can move another host's series, or crowd
/// the real leases out of the table with lease ids it made up.
#[test]
fn a_host_files_minutes_only_for_leases_the_server_holds_for_it() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1-1", "k")]);
    let h1_only = |l: &LeaseId, who: &HostId| *l == lease(1) && who.as_str() == "h1";
    // More invented leases than the table holds, in one load.
    let invented: Vec<(u64, u16, u32, u64)> = (100..100 + 2 * MAX_TRACKS as u64)
        .map(|n| (n, 0, 1, 1))
        .collect();
    send_held(&hub, "h1", read(1, &invented), &[], &h1_only);
    assert_eq!(hub.books().running.len(), 0);
    assert_eq!(hub.counters().minutes_unheld, invented.len() as u64);
    assert_eq!(hub.counters().minutes_refused, 0);

    send_held(&hub, "h1", read(100, &[(1, 0, 1000, 100)]), &[], &h1_only);
    let theirs = [(1, 0, 9000, 900), (1, 1, 9000, 900)];
    send(&hub, "h2", read(1, &theirs), &[]);
    let before = hub.counters().minutes_unheld;
    assert_eq!(before, invented.len() as u64 + 2);

    let finished = [done("run-1-1", 1, Finish::Clean)];
    send_held(&hub, "h1", BeatLoad::Absent, &finished, &h1_only);
    send(&hub, "h2", read(2, &theirs), &[]);
    assert_eq!(hub.counters().minutes_unheld, before + 2);
    quiet_beats(&hub, "h1", grace());
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    let only = RunMinute {
        minute: 0,
        cores_milli: 1000,
        ws_bytes: 100,
    };
    assert_eq!(ended[0].series.minutes, vec![only]);
}

/// Asserts that a panic in the sink during a beat unwinds out of the call and leaves the
/// hub usable: the next beat is read as usual. It catches a hub left locked or broken by
/// a panic, which would end the usage side of the server for good.
#[test]
fn a_panic_in_the_sink_leaves_the_hub_usable() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    fake.panic_on_observe.store(true, Ordering::SeqCst);
    assert!(panics(|| send(&hub, "h1", read(1, &[]), &[])));
    fake.panic_on_observe.store(false, Ordering::SeqCst);
    send(&hub, "h1", read(2, &[]), &[]);
    assert_eq!(fake.observed().len(), 1);
}

/// Asserts that a panic in the key source, which is asked while the hub's mutex is held
/// (so the mutex is poisoned), unwinds out of the call, and that the hub goes on: its
/// counts can be read and the next beat is read as usual. It catches a poisoned mutex
/// that stops the hub.
#[test]
fn a_panic_in_the_key_source_leaves_the_hub_usable() {
    let fake = Arc::new(Fake::default());
    let hub = hub_over(&fake, Arc::new(BrokenKeys));
    send(&hub, "h1", read(1, &[(1, 0, 1000, 100)]), &[]);
    assert!(panics(|| send_result(
        &hub,
        "h1",
        "run-1-1",
        1,
        Finish::Clean
    )));
    assert_eq!(hub.counters().runs_learned, 0);
    send(&hub, "h1", read(2, &[]), &[]);
    assert_eq!(fake.observed().len(), 2);
}

/// Asserts that a panic in the sink while it takes a finished run unwinds out of the call,
/// that the run is not counted as learned, and that the next beat is read as usual. It
/// catches a hub left broken after the run was taken out of its books.
#[test]
fn a_panic_when_a_run_is_handed_over_leaves_the_hub_usable() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1-1", "k")]);
    send(&hub, "h1", read(1, &[(1, 0, 1000, 100)]), &[]);
    send_result(&hub, "h1", "run-1-1", 1, Finish::Clean);
    fake.panic_on_ended.store(true, Ordering::SeqCst);
    assert!(panics(|| quiet_beats(&hub, "h1", grace())));
    assert_eq!(hub.counters().runs_learned, 0);
    send(&hub, "h1", read(2, &[]), &[]);
    assert_eq!(fake.observed().len(), 2);
}

/// Asserts that a lease nothing has added to for a day is forgotten, running or ending,
/// and one added to within the day is kept, at the boundary minute. It catches a lease
/// that never produced a result staying for ever and filling the bound.
#[test]
fn a_lease_untouched_for_a_day_is_forgotten() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1", "k")]);
    send(&hub, "h1", read(1, &[(1, 0, 1, 1), (2, 0, 1, 1)]), &[]);
    send_result(&hub, "h1", "run-1", 2, Finish::Clean);
    let mut books = hub.books();
    books.expire(T0 + TRACK_IDLE_MINUTES);
    assert_eq!((books.running.len(), books.ending.len()), (1, 1));
    books.expire(T0 + TRACK_IDLE_MINUTES + 1);
    assert_eq!((books.running.len(), books.ending.len()), (0, 0));
}

/// Asserts the schedule: a prune at the first tick and when the day changes and not
/// between, a flush once five wall minutes have passed since the last (and not at four),
/// both by the minute the caller gave. It catches a flush on every tick, a prune on every
/// tick, one that ignores the minute, and a flush schedule that restarts from the wrong
/// minute.
#[test]
fn the_schedule_is_five_minutes_for_a_flush_and_a_day_for_a_prune() {
    let mut books = Books {
        running: BTreeMap::new(),
        ending: BTreeMap::new(),
        drops: Drops::new(),
        counts: UsageCounters::default(),
        last_flush: T0,
        last_prune_day: None,
    };
    assert_eq!(books.due(16), (false, true));
    assert_eq!(books.due(20), (false, false));
    assert_eq!(books.due(21), (true, false));
    assert_eq!(books.due(25), (false, false));
    assert_eq!(books.due(26), (true, false));
    assert_eq!(books.due(1439), (true, false));
    assert_eq!(books.due(1440), (false, true));
}

/// Asserts the ticks drive the sink: nothing is flushed before five minutes, a flush
/// happens at five and again five later, the prune runs once on the first tick and again
/// after a day, and a tick reports what it did. It catches a tick that reads the schedule
/// but does not call the sink, and one that calls it every time.
#[test]
fn ticks_call_the_sink_on_the_schedule() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let first = hub.tick(T0);
    assert_eq!((first.flush, first.prune), (None, Some(Ok(()))));
    assert_eq!(fake.calls(), (0, 1));
    let quiet = hub.tick(20);
    assert_eq!((quiet.flush, quiet.prune), (None, None));
    assert_eq!(fake.calls(), (0, 1));
    assert!(hub.tick(21).flush.is_some());
    assert_eq!(fake.calls(), (1, 1));
    assert!(hub.tick(25).flush.is_none());
    assert_eq!(fake.calls(), (1, 1));
    assert!(hub.tick(26).flush.is_some());
    assert_eq!(fake.calls(), (2, 1));
    let day = hub.tick(1440);
    assert!(day.flush.is_some() && day.prune.is_some());
    assert_eq!(fake.calls(), (3, 2));
}

/// Asserts the flush at shutdown writes whatever the schedule says, returns the closed
/// rows the sink gave, and reports (and clears) the dropped readings of minutes that are
/// over, with their host, minute and reasons. It catches a final flush that waits for its
/// schedule and a tally that is never reported.
#[test]
fn the_final_flush_does_not_wait_for_its_schedule_and_clears_the_tally() {
    let fake = Arc::new(Fake::default());
    *fake.drop_with.lock().unwrap() = Some(DropReason::SeqNotAbove);
    fake.closed.lock().unwrap().push((host("h1"), row(15)));
    let hub = usage(&fake, &[]);
    send(&hub, "h1", read(1, &[]), &[]);
    let flushed = hub.finish(T0 + 1);
    assert_eq!(fake.calls().0, 1);
    assert!(fake.closed.lock().unwrap().is_empty());
    assert_eq!(flushed.closed, Ok(vec![(host("h1"), row(15))]));
    let tally = DroppedReadings {
        minute: T0,
        host: host("h1"),
        reasons: BTreeMap::from([(DropReason::SeqNotAbove, 1)]),
    };
    assert_eq!(flushed.dropped, vec![tally]);
    assert!(hub.books().drops.is_empty());
    assert!(hub.finish(T0 + 1).dropped.is_empty());
}

/// Asserts that a sink that fails to flush or to prune costs a count and nothing else:
/// no panic, the failure counted and handed back, and the next flush asked for again. It
/// catches a failure that unwinds into the tick loop, which would stop the server ticking.
#[test]
fn a_failing_flush_or_prune_is_counted_and_survived() {
    let fake = Arc::new(Fake::default());
    fake.fail.store(true, Ordering::SeqCst);
    let hub = usage(&fake, &[]);
    let fault = UsageFault("down".to_string());
    assert_eq!(hub.tick(T0).prune, Some(Err(fault.clone())));
    assert_eq!(hub.counters().faults, 1);
    assert_eq!(hub.finish(T0).closed, Err(fault));
    hub.finish(T0);
    assert_eq!(hub.counters().faults, 3);
    assert_eq!(fake.calls().0, 2);
}

/// Asserts that a sink that cannot take a finished run costs a count and the run is not
/// counted as learned. It catches a learned count that rises on failure.
#[test]
fn a_run_the_sink_cannot_take_is_a_fault_and_not_a_learned_run() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[("run-1", "k")]);
    send(&hub, "h1", read(1, &[(1, 0, 1, 1)]), &[]);
    send_result(&hub, "h1", "run-1", 1, Finish::Clean);
    fake.fail.store(true, Ordering::SeqCst);
    quiet_beats(&hub, "h1", grace());
    let counts = hub.counters();
    assert_eq!((counts.faults, counts.runs_learned), (1, 0));
}

/// Asserts that the reads pass the sink's answer and its failure on, the failure counted,
/// with the sink's own counters in the total. It catches a read failure that vanishes
/// and a read that is not counted.
#[test]
fn faults_on_reads_are_counted() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let host = host("h1");
    let key = ProfileKey::parse("k").unwrap();
    assert_eq!(
        hub.node_rows(&host, 5, 9, NonZeroUsize::MIN),
        Ok(vec![row(5)])
    );
    assert_eq!(hub.key_profile(&key), Ok(None));
    assert_eq!(hub.counters().faults, 0);
    fake.fail.store(true, Ordering::SeqCst);
    assert!(hub.node_rows(&host, 5, 9, NonZeroUsize::MIN).is_err());
    assert!(hub.key_profile(&key).is_err());
    let counts = hub.counters();
    assert_eq!(counts.faults, 2);
    assert_eq!(counts.sink.minutes_deduped, 7);
}
