//! The server's side of the usage hub without a socket: a recording sink and a map of
//! keys stand in for the measuring crate and the registry, and heartbeats are the wire's.
//! What the hub does with plain values is tested in `js-usage`; these tests cover what the
//! server adds: reading the wire, the acknowledgements, the caught panics, the pool for
//! blocking calls and the log.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_agent_wire::{LeaseMinute, LoadDrops, Phase, RunResult};
use js_lease::{ManualClock, Timestamp};
use js_usage::GRACE_BEATS;

use super::*;
use crate::usage::{
    DropReason, KeyProfile, Observation, Observed, ProfileKey, RunEnd, SinkCounters,
};
use crate::usage::{LoadIn, UsageFault};

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
}

impl UsageSink for Fake {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
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
        assert!(
            !self.panic_on_ended.load(Ordering::SeqCst),
            "the sink is broken"
        );
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

    fn node_rows(
        &self,
        _: &HostId,
        from: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        self.down()?;
        Ok(vec![row(from)])
    }

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.down()?;
        Ok(None)
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

fn row(minute: u64) -> NodeRow {
    NodeRow {
        minute,
        cpu_millis: 1500,
        ws_max_bytes: 4096,
        leases: 2,
    }
}

fn limits() -> HostLimits {
    HostLimits {
        threads: 8,
        memory_bytes: 1 << 34,
    }
}

/// The usage side over `fake`, with a key for each `(run id, key)`, and its clock.
fn usage(fake: &Arc<Fake>, keys: &[(&str, &str)]) -> (Usage, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(T0_MS));
    let map = keys
        .iter()
        .map(|(run, key)| (run.to_string(), ProfileKey::parse(key).unwrap()))
        .collect();
    let usage = Usage::new(
        fake.clone(),
        Arc::new(Keys(map)),
        SharedClock::new(clock.clone()),
        limits(),
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

/// The beats of grace a clean run waits out.
fn grace() -> usize {
    usize::from(GRACE_BEATS)
}

/// Asserts the life of a clean run as the wire tells it: minutes arrive in loads, an
/// accepted `succeeded` result starts the grace, a minute that comes on a later beat
/// joins the series, and the run is handed to the sink under its key on the second beat
/// of the host after the acknowledgement. It catches a heartbeat that is read into the
/// wrong plain values (a lease id swapped for its neighbour, an epoch from the wrong
/// place) in a way the hub's own tests, which start from plain values, cannot see.
#[test]
fn a_clean_run_on_the_wire_is_handed_over_after_its_grace_beats() {
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
    let at = |minute, cores_milli, ws_bytes| js_usage::RunMinute {
        minute,
        cores_milli,
        ws_bytes,
    };
    let series = vec![at(0, 1000, 100), at(1, 2500, 300), at(2, 500, 50)];
    assert_eq!(ended[0].series.minutes, series);
    assert_eq!(usage.counters().runs_learned, 1);
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
    quiet_beats(&usage, "h1", grace());
    let ended = fake.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].lease, LeaseId::new(1, 2));
    let after = beat("h1", vec![result("run-a2", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&after, None, &[ack("run-a2", Ack::Accepted)], &anyone);
    assert_eq!(usage.counters().runs_without_series, 1);
}

/// Asserts which outcomes are clean: `succeeded` and `failed` are, and every other one
/// (killed, timed out, out of memory, failed to start, and one this version does not
/// know) is not. It catches an outcome that teaches admission the cost of a job that was
/// cut short.
#[test]
fn only_a_job_that_ran_to_its_own_end_is_clean() {
    assert_eq!(finish_of(Outcome::Succeeded), Finish::Clean);
    assert_eq!(finish_of(Outcome::Failed), Finish::Clean);
    for cut_short in [
        Outcome::Killed,
        Outcome::TimedOut,
        Outcome::OutOfMemory,
        Outcome::FailedToStart,
        Outcome::Unknown,
    ] {
        assert_eq!(finish_of(cut_short), Finish::Unclean);
    }
}

/// Asserts that an accepted result whose lease id is not `<term>-<seq>` is counted in
/// `records_dropped` and reaches no run, and that the others of its beat are still read
/// (the lease of the valid one has no minutes, so it is counted as such). It catches a
/// bad id that fails the whole beat and a silent skip.
#[test]
fn an_accepted_result_with_an_unparsable_lease_is_a_dropped_record() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1", "k")]);
    let results = vec![
        result("run-2", "junk", Outcome::Succeeded),
        result("run-1", "1-9", Outcome::Succeeded),
    ];
    let acks = [ack("run-2", Ack::Accepted), ack("run-1", Ack::Accepted)];
    usage.on_beat(&beat("h1", results), None, &acks, &anyone);
    let counts = usage.counters();
    assert_eq!(counts.records_dropped, 1);
    assert_eq!(counts.runs_without_series, 1);
}

/// Asserts that a load with a scalar out of range never reaches the sink and is counted
/// as rejected, and that a load whose records the wire dropped is given to the sink and
/// has its drops counted (the wire's and the ids that do not parse), with the epoch of the
/// beat. It catches a bad load handed on, a rejection that is not counted, and an epoch
/// taken from the wrong place.
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

/// Asserts that the registry's answer to "does it hold this lease for this host" is the
/// hub's: a lease the closure does not hold for the beat's host begins no track, so its
/// run finds no minutes. It catches a closure that is dropped on the way to the hub.
#[test]
fn the_registrys_holds_check_reaches_the_hub() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1", "k")]);
    let nobody = |_: &LeaseId, _: &HostId| false;
    let minutes = [("1-1", 0, 1, 1)];
    let first = Some(load(1, &minutes));
    usage.on_beat(&beat("h1", Vec::new()), first, &[], &nobody);
    let done = beat("h1", vec![result("run-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1", Ack::Accepted)], &nobody);
    let counts = usage.counters();
    assert_eq!((counts.minutes_unheld, counts.runs_without_series), (1, 1));
}

/// Asserts that a panic in the sink during a beat is caught and counted as a fault, that
/// the call returns, and that the next beat is read as usual. It catches a panic that
/// unwinds out of the hook, which on the server would fail the heartbeat's reply after
/// the registry had accepted the beat and lose its acknowledgements.
#[test]
fn a_panic_in_the_sink_is_a_fault_and_the_hook_returns() {
    quiet();
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

/// Asserts that a panic in the key source, which is asked while the hub is held, is caught
/// and counted as a fault, that the call returns, and that the next beat is read as usual.
/// It catches a panic that unwinds out of the hook, and one that leaves the hub unusable.
#[test]
fn a_panic_in_the_key_source_is_a_fault_and_the_hub_goes_on() {
    quiet();
    let fake = Arc::new(Fake::default());
    let clock = ManualClock::new(Timestamp::from_millis(T0_MS));
    let usage = Usage::new(
        fake.clone(),
        Arc::new(BrokenKeys),
        SharedClock::new(clock),
        limits(),
    );
    let minutes = [("1-1", 0, 1000, 100)];
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &minutes)),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1-1", Ack::Accepted)], &anyone);
    assert_eq!(usage.counters().faults, 1);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(2, &[])), &[], &anyone);
    assert_eq!(usage.counters().faults, 1);
    assert_eq!(fake.observed.lock().unwrap().len(), 2);
}

/// Asserts that a panic in the sink while it takes a finished run is caught and counted
/// as a fault, that the call returns, the run is not counted as learned, and the next beat
/// is read as usual. It catches a panic that unwinds out of the hook after the run was
/// taken out of the hub.
#[test]
fn a_panic_when_a_run_is_handed_over_is_a_fault_and_the_hook_returns() {
    quiet();
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[("run-1-1", "k")]);
    let minutes = [("1-1", 0, 1000, 100)];
    usage.on_beat(
        &beat("h1", Vec::new()),
        Some(load(1, &minutes)),
        &[],
        &anyone,
    );
    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, None, &[ack("run-1-1", Ack::Accepted)], &anyone);
    fake.panic_on_ended.store(true, Ordering::SeqCst);
    quiet_beats(&usage, "h1", grace());
    assert_eq!(usage.counters().faults, 1);
    assert_eq!(usage.counters().runs_learned, 0);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(2, &[])), &[], &anyone);
    assert_eq!(fake.observed.lock().unwrap().len(), 2);
}

/// Asserts that a sink that cannot take a finished run costs a count and the run is not
/// counted as learned, that the sink's own counters are in the total, and that the usage
/// side reads the server's clock in wall minutes and asks the sink for its curves. It
/// catches a learned count that rises on failure.
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
    quiet_beats(&usage, "h1", grace());
    let counts = usage.counters();
    assert_eq!((counts.faults, counts.runs_learned), (1, 0));
    assert_eq!(counts.sink.minutes_deduped, 7);
    assert_eq!(usage.now_minute(), 16);
    assert!(usage.curves().is_none());
}

/// Asserts the ticks of the server drive the sink on the pool for blocking calls: nothing
/// is flushed before five minutes, a flush happens at five and again five later, the
/// prune runs once on the first tick and again after a day. It catches a tick that reads
/// the schedule but does not call the sink, one that calls it every time, and one that
/// reads the wrong clock.
#[tokio::test]
async fn ticks_call_the_sink_on_the_schedule() {
    quiet();
    let fake = Arc::new(Fake::default());
    let (usage, clock) = usage(&fake, &[]);
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
/// rows the sink returns, and clears the dropped readings of minutes that are over. It
/// catches a final flush that waits for its schedule and a tally that is never reported.
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
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &[])), &[], &anyone);
    clock.set(Timestamp::from_millis(17 * 60_000));
    usage.finish().await;
    assert_eq!(fake.flushes.load(Ordering::SeqCst), 1);
    assert!(fake.closed.lock().unwrap().is_empty());
    assert_eq!(usage.counters().loads_dropped, 1);
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
    usage.tick().await;
    assert_eq!(usage.counters().faults, 1);
    usage.finish().await;
    usage.finish().await;
    assert_eq!(usage.counters().faults, 3);
    assert_eq!(fake.flushes.load(Ordering::SeqCst), 2);
}

/// A log that is written to a buffer for the thread that made it.
struct Captured {
    text: Arc<Mutex<Vec<u8>>>,
    _scope: tracing::subscriber::DefaultGuard,
}

struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn start() -> Self {
        let text = Arc::new(Mutex::new(Vec::new()));
        let sink = text.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || Buffer(sink.clone()))
            .finish();
        Self {
            text,
            _scope: tracing::subscriber::set_default(subscriber),
        }
    }

    fn lines_with(&self, needle: &str) -> usize {
        let text = String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned();
        text.lines().filter(|line| line.contains(needle)).count()
    }
}

/// Asserts that handling heartbeats logs no node minute, whatever the host or the number
/// of beats (twelve beats of each of two hosts, with loads and minutes), and that the
/// flush then logs one for each minute the sink closed. The capture is of this thread
/// only, so no other test's lines are counted. It catches a line per heartbeat, which
/// would drown the log, with or without fields.
#[tokio::test]
async fn beats_log_no_node_minute_and_a_flush_logs_one_for_each_closed() {
    let log = Captured::start();
    let fake = Arc::new(Fake::default());
    {
        let mut closed = fake.closed.lock().unwrap();
        closed.push((HostId::new("h1").unwrap(), row(15)));
        closed.push((HostId::new("h2").unwrap(), row(15)));
    }
    let (usage, _) = usage(&fake, &[]);
    for seq in 1..=12u64 {
        for host in ["h1", "h2"] {
            let minutes = [("1-1", u16::try_from(seq).unwrap(), 1000, 100)];
            usage.on_beat(
                &beat(host, Vec::new()),
                Some(load(seq, &minutes)),
                &[],
                &anyone,
            );
        }
    }
    assert_eq!(log.lines_with("node minute"), 0);
    usage.finish().await;
    assert_eq!(log.lines_with("node minute"), 2);
}

/// Asserts that a flush warns once for each host and minute that had readings dropped,
/// with the reason's fixed name, once the minute is over, and logs the sink's failure
/// when a flush or a prune fails. It catches a tally that is not reported and a failure
/// that is swallowed without a line.
#[tokio::test]
async fn a_flush_logs_the_dropped_readings_and_the_failures() {
    let log = Captured::start();
    let fake = Arc::new(Fake::default());
    *fake.drop_with.lock().unwrap() = Some(DropReason::ClockBack);
    let (usage, clock) = usage(&fake, &[]);
    for seq in [1, 2] {
        usage.on_beat(&beat("h1", Vec::new()), Some(load(seq, &[])), &[], &anyone);
    }
    clock.set(Timestamp::from_millis(17 * 60_000));
    usage.finish().await;
    assert_eq!(log.lines_with("node readings dropped"), 1);
    assert_eq!(log.lines_with("clock-back=2"), 1);
    fake.fail.store(true, Ordering::SeqCst);
    usage.finish().await;
    usage.tick().await;
    assert_eq!(log.lines_with("cannot write the usage rows"), 1);
    assert_eq!(log.lines_with("cannot prune the usage store"), 1);
}

/// Asserts that the server's reads of rows and of a key's profile pass the sink's answer
/// and its failure on, the failure counted. It catches a wrapper that answers from
/// somewhere else or loses a failure.
#[test]
fn reads_pass_the_sinks_answer_and_failure_on() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake, &[]);
    let host = HostId::new("h1").unwrap();
    let key = ProfileKey::parse("k").unwrap();
    assert_eq!(
        usage.node_rows(&host, 5, 9, NonZeroUsize::MIN),
        Ok(vec![row(5)])
    );
    assert_eq!(usage.key_profile(&key), Ok(None));
    fake.fail.store(true, Ordering::SeqCst);
    assert!(usage.node_rows(&host, 5, 9, NonZeroUsize::MIN).is_err());
    assert!(usage.key_profile(&key).is_err());
    assert_eq!(usage.counters().faults, 2);
}
