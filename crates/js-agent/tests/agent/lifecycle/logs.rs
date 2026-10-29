//! The log tail of a job that exited: read before the container is removed, carried by
//! the result, and never able to change the result.

use js_agent::lifecycle::Event;
use js_agent_wire::limits::{
    HEARTBEAT_BODY_BUDGET, MAX_LOAD_LEASES, MAX_LOAD_MINUTES, MAX_MEMORY_BYTES, TAIL_MINUTE,
};
use js_agent_wire::{
    Heartbeat, LeaseClock, LeaseMinute, LoadDrops, LogTail, NodeLoad, Outcome, Phase,
};

use super::{Harness, job};

fn tail() -> LogTail {
    LogTail {
        stdout: "compiled\n".to_owned(),
        stderr: "error: boom\n".to_owned(),
        truncated: false,
    }
}

/// Asserts that the result of a job that exited carries the tail the runtime gave, and
/// that the log was read before the container was removed (the runtime has no log to read
/// once the container is gone). It catches a result sent without the tail, and a read
/// placed after the removal, which would find nothing.
#[test]
fn the_result_carries_the_tail_read_before_the_removal() {
    let mut h = Harness::new("log-tail");
    h.start(job("r1"));
    h.fake.print("r1", tail());
    h.fake.exit("r1", 3);
    h.life.step();
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Failed);
    assert_eq!(results[0].log_tail, Some(tail()));
    let log = h.log.borrow();
    let at = |prefix: &str| log.iter().position(|line| line.starts_with(prefix));
    let (read, removed) = (at("logs c1").unwrap(), at("remove c1").unwrap());
    assert!(read < removed, "{log:?}");
}

/// Asserts that a log that cannot be read leaves the result as it would have been without
/// it (the same outcome and exit code, the container removed, the slot free) but without a
/// tail, and notes a `logs-failed` finding for the operator. It catches a run that fails,
/// hangs or keeps its container because its log was unreadable.
#[test]
fn a_failed_logs_call_still_yields_the_result_without_a_tail() {
    let mut h = Harness::new("log-fail");
    h.start(job("r1"));
    h.fake.print("r1", tail());
    h.fake.fail("logs", u32::MAX);
    h.fake.exit("r1", 0);
    h.life.step();
    let results = h.life.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, Outcome::Succeeded);
    assert_eq!(results[0].exit_code, Some(0));
    assert_eq!(results[0].log_tail, None);
    assert!(h.fake.gone("r1"), "the container is removed all the same");
    assert_eq!(h.life.free().cpu_millis, 4000, "the slot is free");
    let findings = h.life.findings();
    assert!(
        findings.iter().any(|found| found.code == "logs-failed"),
        "{findings:?}"
    );
}

/// Asserts that a job whose runtime keeps no log (a tail of `None`) is reported without a
/// tail and without a finding. It catches a backend that cannot read logs being reported
/// as a failure on every run.
#[test]
fn a_runtime_without_logs_gives_no_tail_and_no_finding() {
    let mut h = Harness::new("log-none");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    assert_eq!(h.life.results()[0].log_tail, None);
    assert!(h.life.findings().is_empty());
}

/// A tail of `prefix`, then `n` bytes of a control character (six bytes each once
/// serialised), then `suffix`, on both streams.
fn noisy(prefix: &str, n: usize, suffix: &str) -> LogTail {
    let text = format!("{prefix}{}{suffix}", "\u{1}".repeat(n));
    LogTail {
        stdout: text.clone(),
        stderr: text,
        truncated: false,
    }
}

/// Asserts that a job that prints control characters, which serialise six times as large,
/// keeps a tail: the tail is cut down (the end kept, `truncated` set) until the results fit
/// the heartbeat, and no finding is noted. It catches a tail dropped whole, which is the
/// very job an operator wants to see, and one that is sent over the body the server reads.
#[test]
fn a_tail_that_serialises_too_large_is_truncated_to_fit() {
    let mut h = Harness::new("log-shed-cut");
    h.start(job("r1"));
    h.fake.print("r1", noisy("START", 8000, "END"));
    h.fake.exit("r1", 1);
    h.life.step();
    let result = h.life.results().remove(0);
    let tail = result.log_tail.clone().unwrap();
    assert!(tail.truncated);
    assert!(tail.stdout.ends_with("END") && tail.stderr.ends_with("END"));
    assert!(!tail.stdout.contains("START"), "the beginning went first");
    assert!(!tail.stdout.is_empty());
    assert!(serde_json::to_vec(&result).unwrap().len() <= 48 * 1024);
    assert!(h.life.findings().is_empty());
}

/// Asserts that when even an empty tail does not fit the budget the tail is left out, the
/// rest of the result is kept, and the run notes a `log-tail-shed` finding; and that a
/// tail that fits is left alone. It catches a silent drop (no finding), a result lost with
/// its tail, and a tail cut that did not need to be.
#[test]
fn a_tail_that_cannot_fit_even_empty_is_dropped_with_a_finding() {
    let mut h = Harness::new("log-shed-drop");
    h.start(job("r1"));
    h.fake.print("r1", tail());
    h.fake.exit("r1", 1);
    h.life.step();
    let mut events = Vec::new();
    h.life.fit_log_tails(1 << 20, &mut events);
    assert_eq!(h.life.results()[0].log_tail, Some(tail()), "it fits");
    h.life.fit_log_tails(1, &mut events);
    let result = h.life.results().remove(0);
    assert_eq!(
        (result.outcome, result.exit_code),
        (Outcome::Failed, Some(1))
    );
    assert_eq!(result.log_tail, None);
    let findings = h.life.findings();
    assert!(
        findings.iter().any(|f| f.code == "log-tail-shed"),
        "{findings:?}"
    );
}

/// Asserts that a tail with a multi-byte character at the halving point is cut at a
/// character boundary (the end kept, whole characters only), that an empty stream stays
/// empty, and that the cut stops as soon as the results fit. It catches a cut that splits
/// a character (a broken string on the wire) and one that goes on past what was needed.
#[test]
fn halving_cuts_at_a_character_boundary_and_stops_when_it_fits() {
    let mut h = Harness::new("log-shed-boundary");
    let tail = LogTail {
        stdout: "\u{20AC}".repeat(41),
        stderr: String::new(),
        truncated: false,
    };
    h.fake.print("r1", tail);
    h.start(job("r1"));
    h.fake.exit("r1", 1);
    h.life.step();
    let size = serde_json::to_vec(&h.life.results()[0]).unwrap().len();
    h.life.fit_log_tails(size - 1, &mut Vec::new());
    let cut = h.life.results().remove(0).log_tail.unwrap();
    assert_eq!(
        cut.stdout,
        "\u{20AC}".repeat(20),
        "61 bytes, from a boundary: 60"
    );
    assert_eq!(cut.stderr, "");
    assert!(cut.truncated);
}

/// A load with every list at its cap and every id and number at its longest, as the wire's
/// own size test builds it.
fn full_load() -> NodeLoad {
    let id = |i: usize| format!("{}-{}", u64::MAX - i as u64, u64::MAX);
    NodeLoad {
        seq: u64::MAX,
        mono_usec: u64::MAX,
        cpu_usage_usec: u64::MAX,
        working_set_bytes: MAX_MEMORY_BYTES,
        active_file_bytes: MAX_MEMORY_BYTES,
        actions_cpu_limited: true,
        covered: (0..MAX_LOAD_LEASES).map(id).collect(),
        clocks: (0..MAX_LOAD_LEASES)
            .map(|i| LeaseClock {
                lease_id: id(i),
                started_mono_usec: u64::MAX,
            })
            .collect(),
        minutes: (0..MAX_LOAD_MINUTES)
            .map(|i| LeaseMinute {
                lease_id: id(i),
                minute: TAIL_MINUTE,
                cpu_mean_millis: u32::MAX,
                cpu_peak_window_millis: u32::MAX,
                ws_max_bytes: MAX_MEMORY_BYTES,
            })
            .collect(),
        dropped: LoadDrops::default(),
    }
}

/// Asserts that a whole heartbeat of a busy host stays within the body the server reads:
/// 96 held runs with 128-byte ids, a load at every cap, and the 8 finished runs whose
/// tails are full of control characters (six times as large once serialised), after the
/// agent has shed what does not fit. A server answers a beat over the body limit with 413,
/// which ends the agent, so this is the line between a noisy job and a dead agent. It
/// catches a results budget that leaves too little room for the load and the held list
/// (the first one, 40 KiB for the results alone, fails it).
#[test]
fn a_full_beat_with_noisy_tails_fits_the_body_limit() {
    let mut h = Harness::new("log-full-beat");
    let wide = |i: usize| format!("{i:0>128}");
    for i in 0..MAX_LOAD_LEASES {
        let mut spec = job(&wide(i));
        spec.limits.cpu_millis = 40;
        spec.limits.memory_bytes = 1 << 20;
        // "lease-" and a 128-byte run id would be over the id cap and the start refused.
        spec.lease_id = format!("l{i}");
        let events = h.start(spec);
        assert!(
            matches!(events.first(), Some(Event::Started { .. })),
            "run {i} was not started: {events:?}"
        );
    }
    for i in 0..8 {
        h.fake.print(&wide(i), noisy("S", 8190, "E"));
        h.fake.exit(&wide(i), 1);
    }
    h.life.step();
    let beat = Heartbeat {
        host_id: wide(0),
        epoch: u64::MAX,
        session: wide(1),
        phase: Phase::Serving,
        held: h.life.held(),
        results: h.life.results(),
        taint: None,
        exclusive_ready: None,
        exclusive_refused: None,
        load: Some(full_load()),
    };
    assert_eq!((beat.held.len(), beat.results.len()), (MAX_LOAD_LEASES, 8));
    assert_eq!(beat.validate(), Ok(()));
    let size = serde_json::to_vec(&beat).unwrap().len();
    assert!(size <= HEARTBEAT_BODY_BUDGET, "{size} bytes");
    let kept = beat.results.iter().filter(|r| r.log_tail.is_some()).count();
    assert!(kept > 0, "the tails are cut down and not all dropped");
}
