//! Hostile input to a heartbeat's load: every rule of `NodeLoad::vet` has a test, and
//! each says the defect it catches.

mod common;

use common::{expect, heartbeat, load, long};
use js_agent_wire::limits::*;
use js_agent_wire::refusal;
use js_agent_wire::{
    Heartbeat, HeldRun, HeldState, LeaseClock, LeaseMinute, LoadDrops, NodeLoad, Outcome, Rule,
    RunResult, lease_parts,
};

const BAD_IDS: [&str; 8] = [
    "",
    "01-1",
    "+1-1",
    "1-",
    "-1",
    "1-1-1",
    "1.1",
    "1-18446744073709551616",
];

fn minute(lease: &str, minute: u16, mean: u32, peak: u32, ws: u64) -> LeaseMinute {
    LeaseMinute {
        lease_id: lease.into(),
        minute,
        cpu_mean_millis: mean,
        cpu_peak_window_millis: peak,
        ws_max_bytes: ws,
    }
}

fn clock(lease: &str, started: u64) -> LeaseClock {
    LeaseClock {
        lease_id: lease.into(),
        started_mono_usec: started,
    }
}

/// The fixture load with only the given lists.
fn with(minutes: Vec<LeaseMinute>, clocks: Vec<LeaseClock>, covered: Vec<String>) -> NodeLoad {
    NodeLoad {
        minutes,
        clocks,
        covered,
        ..load()
    }
}

fn vetted(mut load: NodeLoad) -> NodeLoad {
    assert_eq!(load.vet(), Ok(()));
    load
}

fn drops(invalid: u32, duplicate: u32, over_cap: u32) -> LoadDrops {
    LoadDrops {
        malformed: 0,
        invalid,
        duplicate,
        over_cap,
    }
}

/// Asserts that an honest load passes `vet` untouched with nothing dropped. It catches a
/// rule that is too eager.
#[test]
fn an_honest_load_passes_vet_untouched() {
    let mut got = load();
    assert_eq!(got.vet(), Ok(()));
    assert_eq!(got, load());
    assert_eq!(got.dropped.total(), 0);
}

/// Asserts that the two memory scalars pass at `MAX_MEMORY_BYTES` and fail one above with
/// the right field, rule and code. It catches `<` for `<=`, a wrong field name and a rule
/// put on the wrong scalar.
#[test]
fn scalar_bounds() {
    let code = refusal::INVALID_HEARTBEAT;
    let mut at_cap = load();
    at_cap.working_set_bytes = MAX_MEMORY_BYTES;
    at_cap.active_file_bytes = MAX_MEMORY_BYTES;
    assert_eq!(at_cap.vet(), Ok(()));

    let mut ws = load();
    ws.working_set_bytes = MAX_MEMORY_BYTES + 1;
    let err = ws.vet().unwrap_err();
    expect(
        err,
        "heartbeat.load.working_set_bytes",
        Rule::OutOfRange,
        code,
    );

    let mut active = load();
    active.active_file_bytes = MAX_MEMORY_BYTES + 1;
    let err = active.vet().unwrap_err();
    expect(
        err,
        "heartbeat.load.active_file_bytes",
        Rule::OutOfRange,
        code,
    );

    let mut counters = load();
    counters.seq = u64::MAX;
    counters.mono_usec = u64::MAX;
    counters.cpu_usage_usec = u64::MAX;
    assert_eq!(counters.vet(), Ok(()));
}

/// Asserts the bounds of a minute record: minute 63 is kept and 64 dropped, the working
/// set at its cap is kept and one above dropped, a mean equal to the peak is kept and a
/// mean above it dropped, and each bad lease id form is dropped with its neighbours kept
/// and counted once. It catches an off-by-one at any of them and `<` for `<=`.
#[test]
fn minute_record_bounds() {
    let ok = minute("1-1", 0, 1, 1, 1);
    let cases = [
        (minute("1-2", TAIL_MINUTE, 5, 9, 1), true),
        (minute("1-2", TAIL_MINUTE + 1, 5, 9, 1), false),
        (minute("1-2", 0, 5, 9, MAX_MEMORY_BYTES), true),
        (minute("1-2", 0, 5, 9, MAX_MEMORY_BYTES + 1), false),
        (minute("1-2", 0, 9, 9, 1), true),
        (minute("1-2", 0, 10, 9, 1), false),
    ];
    for (record, kept) in cases {
        let got = vetted(with(
            vec![ok.clone(), record.clone(), ok.clone()],
            vec![],
            vec![],
        ));
        let (len, want) = if kept {
            (3, drops(0, 1, 0))
        } else {
            (1, drops(1, 1, 0))
        };
        assert_eq!(got.minutes.len(), len, "{record:?}");
        assert_eq!(got.dropped, want, "{record:?}");
    }
    for bad in BAD_IDS {
        let neighbours = vec![
            minute("1-1", 0, 1, 1, 1),
            minute(bad, 0, 1, 1, 1),
            minute("1-2", 0, 1, 1, 1),
        ];
        let got = vetted(with(neighbours, vec![], vec![]));
        assert_eq!(got.minutes.len(), 2, "{bad:?}");
        assert_eq!(got.dropped, drops(1, 0, 0), "{bad:?}");
    }
}

/// Asserts that a clock started at the reading is kept and one started after it dropped,
/// and that bad lease ids are dropped from both `clocks` and `covered`. It catches `<`
/// for `<=` and a list that skips the id grammar.
#[test]
fn clock_and_covered_bounds() {
    let now = load().mono_usec;
    let got = vetted(with(
        vec![],
        vec![clock("1-1", now), clock("1-2", now + 1), clock("1-3", 0)],
        vec![],
    ));
    assert_eq!(got.clocks, vec![clock("1-1", now), clock("1-3", 0)]);
    assert_eq!(got.dropped, drops(1, 0, 0));
    for bad in BAD_IDS {
        let got = vetted(with(
            vec![],
            vec![clock("1-1", 0), clock(bad, 0)],
            vec!["1-1".into(), bad.into(), "1-2".into()],
        ));
        assert_eq!(got.clocks.len(), 1, "{bad:?}");
        assert_eq!(got.covered, vec!["1-1".to_string(), "1-2".to_string()]);
        assert_eq!(got.dropped, drops(2, 0, 0), "{bad:?}");
    }
}

/// Asserts the duplicate keys: the same lease and minute twice keeps the first; the same
/// lease in two minutes and two leases in the same minute are all kept; a repeated clock
/// and a repeated covered id are dropped. It catches a key of the lease alone, a key of
/// the minute alone, and a duplicate that keeps the last.
#[test]
fn duplicates_are_dropped_by_the_right_key() {
    let got = vetted(with(
        vec![minute("1-1", 1, 1, 1, 10), minute("1-1", 1, 2, 2, 20)],
        vec![clock("1-1", 5), clock("1-1", 6)],
        vec!["1-1".into(), "1-1".into()],
    ));
    assert_eq!(got.minutes, vec![minute("1-1", 1, 1, 1, 10)]);
    assert_eq!(got.clocks, vec![clock("1-1", 5)]);
    assert_eq!(got.covered, vec!["1-1".to_string()]);
    assert_eq!(got.dropped, drops(0, 3, 0));

    let got = vetted(with(
        vec![
            minute("1-1", 1, 1, 1, 1),
            minute("1-1", 2, 1, 1, 1),
            minute("1-2", 1, 1, 1, 1),
        ],
        vec![],
        vec![],
    ));
    assert_eq!(got.minutes.len(), 3);
    assert_eq!(got.dropped, drops(0, 0, 0));
}

/// Asserts that a locally built load over a cap is cut to it, counting the excess, and
/// that a list at its cap is left alone. It catches a cap that `vet` does not enforce.
#[test]
fn vet_cuts_lists_to_their_caps() {
    let ids = |n: usize| (0..n).map(|i| format!("1-{i}")).collect::<Vec<_>>();
    let minutes = |n: usize| {
        ids(n)
            .iter()
            .map(|id| minute(id, 0, 1, 1, 1))
            .collect::<Vec<_>>()
    };
    let clocks = |n: usize| ids(n).iter().map(|id| clock(id, 0)).collect::<Vec<_>>();
    let got = vetted(with(
        minutes(MAX_LOAD_MINUTES + 5),
        clocks(MAX_LOAD_LEASES + 6),
        ids(MAX_LOAD_LEASES + 7),
    ));
    assert_eq!(got.minutes.len(), MAX_LOAD_MINUTES);
    assert_eq!(got.clocks.len(), MAX_LOAD_LEASES);
    assert_eq!(got.covered.len(), MAX_LOAD_LEASES);
    assert_eq!(got.dropped, drops(0, 0, 18));
    let at_cap = vetted(with(
        minutes(MAX_LOAD_MINUTES),
        clocks(MAX_LOAD_LEASES),
        ids(MAX_LOAD_LEASES),
    ));
    assert_eq!(at_cap.dropped, drops(0, 0, 0));
}

/// Asserts that `vet` twice gives what `vet` once gives, and counts nothing new the
/// second time. It catches counters that accumulate and a first pass that leaves work
/// for the second.
#[test]
fn vet_is_idempotent() {
    let mut messy = with(
        vec![
            minute("bad", 0, 1, 1, 1),
            minute("1-1", 0, 1, 1, 1),
            minute("1-1", 0, 1, 1, 1),
        ],
        vec![clock("1-1", u64::MAX), clock("1-2", 0), clock("1-2", 0)],
        vec!["x".into(), "1-1".into(), "1-1".into()],
    );
    assert_eq!(messy.vet(), Ok(()));
    let once = messy.clone();
    assert_eq!(once.dropped, drops(3, 3, 0));
    assert_eq!(messy.vet(), Ok(()));
    assert_eq!(messy, once);
}

/// Asserts that a heartbeat whose load fails `vet` still passes `validate`. It catches
/// `validate` calling `vet`, which would cost a beat for its telemetry.
#[test]
fn heartbeat_validate_ignores_load() {
    let mut bad = load();
    bad.working_set_bytes = u64::MAX;
    bad.minutes = vec![minute("not a lease", 9999, 9, 1, u64::MAX)];
    let beat = Heartbeat {
        load: Some(bad.clone()),
        ..heartbeat()
    };
    assert_eq!(beat.validate(), Ok(()));
    assert!(bad.vet().is_err());
}

/// Asserts that a megabyte in a lease id is dropped as one invalid record and is not
/// echoed, and that a scalar error stays a fixed short text. It catches an error or a
/// count built from the value.
#[test]
fn vet_errors_never_echo_input() {
    let big = long(1 << 20);
    assert_eq!(lease_parts(&big), None);
    let got = vetted(with(
        vec![minute(&big, 0, 1, 1, 1)],
        vec![clock(&big, 0)],
        vec![big.clone()],
    ));
    assert_eq!(got.dropped, drops(3, 0, 0));
    assert!(got.minutes.is_empty() && got.clocks.is_empty() && got.covered.is_empty());
    let mut ws = load();
    ws.working_set_bytes = u64::MAX;
    let text = ws.vet().unwrap_err().to_string();
    assert!(text.len() < 100 && !text.contains("aaaa"), "{text}");
}

/// Asserts that the total of drops adds every reason and does not overflow. It catches a
/// total that leaves a reason out or wraps.
#[test]
fn drops_total_adds_every_reason_and_saturates() {
    let all = LoadDrops {
        malformed: 1,
        invalid: 2,
        duplicate: 4,
        over_cap: 8,
    };
    assert_eq!(all.total(), 15);
    let max = LoadDrops {
        malformed: u32::MAX,
        invalid: u32::MAX,
        duplicate: 1,
        over_cap: 1,
    };
    assert_eq!(max.total(), u32::MAX);
}

/// The id at index `i` of the longest form: 41 bytes, all distinct.
fn worst_id(i: u64) -> String {
    format!("{}-{}", u64::MAX - i, u64::MAX)
}

/// Asserts the size of a heartbeat at every cap: a load with lists at their caps, every
/// id the longest form and every number at its digit maximum (and so untouched by
/// `vet`), 96 held runs and 8 results at 128-byte ids. The load must stay within 28 KiB
/// and the whole beat within 48 KiB, three quarters of `HEARTBEAT_BODY_BUDGET`. It
/// catches a cap raised (96 to 512), a longer id grammar and a longer key name. Fix the
/// caps, not this threshold. It proves the load is bounded by its caps; it does not
/// bound a beat with more held runs or results, which `validate` allows.
#[test]
fn a_load_at_every_cap_with_96_held_and_8_results_stays_under_the_budget() {
    let max = u32::MAX;
    let load = NodeLoad {
        seq: u64::MAX,
        mono_usec: u64::MAX,
        cpu_usage_usec: u64::MAX,
        working_set_bytes: MAX_MEMORY_BYTES,
        active_file_bytes: MAX_MEMORY_BYTES,
        actions_cpu_limited: true,
        covered: (0..MAX_LOAD_LEASES as u64).map(worst_id).collect(),
        clocks: (0..MAX_LOAD_LEASES as u64)
            .map(|i| clock(&worst_id(i), u64::MAX))
            .collect(),
        minutes: (0..MAX_LOAD_MINUTES as u64)
            .map(|i| minute(&worst_id(i), TAIL_MINUTE, max, max, MAX_MEMORY_BYTES))
            .collect(),
        dropped: LoadDrops::default(),
    };
    let mut checked = load.clone();
    assert_eq!(checked.vet(), Ok(()));
    assert_eq!(checked, load, "the maximal load must pass vet untouched");
    let wide = |i: usize| format!("{i:0>128}");
    let beat = Heartbeat {
        held: (0..MAX_LOAD_LEASES)
            .map(|i| HeldRun {
                run_id: wide(i),
                state: HeldState::ExitedUnacked,
            })
            .collect(),
        results: (0..8)
            .map(|i| RunResult {
                run_id: wide(i),
                lease_id: wide(i + 1000),
                outcome: Outcome::TimedOut,
                exit_code: None,
                detail: None,
            })
            .collect(),
        taint: None,
        load: Some(load.clone()),
        ..heartbeat()
    };
    assert_eq!(beat.validate(), Ok(()));
    let load_len = serde_json::to_string(&load).unwrap().len();
    let beat_len = serde_json::to_string(&beat).unwrap().len();
    assert!(load_len <= 28 * 1024, "load {load_len} bytes");
    assert!(beat_len <= 48 * 1024, "beat {beat_len} bytes");
    assert!(beat_len * 4 <= HEARTBEAT_BODY_BUDGET * 3);
}

/// Asserts the caps are the documented ones. It catches a silent edit of the table.
#[test]
fn cap_table_is_the_documented_one() {
    assert_eq!(MAX_LOAD_LEASES, 96);
    assert_eq!(MAX_LOAD_MINUTES, 64);
    assert_eq!(TAIL_MINUTE, 63);
    assert_eq!(MAX_LEASE_REF_LEN, 41);
    assert_eq!(HEARTBEAT_BODY_BUDGET, 65_536);
    assert_eq!(MAX_LEASE_REF_LEN, format!("{0}-{0}", u64::MAX).len());
}

/// Asserts that the heartbeat's Debug text shows the load and still hides the session.
/// It catches the load dropped from Debug, and the secret leaking with it.
#[test]
fn heartbeat_debug_shows_load_and_still_hides_session() {
    const SECRET: &str = "s3cret-DEBUG-9";
    let mut value = Heartbeat {
        load: Some(load()),
        ..heartbeat()
    };
    value.session = SECRET.into();
    value.load.as_mut().unwrap().seq = 424_242;
    let text = format!("{value:?}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("424242"), "{text}");
}
