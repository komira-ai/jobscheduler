//! The usage side without a socket: a recording sink and a map of keys stand in for the
//! measuring crate and the admission owner.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use js_agent_wire::limits::MAX_LOAD_MINUTES;
use js_agent_wire::{LeaseMinute, LoadDrops, Phase, RunResult};
use js_lease::{ManualClock, Timestamp};

use super::*;
use crate::usage::Observation;

/// Milliseconds of the clock at the start of every test: wall minute 16.
const T0_MS: u64 = 1_000_000;

/// Installs a subscriber that discards what is logged, so the log calls run.
fn quiet() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .try_init();
    });
}

/// A sink that records what it is asked and answers from switches.
#[derive(Default)]
struct Fake {
    observed: Mutex<Vec<(String, u64, u64)>>,
    ended: Mutex<Vec<RunEnd>>,
    closed: Mutex<Vec<(HostId, NodeRow)>>,
    flushes: AtomicUsize,
    prunes: AtomicUsize,
    fail: AtomicBool,
    panic_on_observe: AtomicBool,
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
}

impl UsageSink for Fake {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &crate::usage::LoadIn,
        _: HostLimits,
        _: u64,
    ) -> Observation {
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
        self.down()?;
        self.ended.lock().unwrap().push(end.clone());
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        self.down()?;
        Ok(std::mem::take(&mut *self.closed.lock().unwrap()))
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

fn row(minute: u64) -> NodeRow {
    NodeRow {
        minute,
        cpu_millis: 1500,
        ws_max_bytes: 4096,
        leases: 2,
    }
}

/// The usage side over `fake`, with a key for each `(run id, key)`, and its clock.
fn usage(fake: &Arc<Fake>, keys: &[(&str, &str)]) -> (Usage, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(T0_MS));
    let map = keys
        .iter()
        .map(|(run, key)| (run.to_string(), ProfileKey::parse(key).unwrap()))
        .collect();
    let limits = HostLimits {
        threads: 8,
        memory_bytes: 1 << 34,
    };
    let usage = Usage::new(
        fake.clone(),
        Arc::new(Keys(map)),
        SharedClock::new(clock.clone()),
        limits,
    );
    (usage, clock)
}

fn beat(host: &str, results: Vec<RunResult>) -> Heartbeat {
    Heartbeat {
        host_id: host.to_string(),
        epoch: 3,
        session: String::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        results,
        taint: None,
        exclusive_ready: None,
        exclusive_refused: None,
        load: None,
    }
}

fn result(run: &str, lease: &str, outcome: Outcome) -> RunResult {
    RunResult {
        run_id: run.to_string(),
        lease_id: lease.to_string(),
        outcome,
        exit_code: None,
        detail: None,
    }
}

fn ack(run: &str, ack: Ack) -> ResultAck {
    ResultAck {
        run_id: run.to_string(),
        ack,
    }
}

/// A load with these `(lease, minute, mean, working set)` minutes.
fn load(seq: u64, minutes: &[(&str, u16, u32, u64)]) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: 10,
        cpu_usage_usec: 10,
        working_set_bytes: 1,
        active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        clocks: Vec::new(),
        minutes: minutes
            .iter()
            .map(|(lease, minute, mean, ws)| LeaseMinute {
                lease_id: lease.to_string(),
                minute: *minute,
                cpu_mean_millis: *mean,
                cpu_peak_window_millis: mean + 100,
                ws_max_bytes: *ws,
            })
            .collect(),
        dropped: LoadDrops::default(),
    }
}

/// The registry that holds every lease for every host.
fn anyone(_: &LeaseId, _: &HostId) -> bool {
    true
}

/// Plain beats of `host`, `n` of them, with nothing in them.
fn quiet_beats(usage: &Usage, host: &str, n: usize) {
    for _ in 0..n {
        usage.on_beat(&beat(host, Vec::new()), None, &[], &anyone);
    }
}

/// Asserts the life of a clean run: its lease's minutes are filed as the beats bring
/// them, an accepted `succeeded` result with a key starts a grace, a minute that comes on
/// a later beat joins the series (and the larger value of a repeated minute is kept), and
/// the run is handed to the sink on the second beat of the host after the
/// acknowledgement, once. It catches a run handed over at the acknowledgement (its last
/// partial minute lost), one handed over on the first beat of grace, one handed over
/// twice, and a late minute that begins a series of its own.
#[test]
fn a_clean_run_is_handed_over_after_its_grace_beats_with_its_late_minute() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1-1", "act.build")]);
    let minutes = [("1-1", 0, 1000, 100), ("1-1", 1, 2000, 300)];
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &minutes)),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1-1", Ack::Accepted)], &anyone);
    let late = [("1-1", 2, 500, 50), ("1-1", 1, 2500, 200)];
    usage.on_beat(&beat("h1", Vec::new()), Some(load(2, &late)), &[], &anyone);
    assert!(fake.ended().is_empty());
    quiet_beats(&usage, "h1", 1);
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].key.as_str(), "act.build");
    assert_eq!(ended[0].lease, LeaseId::new(1, 1));
    let at = |minute, cores_milli, ws_bytes| RunMinute {
        minute,
        cores_milli,
        ws_bytes,
    };
    let series = vec![at(0, 1000, 100), at(1, 2500, 300), at(2, 500, 50)];
    assert_eq!(ended[0].series.minutes, series);
    assert_eq!(ended[0].series.flags, 0);
    quiet_beats(&usage, "h1", 3);
    assert_eq!(fake.ended().len(), 1);
    assert_eq!(usage.counters().runs_learned, 1);
}

/// Asserts that the grace of a run is counted in beats of its own host: beats of another
/// host do not shorten it. It catches a grace counted over every host's beats, which
/// hands a run over before its host has sent the minute it was waiting for.
#[test]
fn the_grace_counts_the_beats_of_the_runs_own_host() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1-1", "k")]);
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &[("1-1", 0, 1, 1)])),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1-1", Ack::Accepted)], &anyone);
    quiet_beats(&usage, "h2", 5);
    assert!(fake.ended().is_empty());
    quiet_beats(&usage, "h1", 2);
    assert_eq!(fake.ended().len(), 1);
}

/// Asserts which results start a grace: only one the registry accepted (not a duplicate,
/// and not a stale, unknown-lease, mismatched or unknown verdict, nor an
/// acknowledgement that names another run) and whose outcome is `succeeded` or
/// `failed`; an accepted `killed` result discards the lease's minutes, and a result that
/// was not accepted leaves them for the resend. It catches a run learned from a killed
/// job (a short run would teach admission a too-low cost), from a result that was only
/// seen again, or from an acknowledgement of a different run.
#[test]
fn only_an_accepted_clean_result_starts_a_grace() {
    let fake = Arc::new(Fake::default());
    let keys = [
        ("run-a", "ka"),
        ("run-b", "kb"),
        ("run-c", "kc"),
        ("run-d", "kd"),
        ("run-e", "ke"),
        ("run-f", "kf"),
        ("run-g", "kg"),
    ];
    let (usage, _) = usage(&fake, &keys);
    let minutes = [
        ("1-1", 0, 1, 1),
        ("1-2", 0, 1, 1),
        ("1-3", 0, 1, 1),
        ("1-4", 0, 1, 1),
        ("1-5", 0, 1, 1),
        ("1-6", 0, 1, 1),
        ("1-7", 0, 1, 1),
    ];
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &minutes)),
        &[],
        &anyone,
    );
    let results = vec![
        result("run-a", "1-1", Outcome::Killed),
        result("run-b", "1-2", Outcome::Succeeded),
        result("run-c", "1-3", Outcome::Failed),
        result("run-d", "1-4", Outcome::Succeeded),
        result("run-e", "1-5", Outcome::Succeeded),
        result("run-f", "1-6", Outcome::Succeeded),
        result("run-g", "1-7", Outcome::Succeeded),
    ];
    let acks = [
        ack("run-a", Ack::Accepted),
        ack("run-b", Ack::Duplicate),
        ack("run-x", Ack::Accepted),
        ack("run-d", Ack::Stale),
        ack("run-e", Ack::UnknownLease),
        ack("run-f", Ack::Mismatch),
        ack("run-g", Ack::Unknown),
    ];
    usage.on_beat(&beat("h1", results), None, &acks, &anyone);
    quiet_beats(&usage, "h1", 4);
    assert!(fake.ended().is_empty());
    let again = beat("h1", vec![result("run-b", "1-2", Outcome::Failed)]);
    usage.on_beat(&again, None, &[ack("run-b", Ack::Accepted)], &anyone);
    quiet_beats(&usage, "h1", 2);
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].lease, LeaseId::new(1, 2));
    let after = beat("h1", vec![result("run-a2", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&after, None, &[ack("run-a2", Ack::Accepted)], &anyone);
    assert_eq!(usage.counters().runs_without_series, 1);
}

/// Asserts that a clean accepted result which cannot be learned from is counted under
/// the reason: the key source has no key (`runs_without_key`), the lease has no minutes
/// on file (`runs_without_series`), or the lease id is not `<term>-<seq>`
/// (`records_dropped`). It catches a silent skip, which would leave an operator with
/// empty profiles and no number to look at.
#[test]
fn a_run_that_cannot_be_learned_from_is_counted_by_reason() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &[("1-5", 0, 1, 1)])),
        &[],
        &anyone,
    );
    let results = vec![
        result("run-1", "1-5", Outcome::Succeeded),
        result("run-2", "junk", Outcome::Succeeded),
        result("run-3", "1-9", Outcome::Succeeded),
    ];
    let acks = [
        ack("run-1", Ack::Accepted),
        ack("run-2", Ack::Accepted),
        ack("run-3", Ack::Accepted),
    ];
    usage.on_beat(&beat("h1", results), None, &acks, &anyone);
    quiet_beats(&usage, "h1", 3);
    let counts = usage.counters();
    assert_eq!(counts.runs_without_key, 1);
    assert_eq!(counts.records_dropped, 1);
    assert_eq!(counts.runs_without_series, 1);
    assert_eq!(counts.runs_learned, 0);
    assert!(fake.ended().is_empty());
}

/// Asserts that a load with a scalar out of range never reaches the sink and is counted
/// as rejected, and that a load whose records the wire dropped is given to the sink and
/// has its drops counted, with the epoch of the beat. It catches a bad load handed on, a
/// rejection that is not counted, and an epoch taken from the wrong place.
#[test]
fn a_bad_load_is_counted_and_a_load_with_dropped_records_is_still_read() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    let mut bad = load(1, &[("1-1", 0, 1, 1)]);
    bad.working_set_bytes = u64::MAX;
    usage.on_beat(&beat("h1", Vec::new()), Some(bad), &[], &anyone);
    assert!(fake.observed.lock().unwrap().is_empty());
    assert_eq!(usage.counters().loads_rejected, 1);
    let mut partial = load(2, &[]);
    partial.covered = vec!["1-1".to_string(), "junk".to_string()];
    partial.dropped.over_cap = 2;
    usage.on_beat(&beat("h1", Vec::new()), Some(partial), &[], &anyone);
    let observed = fake.observed.lock().unwrap().clone();
    assert_eq!(observed, vec![("h1".to_string(), 3, 2)]);
    let counts = usage.counters();
    assert_eq!(counts.records_dropped, 3);
    assert_eq!(counts.loads_rejected, 1);
}

/// Asserts that a heartbeat whose host id is not an id is recorded as nothing. The
/// registry has checked the id before this runs, so this is the guard against a caller
/// that did not.
#[test]
fn a_heartbeat_with_no_valid_host_records_nothing() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    usage.on_beat(&beat("UPPER", Vec::new()), Some(load(1, &[])), &[], &anyone);
    assert!(fake.observed.lock().unwrap().is_empty());
    assert_eq!(usage.counters().records_dropped, 1);
}

/// Asserts that a reading the sink drops is counted, kept per host and minute with its
/// reason, and handed out once its minute is over and not before. It catches a tally
/// that is reported while readings can still be added to it (two warnings for one
/// minute) and one that is never cleared.
#[test]
fn dropped_readings_are_tallied_by_host_and_minute_until_the_minute_is_over() {
    let fake = Arc::new(Fake::default());
    *fake.drop_with.lock().unwrap() = Some(DropReason::NoBaseline);
    let (usage, _) = usage(&fake, &[]);
    for seq in [1, 2] {
        usage.on_beat(&beat("h1", Vec::new()), Some(load(seq, &[])), &[], &anyone);
    }
    assert_eq!(usage.counters().loads_dropped, 2);
    let mut hub = usage.hub();
    assert!(hub.take_drops(16).is_empty());
    let past = hub.take_drops(17);
    let key = (16, HostId::new("h1").unwrap());
    assert_eq!(past.len(), 1);
    assert_eq!(past[&key][&DropReason::NoBaseline], 2);
    assert!(hub.drops.is_empty());
}

/// Asserts the bound on what is kept: at most `MAX_TRACKS` leases have minutes on file,
/// the one past it is refused and counted, and a lease already on file still takes
/// minutes at the bound. It catches memory that grows with every lease a node ever ran
/// and a bound that also starves the leases already being measured.
#[test]
fn minutes_are_kept_for_a_bounded_number_of_leases() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    let minute = |lease: u64| MinuteIn {
        lease: LeaseId::new(1, lease),
        minute: 0,
        mean_cores_milli: 1,
        peak_window_milli: 1,
        working_set_max_bytes: 1,
    };
    let h1 = HostId::new("h1").unwrap();
    let held: BTreeSet<LeaseId> = (0..=MAX_TRACKS as u64)
        .map(|n| LeaseId::new(1, n))
        .collect();
    let mut hub = usage.hub();
    for lease in 0..=MAX_TRACKS as u64 {
        hub.add_minutes(&h1, &[minute(lease)], &held, 16);
    }
    assert_eq!(hub.running.len(), MAX_TRACKS);
    assert_eq!(hub.counts.minutes_refused, 1);
    hub.add_minutes(&h1, &[minute(0)], &held, 16);
    assert_eq!(hub.counts.minutes_refused, 1);
}

/// Asserts that minutes are filed for the host that has the lease: a lease the registry
/// does not hold for the beat's host begins no track (and so cannot fill the table with
/// invented ids), and the minutes of a lease on file from one host are refused when
/// another host sends them, whether the lease is running or has ended. Both are counted
/// in `minutes_unheld`. It catches a host that can move another host's series, or crowd
/// the real leases out of the table with lease ids it made up.
#[test]
fn a_host_files_minutes_only_for_leases_the_registry_holds_for_it() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1-1", "k")]);
    let h1_only =
        |lease: &LeaseId, host: &HostId| *lease == LeaseId::new(1, 1) && host.as_str() == "h1";
    // More invented leases than the table holds, in loads of the most minutes a load has.
    let invented: Vec<(String, u16, u32, u64)> = (100..100 + 2 * MAX_TRACKS as u64)
        .map(|n| (format!("1-{n}"), 0, 1, 1))
        .collect();
    for (seq, chunk) in invented.chunks(MAX_LOAD_MINUTES).enumerate() {
        let minutes: Vec<(&str, u16, u32, u64)> = chunk
            .iter()
            .map(|(l, m, c, w)| (l.as_str(), *m, *c, *w))
            .collect();
        let load = load(seq as u64 + 1, &minutes);
        usage.on_beat(&beat("h1", Vec::new()), Some(load), &[], &h1_only);
    }
    assert_eq!(usage.hub().running.len(), 0);
    assert_eq!(usage.counters().minutes_unheld, invented.len() as u64);
    assert_eq!(usage.counters().minutes_refused, 0);

    let mine = [("1-1", 0, 1000, 100)];
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(100, &mine)),
        &[],
        &h1_only,
    );
    let theirs = [("1-1", 0, 9000, 900), ("1-1", 1, 9000, 900)];
    usage.on_beat(
        &beat("h2", Vec::new()),
        Some(load(1, &theirs)),
        &[],
        &anyone,
    );
    let before = usage.counters().minutes_unheld;
    assert_eq!(before, invented.len() as u64 + 2);

    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1-1", Ack::Accepted)], &h1_only);
    usage.on_beat(
        &beat("h2", Vec::new()),
        Some(load(2, &theirs)),
        &[],
        &anyone,
    );
    assert_eq!(usage.counters().minutes_unheld, before + 2);
    quiet_beats(&usage, "h1", 2);
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    let only = RunMinute {
        minute: 0,
        cores_milli: 1000,
        ws_bytes: 100,
    };
    assert_eq!(ended[0].series.minutes, vec![only]);
}

/// Asserts that a panic in the sink during a beat is caught and counted as a fault, that
/// the call returns, and that the next beat is read as usual. It catches a panic that
/// unwinds out of the hook, which on the server would fail the heartbeat's reply after
/// the registry had accepted the beat and lose its acknowledgements.
#[test]
fn a_panic_in_the_sink_is_a_fault_and_the_hook_returns() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    fake.panic_on_observe.store(true, Ordering::SeqCst);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &[])), &[], &anyone);
    assert_eq!(usage.counters().faults, 1);
    fake.panic_on_observe.store(false, Ordering::SeqCst);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(2, &[])), &[], &anyone);
    assert_eq!(usage.counters().faults, 1);
    assert_eq!(fake.observed.lock().unwrap().len(), 1);
}

/// Asserts that a lease nothing has added to for a day is forgotten, running or ending,
/// and one added to within the day is kept, at the boundary minute. It catches a lease
/// that never produced a result staying for ever and filling the bound.
#[test]
fn a_lease_untouched_for_a_day_is_forgotten() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1", "k")]);
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &[("1-1", 0, 1, 1), ("1-2", 0, 1, 1)])),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1", "1-2", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1", Ack::Accepted)], &anyone);
    let mut hub = usage.hub();
    hub.expire(16 + TRACK_IDLE_MINUTES);
    assert_eq!((hub.running.len(), hub.ending.len()), (1, 1));
    hub.expire(16 + TRACK_IDLE_MINUTES + 1);
    assert_eq!((hub.running.len(), hub.ending.len()), (0, 0));
}

/// Asserts the schedule: a prune at the first tick and when the day changes and not
/// between, a flush once five wall minutes have passed since the last (and not at four),
/// both by the clock the server was given. It catches a flush on every tick, a prune on
/// every tick, one that ignores the clock, and a flush schedule that restarts from the
/// wrong minute.
#[test]
fn the_schedule_is_five_minutes_for_a_flush_and_a_day_for_a_prune() {
    let mut hub = Hub {
        running: BTreeMap::new(),
        ending: BTreeMap::new(),
        drops: Drops::new(),
        counts: UsageCounters::default(),
        last_flush: 16,
        last_prune_day: None,
    };
    assert_eq!(hub.due(16), (false, true));
    assert_eq!(hub.due(20), (false, false));
    assert_eq!(hub.due(21), (true, false));
    assert_eq!(hub.due(25), (false, false));
    assert_eq!(hub.due(26), (true, false));
    assert_eq!(hub.due(1439), (true, false));
    assert_eq!(hub.due(1440), (false, true));
}

/// Asserts the ticks of the server drive the sink: nothing is flushed before five
/// minutes, a flush happens at five and again five later, the prune runs once on the
/// first tick and again after a day. It catches a tick that reads the schedule but does
/// not call the sink, and one that calls it every time.
#[tokio::test]
async fn ticks_call_the_sink_on_the_schedule() {
    quiet();
    let fake = Arc::new(Fake::default());
    let (usage, clock) = usage(&fake, &[]);
    let usage = Arc::new(usage);
    let at = |minute: u64| Timestamp::from_millis(minute * 60_000);
    let seen = |fake: &Fake| {
        (
            fake.flushes.load(Ordering::SeqCst),
            fake.prunes.load(Ordering::SeqCst),
        )
    };
    usage.tick().await;
    assert_eq!(seen(&fake), (0, 1));
    clock.set(at(20));
    usage.tick().await;
    assert_eq!(seen(&fake), (0, 1));
    clock.set(at(21));
    usage.tick().await;
    assert_eq!(seen(&fake), (1, 1));
    clock.set(at(25));
    usage.tick().await;
    assert_eq!(seen(&fake), (1, 1));
    clock.set(at(26));
    usage.tick().await;
    assert_eq!(seen(&fake), (2, 1));
    clock.set(at(1440));
    usage.tick().await;
    assert_eq!(seen(&fake), (3, 2));
}

/// Asserts the flush at shutdown writes whatever the schedule says, takes the closed
/// rows the sink returns, and reports (and clears) the dropped readings of minutes that
/// are over. It catches a final flush that waits for its schedule and a tally that is
/// never reported.
#[tokio::test]
async fn the_final_flush_does_not_wait_for_its_schedule_and_clears_the_tally() {
    quiet();
    let fake = Arc::new(Fake::default());
    *fake.drop_with.lock().unwrap() = Some(DropReason::SeqNotAbove);
    fake.closed
        .lock()
        .unwrap()
        .push((HostId::new("h1").unwrap(), row(15)));
    let (usage, clock) = usage(&fake, &[]);
    let usage = Arc::new(usage);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &[])), &[], &anyone);
    clock.set(Timestamp::from_millis(17 * 60_000));
    usage.finish().await;
    assert_eq!(fake.flushes.load(Ordering::SeqCst), 1);
    assert!(fake.closed.lock().unwrap().is_empty());
    assert!(usage.hub().drops.is_empty());
}

/// Asserts that a sink that fails to flush or to prune costs a count and nothing else:
/// no panic, the failure counted, and the next flush asked for again. It catches a
/// failure that unwinds into the tick loop, which would stop the server ticking.
#[tokio::test]
async fn a_failing_flush_or_prune_is_counted_and_survived() {
    quiet();
    let fake = Arc::new(Fake::default());
    fake.fail.store(true, Ordering::SeqCst);
    let (usage, _) = usage(&fake, &[]);
    let usage = Arc::new(usage);
    usage.tick().await;
    assert_eq!(usage.counters().faults, 1);
    usage.finish().await;
    usage.finish().await;
    assert_eq!(usage.counters().faults, 3);
    assert_eq!(fake.flushes.load(Ordering::SeqCst), 2);
}

/// Asserts that a sink that cannot take a finished run costs a count and the run is not
/// counted as learned, and that the sink's own counters are in the total. It catches a
/// learned count that rises on failure.
#[test]
fn a_run_the_sink_cannot_take_is_a_fault_and_not_a_learned_run() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1", "k")]);
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &[("1-1", 0, 1, 1)])),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1", Ack::Accepted)], &anyone);
    fake.fail.store(true, Ordering::SeqCst);
    quiet_beats(&usage, "h1", 2);
    let counts = usage.counters();
    assert_eq!((counts.faults, counts.runs_learned), (1, 0));
    assert_eq!(counts.sink.minutes_deduped, 7);
    assert_eq!(usage.now_minute(), 16);
}
