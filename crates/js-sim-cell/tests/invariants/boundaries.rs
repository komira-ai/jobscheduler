//! Boundaries of the invariants: sums past the largest resource count, zero
//! capacity, a duplicate that outlives the agent first indexed, a repeated identical
//! answer, a host the server does not list, an escaped run id, the end of a fault and a
//! quiet window past the largest time. Uses the helpers of the parent file.

use super::*;

/// Resources of `n` on one booked axis (0 cpu, 1 memory, 2 gpus) and nothing else.
fn on_axis(axis: usize, n: u64) -> Resources {
    match axis {
        0 => res(n, 0),
        1 => res(0, n),
        _ => res_gpus(0, 0, n),
    }
}

/// Host 0 of `capacity` running run 0 and run 1 of leases 0 and 1.
fn two_runs_on(capacity: Resources, first: Resources, second: Resources) -> Observations {
    let agent = AgentTruth::new(capacity)
        .with_run(run(0), lease(0), first)
        .with_run(run(1), lease(1), second);
    Observations::quiet().at(ms(300)).with_agent(host(0), agent)
}

/// Asserts that runs whose needs sum past `u64::MAX` on a host of `u64::MAX` on every
/// axis are flagged, on each axis, with the true total in the line; and that runs
/// summing to exactly `u64::MAX` are not. Catches a sum that saturates (or wraps) and so
/// reads as within the capacity it exceeds.
#[test]
fn runs_whose_needs_sum_past_the_largest_capacity_are_flagged() {
    let max = u64::MAX;
    let largest = || res_gpus(max, max, max);
    let found = step_all(&two_runs_on(largest(), on_axis(0, max), on_axis(0, 1)));
    assert_eq!(
        lines(&found),
        [
            "no-double-booking at 300: host host-0 runs [run \"run-0\"; run \"run-1\"] needing \
             res(cpu=18446744073709551616 mem=0 gpus=0) beyond its capacity \
             res(cpu=18446744073709551615 mem=18446744073709551615 gpus=18446744073709551615) \
             (over on cpu)"
        ]
    );
    for (axis, name) in [(0, "cpu"), (1, "mem"), (2, "gpus")] {
        let over = two_runs_on(largest(), on_axis(axis, max), on_axis(axis, 1));
        let found = step_all(&over);
        assert_eq!(flagged(&found), [Invariant::NoDoubleBooking], "{name}");
        assert!(
            found[0].detail.ends_with(&format!("(over on {name})")),
            "{}",
            found[0]
        );
        let exact = two_runs_on(largest(), on_axis(axis, max - 1), on_axis(axis, 1));
        none(&step_all(&exact));
    }
}

/// Asserts the zero boundaries of capacity: an idle agent of no capacity and a run that
/// needs nothing on it pass, and a run needing one unit of any axis on it is flagged for
/// that axis. Catches a check that treats a zero capacity as unlimited, or that flags a
/// zero need.
#[test]
fn zero_capacity_and_zero_needs_are_boundaries() {
    let zero = || Resources::new(0, 0, 0, None).expect("resources");
    let idle = Observations::quiet().with_agent(host(0), AgentTruth::new(zero()));
    none(&step_all(&idle));
    none(&step_all(&two_runs_on(zero(), zero(), zero())));
    for (axis, name) in [(0, "cpu"), (1, "mem"), (2, "gpus")] {
        let found = step_all(&two_runs_on(zero(), zero(), on_axis(axis, 1)));
        assert_eq!(flagged(&found), [Invariant::NoDoubleBooking], "{name}");
        assert!(
            found[0].detail.ends_with(&format!("(over on {name})")),
            "{}",
            found[0]
        );
    }
}

/// Asserts that a run double-booked on hosts 0 and 1 is still found when host 0's agent
/// goes away and host 2 starts the run at a step touching only host 2 (hosts 1 and 2
/// then run it together), and that once host 1 stops it, host 2 alone is not flagged.
/// Catches an index that keeps one owner per run and drops the surviving duplicate
/// when the owner it kept goes away.
#[test]
fn a_duplicate_that_outlives_the_first_agent_is_still_flagged() {
    let mut checker = Checker::new();
    let only = |h: u64| Touched::nothing().agent(host(h));
    let on_zero = Observations::quiet()
        .at(ms(100))
        .with_agent(host(0), runs_lease_zero());
    none(&checker.step(&on_zero, &only(0)));
    let on_one = on_zero.with_agent(host(1), runs_lease_zero()).at(ms(200));
    assert_eq!(
        flagged(&checker.step(&on_one, &only(1))),
        [Invariant::NoDoubleBooking]
    );
    let mut gone = on_one.at(ms(300));
    gone.agents.remove(&host(0));
    none(&checker.step(&gone, &only(0)));
    let on_two = gone.with_agent(host(2), runs_lease_zero()).at(ms(400));
    assert_eq!(
        lines(&checker.step(&on_two, &only(2))),
        ["no-double-booking at 400: run \"run-0\" runs on host host-1 and on host host-2"]
    );
    let alone = on_two
        .with_agent(host(1), AgentTruth::new(capacity()))
        .at(ms(500));
    none(&checker.step(&alone, &only(1)));
    none(&checker.step(&alone, &only(2)));
}

/// Asserts that the same terminal answer heard again at a later step, at the same
/// virtual time, is flagged by the checker that saw the first: the order heard decides
/// which answer is second. Catches a check that compares answers by value or by time.
#[test]
fn the_same_terminal_answer_heard_again_is_flagged() {
    let mut checker = Checker::new();
    let touched = Touched::nothing().item(ItemId(0));
    let first = item_ended(HowEnded::Succeeded);
    none(&checker.step(&first, &touched));
    let again = first.with_event(ItemId(0), ms(1_000), ended(HowEnded::Succeeded));
    assert_eq!(
        lines(&checker.step(&again, &touched)),
        [
            "one-terminal-answer at 1500: item0 heard terminal(succeeded) at 1000 after \
             terminal(succeeded) at 1000"
        ]
    );
}

/// Asserts that a run on an agent whose host is missing from the server's listing
/// altogether (a host the server forgot) is flagged at the end with the same line as a
/// listed host that books nothing. Catches a leak detector that only reads the hosts
/// the server lists.
#[test]
fn a_run_on_a_host_the_server_does_not_list_is_flagged() {
    let mut unlisted = run_nothing_books();
    unlisted.server.hosts.clear();
    assert_eq!(
        lines(&end(&unlisted)),
        [
            "bookings-match-agents at 5000: host host-0 runs run \"run-0\" of lease 1.0 and \
             the server books no such lease there"
        ]
    );
}

/// Asserts that a run id holding a quote, a backslash, a newline and a non-ASCII letter
/// is written escaped, so the failure stays one line. Catches a run id written raw,
/// which would break a failure (and a trace) line in two.
#[test]
fn a_run_id_is_escaped_so_a_failure_stays_one_line() {
    let odd = RunId::new("a\"b\\c\nd \u{e9}");
    let twice = Observations::quiet()
        .at(ms(300))
        .with_launch(odd.clone(), host(0), ms(100))
        .with_launch(odd, host(1), ms(200));
    let found = lines(&step_all(&twice));
    assert_eq!(
        found,
        [
            r#"at-most-one-launch at 300: run "a\"b\\c\nd \u{e9}" launched on host host-1 at 200, already launched on host host-0 at 100"#
        ]
    );
    assert!(!found[0].contains('\n'));
}

/// Asserts that observations that had a fault and then were marked quiet since a time
/// (`quiet_since`) are no longer faulted: the end check refuses until `settle_within`
/// after that time and runs from it. Catches a builder that leaves the fault active, so
/// a recovered run could never be checked at its end.
#[test]
fn an_end_check_runs_after_a_fault_ends() {
    let recovered = unfinished_at_end()
        .with_fault()
        .quiet_since(ms(1_000))
        .at(ms(1_500));
    let expected = NotSettled {
        at: ms(1_500),
        fault_active: false,
        settles_at: ms(2_000),
    };
    assert_eq!(Checker::end(&recovered, SETTLE), Err(expected));
    let settled = recovered.at(ms(2_000));
    assert_eq!(
        lines(&end(&settled)),
        ["settles-when-quiet at 2000: item0 is not terminal"]
    );
}

/// Asserts that a quiet window that would end past the last representable virtual time
/// is not taken as passed: quiet since `u64::MAX - 1` ms and checked at `u64::MAX`, a
/// window of 2 ms is refused and one of 1 ms runs. Catches a deadline that saturates at
/// the largest time and so lets the end check run early.
#[test]
fn a_quiet_window_past_the_largest_time_is_not_passed_early() {
    let late = unfinished_at_end()
        .quiet_since(ms(u64::MAX - 1))
        .at(ms(u64::MAX));
    let expected = NotSettled {
        at: ms(u64::MAX),
        fault_active: false,
        settles_at: ms(u64::MAX),
    };
    assert_eq!(Checker::end(&late, Duration::from_millis(2)), Err(expected));
    let found = Checker::end(&late, Duration::from_millis(1)).expect("settled");
    assert_eq!(flagged(&found), [Invariant::SettlesWhenQuiet]);
}
