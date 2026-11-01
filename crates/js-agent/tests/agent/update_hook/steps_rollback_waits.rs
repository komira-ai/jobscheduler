//! `rollback`'s waits and the endings that are not success: the drain of a new agent that still
//! holds a lease, the stop, the wait for the previous agent's hello, a taint at any point, a
//! host that cannot be read. Every bound is in the fake clock's polls, never in elapsed time.

use js_agent::update_hook::{
    Class, Invalid, Link, Outcome, POLL_SECS, ROLLBACK_HELLO_SECS, ROLLBACK_MARGIN_SECS,
    ROLLBACK_STOP_MIN_SECS, Reason,
};
use serde_json::json;

use super::step_rig::{DRAIN_WAIT_SECS, Rig, Spy, UNIT, line, running};
use super::steps_rollback::{
    RELINK, failed_start, link_target, previous, rolled_back, serving_new, staged, start, stop,
    unit_state,
};

#[test]
fn a_new_agent_that_holds_a_lease_under_our_cordon_is_waited_for_and_not_stopped_when_it_stays() {
    let rig = serving_new("hook-rollback-lease-stays");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(line(&done)["reversible"], json!(false));
    assert!(
        done.report.detail.as_deref().unwrap().contains("lease"),
        "{:?}",
        done.report.detail
    );
    // The drain bound is the declared wait, in polls.
    assert_eq!(rig.world.ticks() as u64, DRAIN_WAIT_SECS / POLL_SECS);
    assert_eq!(
        spy.act_names(),
        Vec::<String>::new(),
        "the agent is not stopped"
    );
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    // Nothing was drained, so no mark says so; the record stays for the rerun.
    let rb = rig.record().unwrap().rb;
    assert!(!rb.drained && !rb.stop_begun);
}

#[test]
fn a_rollback_run_again_after_the_lease_ended_drains_stops_and_restores() {
    let rig = serving_new("hook-rollback-lease-ends-later");
    rig.set_leases(1);
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Failure);
    rig.set_leases(0);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    // The cordon lapsed while nobody renewed it: the rerun has nothing of it to lift, and the
    // wait for the lease did not need it.
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(), RELINK.to_owned(), start()]);
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert_eq!(rig.record(), None);
}

#[test]
fn a_lease_that_ends_during_the_drain_lets_the_rollback_go_on_and_the_cordon_is_renewed_meanwhile()
{
    let rig = serving_new("hook-rollback-lease-ends");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    // 400 s of polls: longer than the cordon's lease of 300 s.
    spy.on_pause(|spy, ticks| {
        if ticks == 40 {
            spy.world.alter_host(|host| {
                host.leases = 0;
                host.lease = None;
            });
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), 40);
    assert!(
        spy.log()
            .iter()
            .filter(|c| c.starts_with("cordon("))
            .count()
            >= 2,
        "the cordon is renewed while the drain waits: {:?}",
        spy.log()
    );
    assert!(
        spy.log().iter().any(|call| call == "uncordon"),
        "the cordon had not lapsed when the rollback ended"
    );
    assert!(rig.host().cordon.is_none());
}

#[test]
fn the_drain_is_cut_to_what_the_declared_rollback_timeout_leaves() {
    let secs = ROLLBACK_MARGIN_SECS + ROLLBACK_STOP_MIN_SECS + ROLLBACK_HELLO_SECS + 100;
    let rig = serving_new("hook-rollback-short-timeout").with_rollback_timeout(secs);
    rig.set_leases(1);
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(rig.world.ticks() as u64, 100 / POLL_SECS);
}

#[test]
fn an_agent_that_is_still_stopping_is_waited_for_and_then_restored_without_a_second_stop() {
    let rig = failed_start("hook-rollback-stopping");
    rig.world.seed_unit(UNIT, "deactivating", "stop-sigterm");
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 3 {
            spy.world.seed_unit(UNIT, "inactive", "dead");
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), 3);
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
}

#[test]
fn an_agent_that_does_not_stop_in_time_fails_the_rollback_as_an_environment_matter() {
    let rig = serving_new("hook-rollback-stop-stuck");
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(line(&done)["reversible"], json!(false));
    // The link is not moved from under an agent that still runs.
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    // The wait leaves what the hello needs of the declared timeout.
    let bound = 8220 - ROLLBACK_MARGIN_SECS - ROLLBACK_HELLO_SECS;
    assert_eq!(
        rig.world.ticks() as u64,
        DRAIN_WAIT_SECS / POLL_SECS + bound.saturating_sub(DRAIN_WAIT_SECS) / POLL_SECS
    );
}

#[test]
fn a_stop_that_fails_is_an_environment_failure_and_a_rerun_goes_on() {
    let rig = serving_new("hook-rollback-stop-fails");
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    let rb = rig.record().unwrap().rb;
    assert!(rb.drained && rb.stop_begun, "{rb:?}");
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [stop(), RELINK.to_owned(), start()]);
}

#[test]
fn a_unit_manager_that_cannot_be_read_fails_the_rollback_before_any_act() {
    let rig = failed_start("hook-rollback-show-fails");
    let spy = Spy::new(&rig);
    spy.fail("show");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(line(&done)["reversible"], json!(false));
}

#[test]
fn an_agent_up_on_neither_release_is_not_touched() {
    let rig = serving_new("hook-rollback-link-elsewhere");
    let elsewhere = rig.tmp.path().join("releases/elsewhere");
    rig.world.relink(&rig.link, &elsewhere).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_link_that_cannot_be_read_fails_the_rollback_before_any_act() {
    let rig = failed_start("hook-rollback-link-unreadable");
    std::fs::remove_file(rig.tmp.path().join("world/link/current")).unwrap();
    std::fs::write(rig.tmp.path().join("world/link/current"), b"not a link").unwrap();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_link_that_cannot_be_moved_back_is_an_environment_failure_and_a_rerun_goes_on() {
    let rig = failed_start("hook-rollback-relink-fails");
    let spy = Spy::new(&rig);
    spy.fail("relink");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::LinkFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(rig.record().unwrap().rb.link_restored);
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(link_target(&rig), Some(previous(&rig)));
}

#[test]
fn the_params_must_name_the_release_link_or_nothing_is_done() {
    let mut rig = failed_start("hook-rollback-no-link-param");
    rig.params.release_link = None;
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_previous_agent_that_fails_to_start_is_a_release_failure_if_it_ran_and_died_and_not_otherwise()
{
    for (state, class) in [
        (("failed", "failed"), Class::Release),
        (("inactive", "dead"), Class::Environment),
    ] {
        let rig = failed_start(&format!("hook-rollback-start-{}", state.0));
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(state);
        let done = spy.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{state:?}");
        assert_eq!(done.report.reason, Some(Reason::StartFailed), "{state:?}");
        assert_eq!(done.report.class, class, "{state:?}");
        assert_eq!(line(&done)["reversible"], json!(false));
    }
}

#[test]
fn a_start_that_errors_is_a_failure_and_the_rerun_starts_it_again() {
    let rig = failed_start("hook-rollback-start-errors");
    let spy = Spy::new(&rig);
    spy.fail("start");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(rig.record().unwrap().rb.start_issued);
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [start()]);
}

#[test]
fn a_previous_agent_that_never_says_hello_fails_the_rollback_after_the_hello_wait() {
    let rig = failed_start("hook-rollback-no-hello");
    let spy = Spy::new(&rig);
    // Active, and no hello: the fake starts nothing that speaks.
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NoHello));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(rig.world.ticks() as u64, ROLLBACK_HELLO_SECS / POLL_SECS);
    assert_eq!(line(&done)["reversible"], json!(false));
    // Nothing is deleted: the rerun looks again.
    assert!(rig.record().is_some());
}

#[test]
fn a_previous_agent_that_says_hello_as_another_version_fails_with_a_version_mismatch() {
    let rig = failed_start("hook-rollback-wrong-hello");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    spy.on_pause(|spy, ticks| {
        if ticks == 1 {
            spy.world.hello("9.9.9").unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::VersionMismatch));
    assert!(done.report.detail.as_deref().unwrap().contains("9.9.9"));
}

#[test]
fn a_lease_server_that_cannot_be_read_during_the_hello_wait_is_an_environment_failure() {
    let rig = failed_start("hook-rollback-hello-unreachable");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    spy.on_pause(|spy, ticks| {
        if ticks == 1 {
            spy.fail("host");
        }
    });
    // The first look of the host succeeded (silent); the later ones fail.
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
}

#[test]
fn a_lease_server_that_cannot_be_read_before_the_drain_ends_the_rollback_with_the_agent_untouched()
{
    let rig = serving_new("hook-rollback-drain-unreachable");
    let spy = Spy::new(&rig);
    spy.fail("host");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_cordon_that_cannot_be_lifted_fails_the_rollback_and_keeps_the_record_for_the_rerun() {
    let rig = failed_start("hook-rollback-uncordon-fails");
    let spy = Spy::new(&rig);
    spy.fail("uncordon");
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(rig.record().unwrap().rb.hello_ok);
    let rerun = Spy::new(&rig);
    assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), Vec::<String>::new());
    assert!(rig.host().cordon.is_none());
}

#[test]
fn a_taint_or_an_owed_check_halts_a_rollback_at_any_point() {
    // Before anything.
    let rig = failed_start("hook-rollback-taint-first");
    rig.taint();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert_eq!(line(&done)["reversible"], json!(false));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    // Before the stop, with nothing to repair.
    let rig = running(Rig::new("hook-rollback-taint-early"));
    rig.taint();
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(line(&done)["reversible"], json!(true));
    // During the drain.
    let rig = serving_new("hook-rollback-taint-drain");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 2 {
            spy.world
                .state_root()
                .write(&js_agent_wire::Taint {
                    reason: "residue".to_owned(),
                    run_id: None,
                    check: None,
                })
                .unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    // While the previous agent is awaited, an owed check halts.
    let rig = failed_start("hook-rollback-owed-hello");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("active", "running"));
    spy.on_pause(|spy, ticks| {
        if ticks == 1 {
            std::fs::write(spy.world.state_root().path().join("owed-run-1"), b"{}").unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::OwedCheck));
}

#[test]
fn a_record_that_breaks_a_rule_or_a_timeout_that_is_too_short_is_refused_and_nothing_is_done() {
    let rig = failed_start("hook-rollback-bad-record");
    let mut record = rig.record().unwrap();
    record.update.unit = "other.service".to_owned();
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.act_names(), Vec::<String>::new());

    let rig = failed_start("hook-rollback-short-declared").with_rollback_timeout(100);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert!(
        done.report
            .detail
            .as_deref()
            .unwrap()
            .contains(&Invalid::RollbackTimeout { rollback: 100 }.to_string()),
        "{:?}",
        done.report.detail
    );
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_rollback_of_a_stopped_agent_ends_as_the_cutover_did_not_happen_except_for_the_runs_it_ended() {
    let rig = failed_start("hook-rollback-end-state");
    let spy = Spy::new(&rig);
    assert_eq!(spy.rollback(&rig).outcome, Outcome::Success);
    let state = rolled_back(&rig);
    assert_eq!(state["end"]["unit"], "active running");
    assert_eq!(state["end"]["record"], serde_json::Value::Null);
    assert_eq!(state["end"]["cordon"], serde_json::Value::Null);
    assert_eq!(state["agent"], "1.3.0");
    assert_eq!(link_target(&rig), Some(previous(&rig)));
}

#[test]
fn a_previous_agent_that_is_still_starting_is_waited_for_until_it_says_hello() {
    let rig = failed_start("hook-rollback-still-starting");
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("activating", "start"));
    spy.on_pause(|spy, ticks| {
        if ticks == 3 {
            spy.world.seed_unit(UNIT, "active", "running");
            spy.world.hello("1.3.0").unwrap();
        }
    });
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), 3);
    assert_eq!(rig.record(), None);
}
