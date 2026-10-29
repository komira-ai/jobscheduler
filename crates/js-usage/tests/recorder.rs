//! The recorder: what it accepts, what it refuses, and what it hands back.
//!
//! Every test says what it proves and the defect it would catch. None reads a clock: time
//! is the wall minute and the node's own microseconds, both given.

mod common;

use common::{host, lease};
use js_usage::recorder::{
    IMPLAUSIBLE_FACTOR, MAX_CLOCKS_PER_HOST, MAX_ENDED_LEASES, MAX_SEEN_PER_HOST, MINUTE_USEC,
    SEEN_RETENTION_MINUTES,
};
use js_usage::{
    ClockIn, DropReason, Limits, LoadIn, MinuteIn, NodeMinute, Observed, Recorder, TAIL_MINUTE,
};

const SEC: u64 = 1_000_000;
const BIG: Limits = Limits {
    threads: 8,
    memory_bytes: 64 << 30,
};
const UNKNOWN: Limits = Limits {
    threads: 0,
    memory_bytes: 0,
};

fn load(seq: u64, mono: u64, cpu: u64, ws: u64) -> LoadIn {
    LoadIn {
        seq,
        mono_usec: mono,
        cpu_usage_usec: cpu,
        working_set_bytes: ws,
        actions_cpu_limited: false,
        covered: Vec::new(),
        minutes: Vec::new(),
        clocks: Vec::new(),
    }
}

fn minute_in(l: u64, minute: u16, mean: u32, peak: u32, ws: u64) -> MinuteIn {
    MinuteIn {
        lease: lease(1, l),
        minute,
        mean_cores_milli: mean,
        peak_window_milli: peak,
        working_set_max_bytes: ws,
    }
}

fn with_minutes(mut l: LoadIn, minutes: Vec<MinuteIn>) -> LoadIn {
    l.minutes = minutes;
    l
}

/// A recorder whose node `h` has its baseline at seq 1, mono 100 s, cpu 0.
fn based() -> Recorder {
    let mut r = Recorder::new();
    let (first, _) = r.observe(&host("h"), 7, &load(1, 100 * SEC, 0, 0), BIG, 500);
    assert_eq!(first, Observed::Dropped(DropReason::NoBaseline));
    r
}

fn due(r: &mut Recorder, now: u64) -> Vec<NodeMinute> {
    r.node_minutes_due(now)
        .into_iter()
        .map(|(_, m)| m)
        .collect()
}

// Asserts the first reading of a node only sets a baseline: it adds no minute and the
// second one is the first counted. Catches a baseline of zero, which would book the whole
// counter since boot to one minute.
#[test]
fn the_first_reading_is_only_a_baseline() {
    let mut r = Recorder::new();
    let first = load(1, 100 * SEC, 9_000 * SEC, 5);
    assert_eq!(
        r.observe(&host("h"), 7, &first, BIG, 500).0,
        Observed::Dropped(DropReason::NoBaseline)
    );
    assert!(due(&mut r, 501).is_empty());
    let second = load(2, 110 * SEC, 9_005 * SEC, 6);
    assert_eq!(
        r.observe(&host("h"), 7, &second, BIG, 500).0,
        Observed::Accepted
    );
    let rows = due(&mut r, 501);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cpu_millis, 500);
}

// Asserts the node's CPU in a minute is the sum of CPU over the sum of time of its
// readings (500 and 1000 millicores over 10 s and 30 s give 875), not the mean of the
// per-reading rates (750) and not the last rate.
#[test]
fn node_cpu_is_total_cpu_over_total_time() {
    let mut r = based();
    let h = host("h");
    r.observe(&h, 7, &load(2, 110 * SEC, 5 * SEC, 1), BIG, 500);
    r.observe(&h, 7, &load(3, 140 * SEC, 35 * SEC, 1), BIG, 500);
    let rows = due(&mut r, 501);
    assert_eq!(rows[0].cpu_millis, 875);
}

// Asserts the working set of a minute is the largest reading, whatever order they came in
// (5, 9, 3 gives 9); catches "last reading" and "first reading".
#[test]
fn the_working_set_is_the_maximum() {
    let mut r = based();
    let h = host("h");
    for (seq, ws) in [(2, 5), (3, 9), (4, 3)] {
        r.observe(&h, 7, &load(seq, (100 + 10 * seq) * SEC, 0, ws), BIG, 500);
    }
    assert_eq!(due(&mut r, 501)[0].ws_max_bytes, 9);
}

// Asserts a wall minute is handed out once, only after it has ended, and in order; catches
// a minute handed out while still open and a minute handed out twice (a buffer that is not
// cleared).
#[test]
fn a_minute_is_due_once_after_it_ends() {
    let mut r = based();
    let h = host("h");
    r.observe(&h, 7, &load(2, 110 * SEC, 5 * SEC, 1), BIG, 500);
    r.observe(&h, 7, &load(3, 120 * SEC, 10 * SEC, 1), BIG, 501);
    assert!(due(&mut r, 500).is_empty());
    let first = r.node_minutes_due(501);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].0, h);
    assert_eq!(first[0].1.minute, 500);
    assert!(due(&mut r, 501).is_empty());
    let second = due(&mut r, 502);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].minute, 501);
    assert!(due(&mut r, 999).is_empty());
}

// Asserts minutes come out host by host, each host's in time order, and that two nodes
// with monotonic clocks hours apart do not disturb each other.
#[test]
fn nodes_with_different_clocks_are_independent() {
    let mut r = Recorder::new();
    let (a, b) = (host("a"), host("b"));
    let hours = 3600 * SEC;
    r.observe(&b, 1, &load(1, 40 * hours, 0, 0), BIG, 10);
    r.observe(&a, 1, &load(1, 5 * SEC, 0, 0), BIG, 10);
    r.observe(&b, 1, &load(2, 40 * hours + 10 * SEC, 10 * SEC, 1), BIG, 10);
    r.observe(&a, 1, &load(2, 15 * SEC, 5 * SEC, 1), BIG, 11);
    let rows = r.node_minutes_due(12);
    let shown: Vec<(String, u64, u32)> = rows
        .iter()
        .map(|(h, m)| (h.to_string(), m.minute, m.cpu_millis))
        .collect();
    assert_eq!(
        shown,
        vec![("a".to_owned(), 11, 500), ("b".to_owned(), 10, 1000)]
    );
}

// Asserts a reading whose sequence is not above the last accepted is refused (equal and
// below), leaves the baseline alone, and returns no lease minutes; catches a missing
// sequence check, where a replayed beat would book its window twice.
#[test]
fn a_sequence_that_does_not_rise_is_refused() {
    let mut r = based();
    let h = host("h");
    let ok = load(2, 110 * SEC, 5 * SEC, 1);
    assert_eq!(r.observe(&h, 7, &ok, BIG, 500).0, Observed::Accepted);
    let again = with_minutes(ok.clone(), vec![minute_in(1, 0, 1, 2, 3)]);
    let (seen, minutes) = r.observe(&h, 7, &again, BIG, 500);
    assert_eq!(seen, Observed::Dropped(DropReason::SeqNotAbove));
    assert!(minutes.is_empty());
    let older = load(1, 120 * SEC, 6 * SEC, 1);
    assert_eq!(
        r.observe(&h, 7, &older, BIG, 500).0,
        Observed::Dropped(DropReason::SeqNotAbove)
    );
    // The baseline is still the accepted reading at 110 s: 10 s later, 5 s more CPU.
    r.observe(&h, 7, &load(3, 120 * SEC, 10 * SEC, 1), BIG, 500);
    assert_eq!(due(&mut r, 501)[0].cpu_millis, 500);
}

// Asserts a monotonic clock that stands still or goes back refuses the reading and makes
// it the new baseline, so the next window is measured from it; catches a subtraction that
// wraps (a giant window) and a baseline stuck at the old reading.
#[test]
fn a_clock_that_does_not_advance_rebaselines() {
    let mut r = based();
    let h = host("h");
    for (seq, mono) in [(2, 100 * SEC), (3, 50 * SEC)] {
        let (seen, minutes) = r.observe(&h, 7, &load(seq, mono, 0, 1), BIG, 500);
        assert_eq!(seen, Observed::Dropped(DropReason::ClockBack));
        assert!(minutes.is_empty());
    }
    r.observe(&h, 7, &load(4, 60 * SEC, 5 * SEC, 1), BIG, 500);
    assert_eq!(due(&mut r, 501)[0].cpu_millis, 500);
}

// Asserts a CPU counter that goes back is refused and becomes the new baseline; catches
// `wrapping_sub` (a wrapped giant CPU) and a baseline that stays at the old, higher value
// (every later reading refused).
#[test]
fn a_counter_that_goes_back_rebaselines() {
    let mut r = based();
    let h = host("h");
    r.observe(&h, 7, &load(2, 110 * SEC, 50 * SEC, 1), BIG, 500);
    let (seen, minutes) = r.observe(
        &h,
        7,
        &with_minutes(
            load(3, 120 * SEC, 4 * SEC, 1),
            vec![minute_in(1, 0, 1, 2, 3)],
        ),
        UNKNOWN,
        500,
    );
    assert_eq!(seen, Observed::Dropped(DropReason::CounterBack));
    assert!(minutes.is_empty());
    let (next, _) = r.observe(&h, 7, &load(4, 130 * SEC, 9 * SEC, 1), BIG, 500);
    assert_eq!(next, Observed::Accepted);
    // 50 s of CPU over the first 10 s window and 5 s over the last: 55 s over 20 s.
    // The refused window (3) contributed nothing.
    assert_eq!(due(&mut r, 501)[0].cpu_millis, 2750);
}

// Asserts a reading implying exactly four times the threads is believed and one
// microsecond more is refused with the baseline left where it was; catches `>=`, a factor
// other than four, and a refused reading that moves the baseline.
#[test]
fn a_node_claiming_more_than_four_times_its_threads_is_refused() {
    assert_eq!(IMPLAUSIBLE_FACTOR, 4);
    let mut r = based();
    let h = host("h");
    let at_limit = 100 * SEC + 10 * SEC;
    // 8 threads x 4 = 32 cores for 10 s = 320 s of CPU.
    let lie = load(2, at_limit, 320 * SEC + 1, 1);
    assert_eq!(
        r.observe(&h, 7, &lie, BIG, 500).0,
        Observed::Dropped(DropReason::ImpliedCoresOverFourXThreads)
    );
    let exact = load(3, at_limit, 320 * SEC, 1);
    assert_eq!(r.observe(&h, 7, &exact, BIG, 500).0, Observed::Accepted);
    assert_eq!(due(&mut r, 501)[0].cpu_millis, 32_000);
}

// Asserts a working set of exactly four times the memory is believed and one byte more is
// refused; catches `>=` and a factor other than four.
#[test]
fn a_node_claiming_more_than_four_times_its_memory_is_refused() {
    let mut r = based();
    let h = host("h");
    let limit = Limits {
        threads: 8,
        memory_bytes: 1000,
    };
    assert_eq!(
        r.observe(&h, 7, &load(2, 110 * SEC, 0, 4001), limit, 500).0,
        Observed::Dropped(DropReason::MemoryOverFourXCapacity)
    );
    assert_eq!(
        r.observe(&h, 7, &load(3, 110 * SEC, 0, 4000), limit, 500).0,
        Observed::Accepted
    );
}

// Asserts a zero limit means "unknown" and skips its check, and that a saturated rate is
// clamped to u32::MAX rather than wrapped; catches treating zero as a real limit (every
// reading of a node of unknown size refused) and a narrowing cast.
#[test]
fn unknown_capacity_skips_the_checks() {
    let mut r = based();
    let h = host("h");
    let (seen, _) = r.observe(
        &h,
        7,
        &load(2, 100 * SEC + 1, u64::MAX, u64::MAX),
        UNKNOWN,
        500,
    );
    assert_eq!(seen, Observed::Accepted);
    let rows = due(&mut r, 501);
    assert_eq!(rows[0].cpu_millis, u32::MAX);
    assert_eq!(rows[0].ws_max_bytes, u64::MAX);
}

// Asserts a new epoch discards the node's state: the reading becomes the baseline (sequence
// numbers may restart), the old clocks and remembered minutes are gone, and that reading's
// lease minutes are still returned; catches an epoch change that keeps the baseline (the
// restarted agent's low sequence would be refused forever).
#[test]
fn an_epoch_change_starts_over() {
    let mut r = based();
    let h = host("h");
    let mut l = load(2, 110 * SEC, 5 * SEC, 1);
    l.clocks = vec![ClockIn {
        lease: lease(1, 1),
        started_mono_usec: 40 * SEC,
    }];
    l.minutes = vec![minute_in(1, 0, 1, 2, 3)];
    r.observe(&h, 7, &l, BIG, 500);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 1);
    let fresh = with_minutes(load(1, 3 * SEC, 0, 0), vec![minute_in(1, 0, 1, 2, 3)]);
    let (seen, minutes) = r.observe(&h, 8, &fresh, BIG, 501);
    assert_eq!(seen, Observed::Dropped(DropReason::EpochChanged));
    assert_eq!(
        minutes.len(),
        1,
        "the old epoch's memory of the minute is gone"
    );
    let (next, _) = r.observe(&h, 8, &load(2, 13 * SEC, SEC, 1), BIG, 501);
    assert_eq!(next, Observed::Accepted);
    assert_eq!(r.counters().dropped[&DropReason::EpochChanged], 1);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 0);
}

// Asserts the lease minutes of a first reading are returned though its counter has no
// baseline, and that a refused (untrusted) reading returns none; catches dropping them with
// the reading, which would lose every minute sent across a server restart.
#[test]
fn lease_minutes_ride_a_baseline_reading_but_not_a_refused_one() {
    let mut r = Recorder::new();
    let h = host("h");
    let first = with_minutes(load(1, 100 * SEC, 0, 0), vec![minute_in(1, 0, 1, 2, 3)]);
    assert_eq!(r.observe(&h, 7, &first, BIG, 500).1.len(), 1);
    let lie = with_minutes(
        load(2, 101 * SEC, 9_999 * SEC, 0),
        vec![minute_in(2, 0, 1, 2, 3)],
    );
    let (seen, minutes) = r.observe(&h, 7, &lie, BIG, 500);
    assert_eq!(
        seen,
        Observed::Dropped(DropReason::ImpliedCoresOverFourXThreads)
    );
    assert!(minutes.is_empty());
    let big = with_minutes(
        load(3, 102 * SEC, 0, 1 << 50),
        vec![minute_in(3, 0, 1, 2, 3)],
    );
    assert!(r.observe(&h, 7, &big, BIG, 500).1.is_empty());
}

// Asserts a resent minute is returned once; the same minute index of another lease and
// another minute of the same lease are both new. Catches a key of the minute alone, of the
// lease alone, and no deduplication at all.
#[test]
fn a_resent_lease_minute_is_not_counted_twice() {
    let mut r = based();
    let h = host("h");
    let batch = vec![
        minute_in(1, 3, 10, 20, 30),
        minute_in(2, 3, 11, 21, 31),
        minute_in(1, 4, 12, 22, 32),
    ];
    let (_, first) = r.observe(
        &h,
        7,
        &with_minutes(load(2, 110 * SEC, 0, 0), batch.clone()),
        BIG,
        500,
    );
    assert_eq!(first, batch);
    let (_, again) = r.observe(
        &h,
        7,
        &with_minutes(load(3, 120 * SEC, 0, 0), batch.clone()),
        BIG,
        500,
    );
    assert!(again.is_empty());
    assert_eq!(r.counters().minutes_deduped, 3);
    // A resend with different figures is still a resend: the first report stands.
    let changed = vec![minute_in(1, 3, 99, 99, 99)];
    let (_, none) = r.observe(
        &h,
        7,
        &with_minutes(load(4, 130 * SEC, 0, 0), changed),
        BIG,
        500,
    );
    assert!(none.is_empty());
}

// Asserts repeats of the tail minute (63, "63 or later") merge by the maximum of each
// field and are returned only when they raised one: (5, 9, 100) then (7, 8, 50) gives
// (7, 9, 100); then a lower one gives nothing. Catches first-wins, last-wins and a merge
// that returns an unchanged record.
#[test]
fn repeats_of_the_tail_minute_merge_by_maximum() {
    let mut r = based();
    let h = host("h");
    let send = |r: &mut Recorder, seq: u64, m: MinuteIn| {
        r.observe(
            &h,
            7,
            &with_minutes(load(seq, (100 + 10 * seq) * SEC, 0, 0), vec![m]),
            BIG,
            500,
        )
        .1
    };
    let tail = TAIL_MINUTE;
    assert_eq!(
        send(&mut r, 2, minute_in(1, tail, 5, 9, 100)),
        vec![minute_in(1, tail, 5, 9, 100)]
    );
    assert_eq!(
        send(&mut r, 3, minute_in(1, tail, 7, 8, 50)),
        vec![minute_in(1, tail, 7, 9, 100)]
    );
    assert!(send(&mut r, 4, minute_in(1, tail, 6, 9, 100)).is_empty());
    assert_eq!(
        send(&mut r, 5, minute_in(1, tail, 7, 9, 101)),
        vec![minute_in(1, tail, 7, 9, 101)]
    );
    assert_eq!(r.counters().minutes_deduped, 1);
}

// Asserts a minute past 63, which a valid agent never sends, is dropped and counted while
// 63 is kept; catches a bound of `>=` and a recorder that would key a series past 64.
#[test]
fn a_minute_past_the_tail_is_invalid() {
    let mut r = based();
    let h = host("h");
    let batch = vec![
        minute_in(1, 64, 1, 2, 3),
        minute_in(1, 63, 1, 2, 3),
        minute_in(1, u16::MAX, 1, 2, 3),
    ];
    let (_, fresh) = r.observe(
        &h,
        7,
        &with_minutes(load(2, 110 * SEC, 0, 0), batch),
        BIG,
        500,
    );
    assert_eq!(fresh, vec![minute_in(1, 63, 1, 2, 3)]);
    assert_eq!(r.counters().minutes_invalid, 2);
}

// Asserts the node's lease count is the distinct leases among covered and clocks (3 of
// [a, b] and [b, c]) and the most of any reading in the minute; catches counting with
// duplicates (4) and counting only one list.
#[test]
fn the_lease_count_is_distinct_and_the_busiest_reading() {
    let mut r = based();
    let h = host("h");
    let mut l = load(2, 110 * SEC, 0, 0);
    l.covered = vec![lease(1, 1), lease(1, 2)];
    l.clocks = vec![
        ClockIn {
            lease: lease(1, 2),
            started_mono_usec: 100 * SEC,
        },
        ClockIn {
            lease: lease(1, 3),
            started_mono_usec: 100 * SEC,
        },
    ];
    r.observe(&h, 7, &l, BIG, 500);
    r.observe(&h, 7, &load(3, 120 * SEC, 0, 0), BIG, 500);
    assert_eq!(due(&mut r, 501)[0].leases, 3);
}

// Asserts the minutes since a lease started are whole minutes of the node's own clock at its
// latest accepted reading, rounded down (59.999999 s is 0, 60 s is 1), and 0 for an unknown
// lease, an unknown node, and a start later than the reading; catches rounding up, a wall
// clock and a subtraction that wraps.
#[test]
fn elapsed_minutes_round_down() {
    let mut r = based();
    let h = host("h");
    let mut l = load(2, 160 * SEC - 1, 0, 0);
    l.clocks = vec![
        ClockIn {
            lease: lease(1, 1),
            started_mono_usec: 100 * SEC,
        },
        ClockIn {
            lease: lease(1, 2),
            started_mono_usec: 100 * SEC - 1,
        },
        ClockIn {
            lease: lease(1, 3),
            started_mono_usec: 200 * SEC,
        },
    ];
    r.observe(&h, 7, &l, BIG, 500);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 0);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 2)), 1);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 3)), 0);
    assert_eq!(r.elapsed_minute(&h, &lease(9, 9)), 0);
    assert_eq!(r.elapsed_minute(&host("nobody"), &lease(1, 1)), 0);
    assert_eq!(MINUTE_USEC, 60 * SEC);
    let mut later = load(3, 160 * SEC + 60 * SEC * 100, 0, 0);
    later.clocks = vec![];
    r.observe(&h, 7, &later, UNKNOWN, 500);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 101);
}

// Asserts a very old start saturates at u32::MAX minutes instead of wrapping.
#[test]
fn elapsed_minutes_saturate() {
    let mut r = Recorder::new();
    let h = host("h");
    let mut l = load(1, u64::MAX, 0, 0);
    l.clocks = vec![ClockIn {
        lease: lease(1, 1),
        started_mono_usec: 0,
    }];
    r.observe(&h, 1, &l, BIG, 500);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), u32::MAX);
}

// Asserts forgetting a node drops its state (its next reading is a baseline again, its
// clocks are gone, its open minute is lost) and leaves other nodes alone.
#[test]
fn forgetting_a_node_forgets_everything_about_it() {
    let mut r = based();
    let h = host("h");
    let other = host("other");
    r.observe(&other, 1, &load(1, SEC, 0, 0), BIG, 500);
    r.observe(&h, 7, &load(2, 110 * SEC, 5 * SEC, 1), BIG, 500);
    r.forget_host(&h);
    assert!(due(&mut r, 999).is_empty());
    assert_eq!(
        r.observe(&h, 7, &load(3, 120 * SEC, 6 * SEC, 1), BIG, 501)
            .0,
        Observed::Dropped(DropReason::NoBaseline)
    );
    assert_eq!(
        r.observe(&other, 1, &load(2, 2 * SEC, 0, 0), BIG, 501).0,
        Observed::Accepted
    );
    assert_eq!(r.counters().dropped_total(), 3);
}

// Asserts remembered minutes expire by age: still deduplicated 30 wall minutes after they
// were last seen, forgotten (and counted) at 31; catches `<` against `<=` on the age and an
// unbounded memory.
#[test]
fn a_remembered_minute_expires_by_age() {
    assert_eq!(SEEN_RETENTION_MINUTES, 30);
    let mut r = based();
    let h = host("h");
    let m = vec![minute_in(1, 3, 1, 2, 3)];
    r.observe(
        &h,
        7,
        &with_minutes(load(2, 110 * SEC, 0, 0), m.clone()),
        BIG,
        600,
    );
    r.observe(&h, 7, &load(3, 120 * SEC, 0, 0), BIG, 630);
    assert_eq!(r.counters().seen_evicted, 0);
    r.observe(&h, 7, &load(4, 130 * SEC, 0, 0), BIG, 631);
    assert_eq!(r.counters().seen_evicted, 1);
    let (_, again) = r.observe(&h, 7, &with_minutes(load(5, 140 * SEC, 0, 0), m), BIG, 631);
    assert_eq!(again.len(), 1);
}

// Asserts a resend refreshes a remembered minute's age (seen at 600, resent at 630, still
// remembered at 660); catches an age taken from the first sighting.
#[test]
fn a_resend_keeps_a_minute_remembered() {
    let mut r = based();
    let h = host("h");
    let m = vec![minute_in(1, 3, 1, 2, 3)];
    for (seq, wall) in [(2, 600), (3, 630), (4, 660)] {
        let l = with_minutes(load(seq, (100 + 10 * seq) * SEC, 0, 0), m.clone());
        let (_, fresh) = r.observe(&h, 7, &l, BIG, wall);
        assert_eq!(fresh.len(), usize::from(seq == 2));
    }
    assert_eq!(r.counters().seen_evicted, 0);
}

// Asserts the remembered minutes are capped per node: with the cap plus 64 records in one
// reading, the 64 smallest keys (the first lease's) are forgotten and counted, the rest
// still deduplicate. Catches an uncapped map.
#[test]
fn remembered_minutes_are_capped() {
    let mut r = based();
    let h = host("h");
    let leases = u64::try_from(MAX_SEEN_PER_HOST / 64).unwrap() + 1;
    let all: Vec<MinuteIn> = (0..leases)
        .flat_map(|l| (0..=63u16).map(move |m| minute_in(l, m, 1, 2, 3)))
        .collect();
    let (_, fresh) = r.observe(
        &h,
        7,
        &with_minutes(load(2, 110 * SEC, 0, 0), all),
        BIG,
        500,
    );
    assert_eq!(fresh.len(), MAX_SEEN_PER_HOST + 64);
    assert_eq!(r.counters().seen_evicted, 64);
    let probe = vec![minute_in(0, 5, 1, 2, 3), minute_in(leases - 1, 5, 1, 2, 3)];
    let (_, again) = r.observe(
        &h,
        7,
        &with_minutes(load(3, 120 * SEC, 0, 0), probe),
        BIG,
        500,
    );
    assert_eq!(again, vec![minute_in(0, 5, 1, 2, 3)]);
}

// Asserts lease clocks are bounded: aged out when not re-sent for over 30 minutes, and
// capped per node (the smallest lease ids go first); catches clocks kept forever.
#[test]
fn lease_clocks_are_bounded() {
    let mut r = based();
    let h = host("h");
    let mut l = load(2, 110 * SEC, 0, 0);
    l.clocks = vec![ClockIn {
        lease: lease(1, 1),
        started_mono_usec: 10 * SEC,
    }];
    r.observe(&h, 7, &l, BIG, 600);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 1);
    r.observe(&h, 7, &load(3, 120 * SEC, 0, 0), BIG, 630);
    assert_eq!(r.counters().clocks_evicted, 0);
    r.observe(&h, 7, &load(4, 130 * SEC, 0, 0), BIG, 631);
    assert_eq!(r.counters().clocks_evicted, 1);
    assert_eq!(r.elapsed_minute(&h, &lease(1, 1)), 0);

    let count = u64::try_from(MAX_CLOCKS_PER_HOST).unwrap() + 16;
    let mut many = load(5, 140 * SEC + 600 * SEC, 0, 0);
    many.clocks = (0..count)
        .map(|n| ClockIn {
            lease: lease(2, n),
            started_mono_usec: 100 * SEC,
        })
        .collect();
    r.observe(&h, 7, &many, UNKNOWN, 700);
    assert_eq!(r.counters().clocks_evicted, 17);
    assert_eq!(r.elapsed_minute(&h, &lease(2, 0)), 0);
    assert_eq!(r.elapsed_minute(&h, &lease(2, count - 1)), 10);
}

/// The readings both recorders in the leases-only tests are fed: a baseline, then
/// accepted readings in successive wall minutes, each carrying a lease minute and clock.
fn feed(r: &mut Recorder, beats: u64) -> Vec<(Observed, Vec<MinuteIn>)> {
    let h = host("h");
    let mut out = vec![r.observe(&h, 7, &load(1, 100 * SEC, 0, 0), BIG, 500)];
    for n in 0..beats {
        let mut l = load(n + 2, (110 + 10 * n) * SEC, (5 + 5 * n) * SEC, n + 1);
        l.minutes = vec![minute_in(n, 1, 2, 3, 4)];
        l.clocks = vec![ClockIn {
            lease: lease(1, n),
            started_mono_usec: 100 * SEC,
        }];
        out.push(r.observe(&h, 7, &l, BIG, 500 + n));
    }
    out
}

/// Readings that pass through every way a reading is refused, an epoch change, and
/// accepted readings between them, each carrying a lease minute and clock.
fn feed_refusals(r: &mut Recorder) -> Vec<(Observed, Vec<MinuteIn>)> {
    let h = host("h");
    let over_cpu = 6 * SEC + 10 * SEC * 8 * IMPLAUSIBLE_FACTOR + 1;
    let over_ws = (64u64 << 30) * IMPLAUSIBLE_FACTOR + 1;
    // (epoch, seq, mono s, cpu usec, working set)
    let steps = [
        (7, 1, 100, 0, 0),
        (7, 2, 110, 5 * SEC, 1),
        (7, 2, 115, 5 * SEC, 1),
        (7, 3, 110, 5 * SEC, 1),
        (7, 4, 120, 10 * SEC, 2),
        (7, 5, 130, SEC, 3),
        (7, 6, 140, 6 * SEC, 4),
        (7, 7, 150, over_cpu, 5),
        (7, 8, 150, 7 * SEC, over_ws),
        (7, 9, 160, 8 * SEC, 6),
        (8, 1, 5, 0, 0),
        (8, 2, 15, 3 * SEC, 7),
    ];
    let mut out = Vec::new();
    for (n, (epoch, seq, mono, cpu, ws)) in (0u64..).zip(steps) {
        let mut l = load(seq, mono * SEC, cpu, ws);
        l.minutes = vec![minute_in(n, 1, 2, 3, 4)];
        l.clocks = vec![ClockIn {
            lease: lease(1, n),
            started_mono_usec: 2 * SEC,
        }];
        out.push(r.observe(&h, epoch, &l, BIG, 500 + n));
    }
    out
}

// Asserts a leases-only recorder answers every reading as a normal one does, the
// observed verdict, the lease minutes, the lease clocks and the counters alike, over
// accepted readings, each refusal and an epoch change; catches a leases-only mode that
// changes a drop path, a counter or the lease side.
#[test]
fn leases_only_gives_the_same_lease_output() {
    let mut normal = Recorder::new();
    let mut lean = Recorder::leases_only();
    let seen = feed_refusals(&mut normal);
    assert_eq!(seen, feed_refusals(&mut lean));
    for reason in [
        DropReason::NoBaseline,
        DropReason::EpochChanged,
        DropReason::SeqNotAbove,
        DropReason::ClockBack,
        DropReason::CounterBack,
        DropReason::ImpliedCoresOverFourXThreads,
        DropReason::MemoryOverFourXCapacity,
    ] {
        assert!(
            seen.iter().any(|(o, _)| *o == Observed::Dropped(reason)),
            "{reason:?} not reached"
        );
    }
    assert!(seen.iter().any(|(o, _)| *o == Observed::Accepted));
    for n in 0..12 {
        assert_eq!(
            normal.elapsed_minute(&host("h"), &lease(1, n)),
            lean.elapsed_minute(&host("h"), &lease(1, n)),
        );
    }
    assert_eq!(normal.counters(), lean.counters());
}

// Asserts a leases-only recorder never hands out a node minute, however many readings it
// took and however late it is asked; catches node minutes still being booked.
#[test]
fn leases_only_gives_no_node_minutes() {
    let mut r = Recorder::leases_only();
    feed(&mut r, 200);
    assert!(r.node_minutes_due(500).is_empty());
    assert!(r.node_minutes_due(701).is_empty());
    assert!(r.node_minutes_due(u64::MAX).is_empty());
}

// Asserts a leases-only recorder holds no node-minute state after many readings while a
// normal one holds one per wall minute; catches state that grows with the readings.
#[test]
fn leases_only_holds_no_node_minute_state() {
    let mut normal = Recorder::new();
    let mut lean = Recorder::leases_only();
    feed(&mut normal, 200);
    feed(&mut lean, 200);
    assert_eq!(normal.held_node_minutes(), 200);
    assert_eq!(lean.held_node_minutes(), 0);
}

/// Both kinds of recorder, with node `h` based, for the lease-ended tests.
fn both() -> [Recorder; 2] {
    let mut lean = Recorder::leases_only();
    lean.observe(&host("h"), 7, &load(1, 100 * SEC, 0, 0), BIG, 500);
    [based(), lean]
}

/// Feeds node `h` reading `seq` (mono 100 s + 10 s per seq) carrying `minutes`.
fn give(r: &mut Recorder, seq: u64, minutes: Vec<MinuteIn>) -> Vec<MinuteIn> {
    let l = with_minutes(load(seq, 100 * SEC + seq * 10 * SEC, 0, 0), minutes);
    r.observe(&host("h"), 7, &l, BIG, 500).1
}

// Asserts ending a lease drops its remembered minutes at once and leaves the other
// leases' minutes remembered and still deduplicating; catches an end that clears too much
// or nothing.
#[test]
fn an_ended_lease_forgets_its_minutes_and_others_keep_theirs() {
    for mut r in both() {
        give(
            &mut r,
            2,
            vec![minute_in(1, 3, 1, 2, 3), minute_in(1, 4, 1, 2, 3)],
        );
        give(&mut r, 3, vec![minute_in(2, 3, 1, 2, 3)]);
        assert_eq!(r.remembered_minutes(&lease(1, 1)), 2);
        r.lease_ended(&lease(1, 1));
        assert_eq!(r.remembered_minutes(&lease(1, 1)), 0);
        assert_eq!(r.remembered_minutes(&lease(1, 2)), 1);
        assert!(give(&mut r, 4, vec![minute_in(2, 3, 1, 2, 3)]).is_empty());
        assert_eq!(r.counters().minutes_deduped, 1);
    }
}

// Asserts ending a lease forgets its minutes on every node; catches an end that only
// reaches one host.
#[test]
fn an_ended_lease_is_forgotten_on_every_host() {
    let mut r = based();
    let other = host("g");
    r.observe(&other, 7, &load(1, 100 * SEC, 0, 0), BIG, 500);
    give(&mut r, 2, vec![minute_in(1, 3, 1, 2, 3)]);
    let l = with_minutes(load(2, 110 * SEC, 0, 0), vec![minute_in(1, 3, 1, 2, 3)]);
    r.observe(&other, 7, &l, BIG, 500);
    assert_eq!(r.remembered_minutes(&lease(1, 1)), 2);
    r.lease_ended(&lease(1, 1));
    assert_eq!(r.remembered_minutes(&lease(1, 1)), 0);
}

// Asserts a late reading naming an ended lease adds no minute and is counted, while the
// same reading's minutes of other leases pass; catches a late repeat counted as new.
#[test]
fn a_late_minute_of_an_ended_lease_is_refused_and_counted() {
    for mut r in both() {
        give(&mut r, 2, vec![minute_in(1, 3, 1, 2, 3)]);
        r.lease_ended(&lease(1, 1));
        let late = give(
            &mut r,
            3,
            vec![
                minute_in(1, 3, 1, 2, 3),
                minute_in(1, 9, 1, 2, 3),
                minute_in(2, 3, 1, 2, 3),
            ],
        );
        assert_eq!(late, vec![minute_in(2, 3, 1, 2, 3)]);
        assert_eq!(r.counters().minutes_after_end, 2);
        assert_eq!(r.remembered_minutes(&lease(1, 1)), 0);
    }
}

// Asserts ending a lease the recorder holds nothing for forgets nothing and disturbs no
// other lease; catches a panic or a stray removal.
#[test]
fn ending_an_unknown_lease_changes_nothing_else() {
    for mut r in both() {
        give(&mut r, 2, vec![minute_in(2, 3, 1, 2, 3)]);
        r.lease_ended(&lease(9, 9));
        assert_eq!(r.remembered_minutes(&lease(1, 2)), 1);
        assert_eq!(r.counters().seen_evicted, 0);
        assert_eq!(r.counters().minutes_after_end, 0);
        assert!(give(&mut r, 3, vec![minute_in(2, 3, 1, 2, 3)]).is_empty());
    }
}

// Asserts the ended-lease memory is bounded: ending more leases than the bound keeps the
// bound, ending one twice does not take a second place, and the oldest ended lease is the
// one forgotten (its minute counts as new again; the next oldest and the newest are still refused); catches an
// unbounded set or the wrong end forgotten.
#[test]
fn the_ended_lease_memory_is_bounded_oldest_first() {
    let mut r = based();
    let max = u64::try_from(MAX_ENDED_LEASES).unwrap();
    for n in 0..max + 3 {
        r.lease_ended(&lease(1, n + 1000));
        r.lease_ended(&lease(1, n + 1000));
    }
    assert_eq!(r.remembered_ended(), MAX_ENDED_LEASES);
    let probe = |l: u64| MinuteIn {
        lease: lease(1, l),
        minute: 1,
        mean_cores_milli: 1,
        peak_window_milli: 2,
        working_set_max_bytes: 3,
    };
    let out = give(
        &mut r,
        2,
        vec![probe(1000), probe(1003), probe(1000 + max + 2)],
    );
    assert_eq!(out, vec![probe(1000)]);
    assert_eq!(r.counters().minutes_after_end, 2);
}
