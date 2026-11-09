//! The server's side of the usage hub without a socket: a recording sink stands in for the
//! measuring crate, and heartbeats are the wire's. What the hub does with plain values is
//! tested in `js-usage`; these tests cover what the server adds: reading the wire, the
//! results a beat carries, the caught panics, the pool for blocking calls and the log.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_agent_wire::{LeaseMinute, LoadDrops, Outcome, Phase, RunResult};
use js_types::{ManualClock, Timestamp};

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

/// The usage side over `fake`, and its clock.
fn usage(fake: &Arc<Fake>) -> (Usage, ManualClock) {
    let clock = ManualClock::new(Timestamp::from_millis(T0_MS));
    let usage = Usage::new(fake.clone(), SharedClock::new(clock.clone()), limits());
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
        disk_gib: None,
        jobs: Vec::new(),
        retired_jobs: Vec::new(),
        log_grant_state: None,
    }
}

fn result(run: &str, lease: &str, outcome: Outcome) -> RunResult {
    RunResult {
        run_id: run.to_string(),
        lease_id: lease.to_string(),
        outcome,
        exit_code: None,
        detail: None,
        log_tail: None,
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

/// Asserts that a load with a scalar out of range never reaches the sink and is counted
/// as rejected, and that a load whose records the wire dropped is given to the sink and
/// has its drops counted (the wire's and the ids that do not parse), with the epoch of the
/// beat. It catches a bad load handed on, a rejection that is not counted, and an epoch
/// taken from the wrong place.
#[test]
fn a_bad_load_is_counted_and_a_load_with_dropped_records_is_still_read() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake);
    let mut bad = load(1, &[("1-1", 0, 1, 1)]);
    bad.working_set_bytes = u64::MAX;
    usage.on_beat(&beat("h1", Vec::new()), Some(bad));
    assert!(fake.observed.lock().unwrap().is_empty());
    assert_eq!(usage.counters().loads_rejected, 1);
    let mut partial = load(2, &[]);
    partial.covered = vec!["1-1".to_string(), "junk".to_string()];
    partial.dropped.over_cap = 2;
    usage.on_beat(&beat("h1", Vec::new()), Some(partial));
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
    let (usage, _) = usage(&fake);
    usage.on_beat(&beat("UPPER", Vec::new()), Some(load(1, &[])));
    assert!(fake.observed.lock().unwrap().is_empty());
    assert_eq!(usage.counters().records_dropped, 1);
}

/// Asserts that the results a heartbeat carries teach the usage side nothing: a beat with
/// a load whose minutes name a lease, and then a beat with a `succeeded` result for it,
/// hands the sink no run and counts none as learned or without a series, and the load is
/// still read. No lease is held, so the previous release's result is not a run to learn
/// from.
#[test]
fn a_result_a_heartbeat_carries_teaches_the_usage_side_nothing() {
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake);
    let minutes = [("1-1", 0, 1000, 100), ("1-1", 1, 2000, 300)];
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &minutes)));
    let done = beat("h1", vec![result("run-1-1", "1-1", Outcome::Succeeded)]);
    usage.on_beat(&done, Some(load(2, &[])));
    assert!(fake.ended().is_empty());
    let counts = usage.counters();
    assert_eq!((counts.runs_learned, counts.runs_without_series), (0, 0));
    assert_eq!(fake.observed.lock().unwrap().len(), 2);
}

/// Asserts that a panic in the sink during a beat is caught and counted as a fault, that
/// the call returns, and that the next beat is read as usual. It catches a panic that
/// unwinds out of the hook, which on the server would fail the heartbeat's reply after
/// the registry had accepted the beat and lose its acknowledgements.
#[test]
fn a_panic_in_the_sink_is_a_fault_and_the_hook_returns() {
    quiet();
    let fake = Arc::new(Fake::default());
    let (usage, _) = usage(&fake);
    fake.panic_on_observe.store(true, Ordering::SeqCst);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &[])));
    assert_eq!(usage.counters().faults, 1);
    fake.panic_on_observe.store(false, Ordering::SeqCst);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(2, &[])));
    assert_eq!(usage.counters().faults, 1);
    assert_eq!(fake.observed.lock().unwrap().len(), 1);
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
    let (usage, clock) = usage(&fake);
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
    let (usage, clock) = usage(&fake);
    usage.on_beat(&beat("h1", Vec::new()), Some(load(1, &[])));
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
    let (usage, _) = usage(&fake);
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
        crate::api::rig::process_sink();
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
    let (usage, _) = usage(&fake);
    for seq in 1..=12u64 {
        for host in ["h1", "h2"] {
            let minutes = [("1-1", u16::try_from(seq).unwrap(), 1000, 100)];
            usage.on_beat(&beat(host, Vec::new()), Some(load(seq, &minutes)));
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
    let (usage, clock) = usage(&fake);
    for seq in [1, 2] {
        usage.on_beat(&beat("h1", Vec::new()), Some(load(seq, &[])));
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
    let (usage, _) = usage(&fake);
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
