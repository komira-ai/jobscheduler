use super::*;

/// One window, in microseconds of the agent's clock and milliseconds of the scheduler's.
const W_USEC: u64 = 5_000_000;

/// A core-second, in microseconds.
const CORE_SEC: u64 = 1_000_000;

/// The scheduler's time at window `i`.
fn at(i: u64) -> FarmTime {
    FarmTime::from_millis(i * 5_000)
}

/// Half a window after window `i`: when a lease is admitted or ends between two readings.
fn between(i: u64) -> FarmTime {
    FarmTime::from_millis(i * 5_000 + 2_500)
}

fn lease(n: u64) -> LeaseId {
    LeaseId::new(1, n)
}

/// A worker of 16 cores whose readings arrive every window, the counter at `cpu` each.
struct Rig {
    check: Check,
    counters: LoadCounters,
    next: u64,
}

impl Rig {
    /// A rig whose baseline reading, window 0, has the counter at 0.
    fn new() -> Self {
        let mut check = Check::default();
        check.baseline(Mark::new(at(0), 0, 0), false);
        Self {
            check,
            counters: LoadCounters::default(),
            next: 1,
        }
    }

    /// The next reading, with the counter at `cpu` and `busy` millicores busy over its
    /// window; `held` is whether the worker holds a lease now.
    fn read(&mut self, cpu: u64, busy: u32, held: bool) {
        let i = self.next;
        self.next += 1;
        let mark = Mark::new(at(i), i * W_USEC, cpu);
        self.check
            .window(mark, (busy, 16_000), held, &mut self.counters);
    }

    /// A lease admitted between windows `from` and `from + 1` that ended between windows
    /// `to` and `to + 1` and used `cpu` microseconds.
    fn ran(&mut self, n: u64, (from, to): (u64, u64), cpu: u64) {
        self.check
            .ended(lease(n), Some(between(from)), Some(cpu), between(to));
    }
}

/// Catches: the comparison's threshold off by one either way (half the sum agrees, a
/// microsecond less is implausible), or the growth compared with the whole sum.
#[test]
fn growth_of_half_the_lease_cpu_agrees_and_a_microsecond_less_does_not() {
    let mut r = Rig::new();
    r.ran(1, (0, 1), 60 * CORE_SEC);
    r.read(10 * CORE_SEC, 0, false);
    r.read(30 * CORE_SEC, 0, false);
    assert!(!r.check.implausible());
    assert_eq!(r.counters.signal_checked_windows, 1);
    assert_eq!(r.counters.signal_unchecked_windows, 1);

    let mut r = Rig::new();
    r.ran(1, (0, 1), 60 * CORE_SEC);
    r.read(10 * CORE_SEC, 0, false);
    r.read(30 * CORE_SEC - 1, 0, false);
    assert!(r.check.implausible());
    assert!(r.check.lease_cpu_implausible());
    assert!(!r.check.idle_implausible());
    assert_eq!(r.counters.signal_implausible_lease_cpu, 1);
}

/// Catches: a span compared below 60 core-seconds (a microsecond short is unchecked, not
/// failed, however little the counter grew), and the unchecked time not counted.
#[test]
fn a_span_short_of_sixty_core_seconds_is_unchecked_and_its_time_counted() {
    let mut r = Rig::new();
    r.ran(1, (0, 1), 60 * CORE_SEC - 1);
    r.read(0, 0, false);
    r.read(0, 0, false);
    assert!(!r.check.implausible());
    assert_eq!(r.counters.signal_checked_windows, 0);
    assert_eq!(r.counters.signal_unchecked_windows, 2);
    assert_eq!(r.counters.signal_unchecked_usec, 2 * W_USEC);
    assert_eq!(r.check.unchecked_usec(), 2 * W_USEC);
}

/// Catches: the span not extended back window by window (only the newest window compared,
/// which holds too little), or extended past the nearest mark that holds 60 core-seconds
/// (the growth taken from too early a mark, which a stuck counter's early growth hides).
#[test]
fn the_span_extends_back_to_the_nearest_reading_that_holds_sixty_core_seconds() {
    let mut r = Rig::new();
    // The counter grows 100 core-seconds in the first window and is stuck after.
    r.read(100 * CORE_SEC, 0, false);
    r.ran(1, (1, 2), 30 * CORE_SEC);
    r.ran(2, (2, 3), 30 * CORE_SEC);
    r.read(100 * CORE_SEC, 0, false);
    r.read(100 * CORE_SEC, 0, false);
    assert!(!r.check.implausible(), "30 core-seconds: unchecked");
    r.read(100 * CORE_SEC, 0, false);
    // From window 1 the span holds both leases (60) and the counter grew by 0; from window 0
    // it grew by 100, which would agree.
    assert!(r.check.implausible());
    assert_eq!(r.counters.signal_unchecked_windows, 3);
    assert_eq!(r.check.unchecked_usec(), 0);
}

/// Catches: a lease counted in a span when it was admitted the moment the span's first
/// reading arrived (its run may have begun before the reading: a long lease's CPU would land
/// in a short span and fail a healthy node), and a lease that ended the very moment the
/// newest reading arrived counted in that span.
#[test]
fn only_leases_admitted_after_the_first_reading_and_ended_before_the_newest_count() {
    let mut r = Rig::new();
    // 800 core-seconds, admitted the moment the baseline arrived; a counter that grows 5
    // core-seconds a window.
    r.check.ended(
        lease(1),
        Some(FarmTime::from_millis(0)),
        Some(800 * CORE_SEC),
        between(1),
    );
    r.read(5 * CORE_SEC, 0, false);
    r.read(10 * CORE_SEC, 0, false);
    assert!(!r.check.implausible());
    assert_eq!(r.counters.signal_checked_windows, 0);

    let mut r = Rig::new();
    r.read(0, 0, false);
    // Ended the moment window 3 arrived.
    r.check
        .ended(lease(2), Some(between(1)), Some(60 * CORE_SEC), at(3));
    r.read(0, 0, false);
    r.read(0, 0, false);
    assert_eq!(
        r.counters.signal_checked_windows, 0,
        "not in the span to window 3"
    );
    r.read(0, 0, false);
    assert!(r.check.implausible());
}

/// Catches: a span longer than an hour (a lease admitted after a reading more than an hour
/// before the newest is compared), and readings or leases kept past it.
#[test]
fn a_span_is_at_most_an_hour_of_the_agents_clock() {
    let mut check = Check::default();
    let mut counters = LoadCounters::default();
    let hour = SPAN_MAX_USEC;
    check.baseline(Mark::new(FarmTime::from_millis(0), 0, 0), false);
    check.ended(
        lease(1),
        Some(FarmTime::from_millis(1)),
        Some(60 * CORE_SEC),
        FarmTime::from_millis(2),
    );
    // Exactly an hour after the baseline: the span from it is an hour long, and counts.
    let mut probe = check.clone();
    probe.window(
        Mark::new(FarmTime::from_millis(3_600_000), hour, 0),
        (0, 16_000),
        false,
        &mut counters,
    );
    assert!(probe.implausible());
    // A microsecond more: the baseline is let go, and the lease with it.
    check.window(
        Mark::new(FarmTime::from_millis(3_600_000), hour + 1, 0),
        (0, 16_000),
        false,
        &mut counters,
    );
    assert!(!check.implausible());
    assert_eq!(check.marks.len(), 1);
    assert!(check.runs.is_empty());
}

/// Catches: a lease that left the ledger before its CPU time arrived never counted, or
/// counted as ended when it left rather than when its CPU arrived (a run whose CPU is
/// reported after the next reading must wait for the one after), a second report of it
/// counted again, and a report for a lease the check does not wait for taken.
#[test]
fn cpu_time_that_arrives_after_the_lease_ended_counts_from_its_arrival() {
    let mut r = Rig::new();
    r.check.ended(lease(1), Some(between(0)), None, between(0));
    r.read(0, 0, false);
    assert!(r.check.late_cpu(lease(1), 60 * CORE_SEC, at(2)));
    assert!(!r.check.late_cpu(lease(1), 60 * CORE_SEC, at(2)));
    assert!(!r.check.late_cpu(lease(9), 60 * CORE_SEC, at(2)));
    // It arrived at the moment window 2 did: not before it.
    r.read(0, 0, false);
    assert!(!r.check.implausible());
    r.read(0, 0, false);
    assert!(r.check.implausible());
    assert_eq!(r.check.runs.len(), 1);
}

/// Catches: a recovered lease (no admission time: its run may have started long before the
/// span) counted.
#[test]
fn a_lease_with_no_admission_time_is_not_counted() {
    let mut r = Rig::new();
    r.check
        .ended(lease(1), None, Some(600 * CORE_SEC), between(0));
    r.check.ended(lease(2), None, None, between(0));
    assert!(r.check.runs.is_empty());
    assert!(r.check.awaiting.is_empty());
    assert!(!r.check.late_cpu(lease(2), 600 * CORE_SEC, between(0)));
}

/// Catches: a mark never cleared once the counter agrees again, cleared by a window that
/// compares nothing, or counted again while it stands; and the unchecked time not reset by
/// a window that was checked.
#[test]
fn a_mark_holds_through_unchecked_windows_and_clears_when_the_counter_agrees() {
    let mut r = Rig::new();
    r.ran(1, (0, 1), 60 * CORE_SEC);
    r.read(0, 0, false);
    r.read(0, 0, false);
    assert!(r.check.implausible());
    r.read(0, 0, false);
    assert_eq!(r.counters.signal_implausible_lease_cpu, 1, "counted once");
    // The baseline and lease 1 drop out after an hour; windows with nothing to compare.
    let mut r2 = Rig::new();
    r2.check = r.check.clone();
    let mut i = r.next;
    let mut cpu = 0;
    while i * W_USEC <= SPAN_MAX_USEC + W_USEC {
        r2.check.window(
            Mark::new(at(i), i * W_USEC, cpu),
            (0, 16_000),
            false,
            &mut r2.counters,
        );
        i += 1;
    }
    assert!(r2.check.implausible(), "unchecked is not agreeing");
    assert!(r2.check.unchecked_usec() > 0);
    // A lease whose CPU the counter now shows.
    r2.check.ended(
        lease(2),
        Some(between(i - 1)),
        Some(60 * CORE_SEC),
        between(i),
    );
    cpu += 60 * CORE_SEC;
    for _ in 0..2 {
        i += 1;
        r2.check.window(
            Mark::new(at(i), i * W_USEC, cpu),
            (0, 16_000),
            false,
            &mut r2.counters,
        );
    }
    assert!(!r2.check.implausible());
    assert_eq!(r2.check.unchecked_usec(), 0);
}

/// Catches: the idle rule marking before the third window, or at exactly half the cores
/// ("more than half"); a window with a lease not ending the run; an idle window at half
/// or below not clearing the mark; and a mark counted again while it stands.
#[test]
fn an_idle_worker_over_half_its_cores_for_three_windows_is_marked() {
    let mut r = Rig::new();
    r.read(0, 8_001, false);
    r.read(0, 8_001, false);
    assert!(!r.check.implausible());
    r.read(0, 8_001, false);
    assert!(r.check.idle_implausible());
    assert!(!r.check.lease_cpu_implausible());
    r.read(0, 16_000, false);
    assert_eq!(r.counters.signal_implausible_idle, 1);
    // A window with a lease says nothing about the mark.
    r.check.lease_present();
    r.read(0, 0, false);
    assert!(r.check.implausible());
    r.read(0, 8_000, false);
    assert!(!r.check.implausible(), "half is not more than half");

    // Two high, a busy window, two high: no run of three.
    let mut r = Rig::new();
    r.read(0, 9_000, false);
    r.read(0, 9_000, true);
    r.read(0, 9_000, false);
    r.read(0, 9_000, false);
    r.read(0, 9_000, false);
    assert!(
        !r.check.implausible(),
        "a window that began with a lease is not idle"
    );
    r.read(0, 9_000, false);
    assert!(r.check.implausible());
}

/// Catches: a window idle at both readings counted idle though a lease was placed and
/// ended inside it, and a worker with no cores marked by the idle rule.
#[test]
fn a_lease_inside_a_window_makes_it_not_idle_and_no_cores_marks_nothing() {
    let mut r = Rig::new();
    for _ in 0..3 {
        r.check.lease_present();
        r.read(0, 9_000, false);
    }
    assert!(!r.check.implausible());

    let mut check = Check::default();
    let mut counters = LoadCounters::default();
    check.baseline(Mark::new(at(0), 0, 0), false);
    for i in 1..=3 {
        check.window(
            Mark::new(at(i), i * W_USEC, 0),
            (1, 0),
            false,
            &mut counters,
        );
    }
    assert!(!check.implausible());
}

/// Catches: a new session that keeps the old session's readings or leases (its counter
/// started again, so a span across sessions compares two counters), clears a mark (a
/// restarted agent believed at once), or keeps the idle run.
#[test]
fn a_new_session_starts_the_spans_again_and_keeps_the_mark() {
    let mut r = Rig::new();
    r.ran(1, (0, 1), 60 * CORE_SEC);
    r.check.ended(lease(2), Some(between(0)), None, between(1));
    r.read(0, 0, false);
    r.read(0, 0, false);
    assert!(r.check.implausible());
    r.read(0, 9_000, false);
    r.read(0, 9_000, false);
    r.check.new_session();
    assert!(r.check.marks.is_empty());
    assert!(r.check.runs.is_empty());
    assert!(r.check.awaiting.is_empty());
    assert_eq!(r.check.idle_high, 0);
    assert!(r.check.implausible());
}

/// Catches: a worker with no accepted reading (one that sends none, or whose readings are
/// all dropped) keeping every lease that ends on it, which nothing would ever let go.
#[test]
fn a_worker_with_no_reading_keeps_no_lease() {
    let mut check = Check::default();
    check.ended(lease(1), Some(between(0)), Some(60 * CORE_SEC), between(1));
    check.ended(lease(2), Some(between(0)), None, between(1));
    assert!(check.runs.is_empty());
    assert!(check.awaiting.is_empty());
    assert!(!check.late_cpu(lease(2), 60 * CORE_SEC, between(2)));
}
