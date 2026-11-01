//! `cutover`'s cordon (host-updates design, 3.6 and 4.2): `prepare` can end with no cordon
//! held, so before the stop `cutover` looks again at whether the host is cordoned and holds no
//! lease, puts a lost cordon of ours back within the cap or defers (75), and never touches a
//! cordon that is not ours. Nothing is stopped on a deferral.

use std::time::Duration;

use js_agent::update_hook::{Class, Clock, Cordon, Done, Lease, Outcome, Reason, Units};
use serde_json::json;

use super::step_rig::{
    LEASE_SECS, MAX_SECS, RETRY_AFTER_SECS, Rig, START, Spy, UNIT, end_state, killable, line,
    marks, running,
};

fn cordon_calls(spy: &Spy<'_>) -> Vec<String> {
    spy.log()
        .into_iter()
        .filter(|call| call.starts_with("cordon("))
        .collect()
}

/// A deferral of a host that is still the agent's: nothing stopped, linked or started.
fn deferred_untouched(done: &Done, spy: &Spy<'_>, rig: &Rig, reason: Reason) {
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.outcome.code(), 75);
    assert_eq!(done.report.reason, Some(reason));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert_eq!(line(done)["reversible"], json!(true));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(marks(rig), [false, false, false]);
    let state = rig.world.show(UNIT).unwrap();
    assert_eq!(
        (state.active.as_str(), state.sub.as_str()),
        ("active", "running")
    );
}

/// The host after a `prepare` that returned 0 with no cordon: the kill window.
fn never_cordoned(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.world.seed_unit(UNIT, "active", "running");
    rig.put_record(&rig.fresh_record());
    rig
}

/// The host after a `prepare` whose cordon the server no longer holds.
fn lost(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    rig.world.uncordon().unwrap();
    rig.kill.disarm();
    rig
}

/// The host after a `prepare` that waited on an operator's cordon.
fn operators(name: &str) -> (Rig, u64) {
    let rig = Rig::new(name);
    rig.world.seed_unit(UNIT, "active", "running");
    let expiry = rig.operator_cordons();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    rig.kill.disarm();
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
    (rig, expiry)
}

/// A record that carries a cordon mark nothing confirmed, and an operator's cordon in force.
fn provisional(name: &str) -> (Rig, Cordon, u64) {
    let rig = Rig::new(name);
    rig.world.seed_unit(UNIT, "active", "running");
    let mark = Cordon {
        placed: true,
        provisional: true,
        first_ms: START,
        expires_ms: START + LEASE_SECS * 1000,
    };
    let mut record = rig.fresh_record();
    record.cordon = mark;
    rig.put_record(&record);
    let expiry = rig.operator_cordons();
    rig.kill.disarm();
    (rig, mark, expiry)
}

#[test]
fn a_cordon_that_was_lost_is_put_back_before_the_stop_and_the_cap_still_counts_from_the_first() {
    let rig = lost("hook-cutover-lost");
    assert_eq!(rig.cordon_expiry(), None);
    let before = rig.record().unwrap().cordon;
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    let log = spy.log();
    let asked = log
        .iter()
        .position(|c| *c == format!("cordon({LEASE_SECS})"));
    let stopped = log.iter().position(|c| *c == format!("units.stop({UNIT})"));
    assert!(asked.unwrap() < stopped.unwrap(), "{log:?}");
    assert!(rig.cordon_expiry().is_some(), "held again");
    let after = rig.record().unwrap().cordon;
    assert_eq!(
        after.first_ms, before.first_ms,
        "the cap runs from the first placement"
    );
    assert_eq!(after.expires_ms, rig.cordon_expiry().unwrap());
    assert!(after.placed && !after.provisional);
}

#[test]
fn a_cordon_that_lapsed_on_the_clock_is_put_back_for_what_the_cap_leaves() {
    let rig = running(Rig::with_cordon("hook-cutover-lapsed", LEASE_SECS, 3700));
    // 3500 s later our 300 s cordon is gone and 200 s of the 3700 s cap are left.
    Clock::pause(&rig.world, Duration::from_secs(3500));
    rig.kill.disarm();
    assert_eq!(rig.cordon_expiry(), None);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(cordon_calls(&spy), ["cordon(200)"]);
    let cordon = rig.record().unwrap().cordon;
    assert_eq!(cordon.first_ms, START);
    assert_eq!(cordon.expires_ms, START + 3500 * 1000 + 200 * 1000);
}

#[test]
fn a_prepare_that_ended_with_no_cordon_at_all_gets_one_placed_and_recorded_first() {
    let rig = never_cordoned("hook-cutover-never");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(cordon_calls(&spy), [format!("cordon({LEASE_SECS})")]);
    let cordon = rig.record().unwrap().cordon;
    assert_eq!(
        cordon,
        Cordon {
            placed: true,
            provisional: false,
            first_ms: START,
            expires_ms: START + LEASE_SECS * 1000
        }
    );
    assert_eq!(rig.cordon_expiry(), Some(cordon.expires_ms));
}

#[test]
fn a_cutover_killed_at_any_point_with_a_lost_cordon_ends_as_an_uninterrupted_one() {
    sweep("hook-cutover-sweep-lost", lost);
    sweep("hook-cutover-sweep-never", never_cordoned);
}

/// A cutover run to its end, then again from a fresh host once for each kill point it passes,
/// killed there and run again. The rerun ends where an uninterrupted run does, except after a
/// kill between the placing of a cordon and the confirming of its mark: it finds a cordon it
/// cannot attribute and defers, with nothing stopped and the mark as it was.
fn sweep(label: &str, build: fn(&str) -> Rig) {
    let baseline = build(&format!("{label}-base"));
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = end_state(&baseline);
    let mut deferred = 0_u32;
    for k in 1..=points {
        let rig = build(&format!("{label}-{k}"));
        rig.kill.arm(k);
        assert!(
            killable(|| Spy::new(&rig).cutover(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let unconfirmed =
            rig.record().is_some_and(|r| r.cordon.provisional) && rig.cordon_expiry().is_some();
        let spy = Spy::new(&rig);
        let done = spy.cutover(&rig);
        if unconfirmed {
            deferred += 1;
            deferred_untouched(&done, &spy, &rig, Reason::CordonUnconfirmed);
            continue;
        }
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(end_state(&rig), end, "killed at point {k}");
        assert_eq!(marks(&rig), [true, true, true], "killed at point {k}");
    }
    assert_eq!(deferred, u32::from(label.ends_with("never")), "{label}");
}

#[test]
fn a_lost_cordon_whose_cap_is_spent_defers_with_nothing_stopped_and_clears_the_stale_mark() {
    let rig = running(Rig::with_cordon("hook-cutover-cap", LEASE_SECS, 3600));
    Clock::pause(&rig.world, Duration::from_secs(3700));
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::CordonCapReached);
    assert_eq!(cordon_calls(&spy), Vec::<String>::new());
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(
        rig.record().unwrap().cordon,
        Cordon::default(),
        "nothing left to lift"
    );
}

#[test]
fn a_lease_on_a_host_whose_cordon_is_ours_defers_lifts_the_cordon_and_stops_nothing() {
    let rig = running(Rig::new("hook-cutover-busy"));
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::DrainBound);
    assert!(spy.log().contains(&"uncordon".to_owned()));
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
    assert_eq!(rig.world.ticks(), 0, "cutover does not wait for a drain");
}

#[test]
fn a_host_that_has_runs_to_finish_is_not_idle_either() {
    let rig = running(Rig::new("hook-cutover-releasing"));
    rig.world.alter_host(|host| host.releasing = true);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::DrainBound);
}

#[test]
fn a_lease_that_came_in_while_the_cordon_was_lost_defers_and_lifts_the_cordon_just_placed() {
    let rig = lost("hook-cutover-lost-busy");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::DrainBound);
    assert_eq!(cordon_calls(&spy), [format!("cordon({LEASE_SECS})")]);
    assert!(spy.log().contains(&"uncordon".to_owned()));
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_lease_server_that_cannot_be_asked_fails_the_cutover_before_anything_is_stopped() {
    for call in ["host", "cordon"] {
        let rig = if call == "host" {
            running(Rig::new("hook-cutover-down-host"))
        } else {
            lost("hook-cutover-down-cordon")
        };
        let spy = Spy::new(&rig);
        spy.fail(call);
        let done = spy.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{call}");
        assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable), "{call}");
        assert_eq!(done.report.class, Class::Environment, "{call}");
        assert!(done.report.reversible, "{call}");
        assert_eq!(spy.act_names(), Vec::<String>::new(), "{call}");
        assert_eq!(marks(&rig), [false, false, false], "{call}");
    }
}

#[test]
fn a_lease_server_that_cannot_lift_the_cordon_fails_the_deferral_and_stops_nothing() {
    let rig = running(Rig::new("hook-cutover-busy-uncordon-fails"));
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert!(done.report.reversible);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert!(
        rig.record().unwrap().cordon.placed,
        "a rerun still knows it is ours"
    );
}

#[test]
fn an_operators_cordon_is_neither_renewed_nor_lifted_and_does_not_stop_the_cutover() {
    let (rig, expiry) = operators("hook-cutover-operator");
    let spy = Spy::new(&rig);
    // A stop that takes 500 s, longer than a lease: our own cordon would be renewed here.
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|spy, ticks| {
        if ticks == 50 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(rig.world.ticks(), 50);
    assert!(
        spy.log()
            .iter()
            .all(|call| !call.starts_with("cordon(") && call != "uncordon")
    );
    assert_eq!(
        rig.cordon_expiry(),
        Some(expiry),
        "neither extended nor lifted"
    );
    assert_eq!(rig.record().unwrap().cordon, Cordon::default());
}

#[test]
fn an_operators_cordon_over_a_lease_defers_and_is_not_lifted() {
    let (rig, expiry) = operators("hook-cutover-operator-busy");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::DrainBound);
    assert!(
        spy.log()
            .iter()
            .all(|call| call != "uncordon" && !call.starts_with("cordon("))
    );
    assert_eq!(rig.cordon_expiry(), Some(expiry));
}

#[test]
fn a_cordon_found_after_ours_lapsed_is_someone_elses_and_is_left_alone() {
    let rig = running(Rig::new("hook-cutover-lapsed-then-operator"));
    Clock::pause(&rig.world, Duration::from_secs(400));
    let expiry = rig.operator_cordons();
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert!(
        spy.log()
            .iter()
            .all(|call| call != "uncordon" && !call.starts_with("cordon("))
    );
    assert_eq!(rig.cordon_expiry(), Some(expiry));
}

#[test]
fn a_provisional_mark_with_a_cordon_in_force_defers_and_leaves_both_alone() {
    let (rig, mark, expiry) = provisional("hook-cutover-provisional");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::CordonUnconfirmed);
    assert!(
        spy.log()
            .iter()
            .all(|call| call != "uncordon" && !call.starts_with("cordon("))
    );
    assert_eq!(
        rig.cordon_expiry(),
        Some(expiry),
        "neither renewed nor lifted"
    );
    assert_eq!(
        rig.record().unwrap().cordon,
        mark,
        "the mark is left as it was"
    );

    // With a lease on the host as well it is the same answer: the cordon is not ours to lift.
    let (rig, mark, expiry) = provisional("hook-cutover-provisional-busy");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::CordonUnconfirmed);
    assert!(
        spy.log()
            .iter()
            .all(|call| call != "uncordon" && !call.starts_with("cordon("))
    );
    assert_eq!(rig.cordon_expiry(), Some(expiry));
    assert_eq!(rig.record().unwrap().cordon, mark);
}

#[test]
fn a_cordon_nobody_renews_is_not_carried_into_a_stop_that_outlasts_it() {
    // The cordon may lapse during a stop of up to about 4200 s and nobody renews it; the
    // lease server could then hand the host a job. The cutover stops nothing.
    let (rig, mark, _) = provisional("hook-cutover-provisional-long-stop");
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|spy, ticks| {
        if ticks == 100 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.cutover(&rig);
    deferred_untouched(&done, &spy, &rig, Reason::CordonUnconfirmed);
    assert_eq!(rig.world.ticks(), 0, "no wait for a stop that never began");
    assert_eq!(rig.record().unwrap().cordon, mark);
}

#[test]
fn a_provisional_mark_is_not_renewed_by_a_rerun_that_is_past_the_point_of_deferring() {
    let (rig, mark, expiry) = provisional("hook-cutover-provisional-committed");
    let mut record = rig.record().unwrap();
    record.stop_begun = true;
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|spy, ticks| {
        if ticks == 20 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert!(
        spy.log()
            .iter()
            .all(|call| call != "uncordon" && !call.starts_with("cordon("))
    );
    assert_eq!(rig.cordon_expiry(), Some(expiry));
    assert_eq!(rig.record().unwrap().cordon, mark);
}

#[test]
fn our_cordon_is_renewed_while_the_unit_takes_its_time_to_stop() {
    let rig = running(Rig::with_cordon("hook-cutover-renew", 30, MAX_SECS));
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|spy, ticks| {
        if ticks == 12 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    let renewals = cordon_calls(&spy).len();
    assert!(
        renewals >= 5,
        "renewed when less than half the lease was left: {renewals}"
    );
    let now = rig.world.now_ms();
    let expiry = rig.cordon_expiry().expect("still cordoned");
    assert!(expiry > now);
    assert_eq!(rig.record().unwrap().cordon.expires_ms, expiry);
}

#[test]
fn a_rerun_after_the_stop_began_does_not_look_at_the_cordon_or_the_leases_again() {
    let rig = running(Rig::new("hook-cutover-committed"));
    let spy = Spy::new(&rig);
    spy.fail("start");
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Failure);
    // The cordon went and a lease came in: too late to defer.
    rig.world.uncordon().unwrap();
    rig.set_leases(1);
    rig.kill.disarm();
    let rerun = Spy::new(&rig);
    rerun.fail("host");
    rerun.fail("cordon");
    let done = rerun.cutover(&rig);
    assert_eq!(
        done.outcome,
        Outcome::Success,
        "the lease server is not asked"
    );
    assert_eq!(rerun.act_names(), [format!("units.start({UNIT})")]);
}
