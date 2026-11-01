//! `verify` (host-updates design, 4.2): after `cutover`, wait for proof that the new agent runs
//! and serves, and mark `verify_ok` after it. It only reads: the call log shows no cordon, link
//! or unit change in any path, and the machine ends as `cutover` left it.

use std::time::Duration;

use js_agent::update_hook::{
    Class, Clock, Done, Outcome, POLL_SECS, Reason, Units, VERIFY_MARGIN_SECS,
};
use serde_json::Value;

use super::step_rig::{Rig, Spy, UNIT, end_state, killable, running};

/// A host `cutover` has finished on. With `hello` the new agent has said hello on its start;
/// without, the unit is active and the server has heard nothing since.
fn cut_over(name: &str, hello: bool) -> Rig {
    let rig = running(Rig::new(name));
    // Time passes between the placing of the cordon and the start of the new agent.
    rig.world.pause(Duration::from_secs(5));
    let done = {
        let spy = Spy::new(&rig);
        if !hello {
            *spy.start_leaves.borrow_mut() = Some(("active", "running"));
        }
        spy.cutover(&rig)
    };
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    rig.kill.disarm();
    rig
}

/// The unit, the link and the cordon, without the record.
fn machine(rig: &Rig) -> Value {
    let mut state = end_state(rig);
    state.as_object_mut().unwrap().remove("record");
    state
}

/// `verify` read and nothing else: every call is a look at the unit.
fn only_looked(spy: &Spy<'_>) {
    assert_eq!(spy.act_names(), Vec::<String>::new());
    for call in spy.log() {
        assert!(call.starts_with("units.show("), "{call}");
    }
}

/// How many seconds of the declared bound `verify` waits.
fn bound_secs(rig: &Rig) -> u64 {
    rig.declaration.timeouts.verify - VERIFY_MARGIN_SECS
}

fn release_failure(done: &Done, reason: Reason) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(reason), "{done:?}");
    assert_eq!(done.report.class, Class::Release);
}

#[test]
fn a_clean_verify_marks_verify_ok_after_it_saw_the_new_agent_and_changes_nothing_else() {
    let rig = cut_over("hook-verify-clean", true);
    let before = machine(&rig);
    let mut want = rig.record().unwrap();
    assert!(!want.verify_ok);
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    want.verify_ok = true;
    assert_eq!(rig.record().unwrap(), want, "only the mark is new");
    assert_eq!(
        machine(&rig),
        before,
        "the cordon, link and unit are as they were"
    );
    only_looked(&spy);
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
}

#[test]
fn verify_waits_for_the_hello_of_an_agent_that_started_without_one() {
    let rig = cut_over("hook-verify-late-hello", false);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 3 {
            spy.world.hello("1.4.0").unwrap();
        }
    });
    let ticks = rig.world.ticks();
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.world.ticks() - ticks,
        3,
        "one pause per look without proof"
    );
    assert!(rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_session_that_began_before_the_cordon_is_not_proof_even_with_the_target_version() {
    let rig = cut_over("hook-verify-stale", false);
    // The server still shows the old agent's session; only its version reads as the target's.
    rig.world
        .alter_host(|host| host.agent_version = Some("1.4.0".to_owned()));
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    release_failure(&done, Reason::NoHello);
    assert_eq!(rig.record_bytes(), before);
    only_looked(&spy);
}

#[test]
fn a_new_session_of_another_version_is_not_proof() {
    let rig = cut_over("hook-verify-wrong-version", false);
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.hello("1.9.0").unwrap();
        }
    });
    let done = spy.verify(&rig);
    release_failure(&done, Reason::VersionMismatch);
    assert!(done.report.detail.unwrap().contains("1.9.0"));
    assert!(!rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_new_session_on_another_runtime_is_not_proof() {
    let rig = cut_over("hook-verify-wrong-runtime", true);
    rig.world
        .alter_host(|host| host.runtime = Some("docker".to_owned()));
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    release_failure(&done, Reason::VersionMismatch);
    assert!(!rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_server_that_restarted_shows_no_session_until_the_agent_says_hello_again() {
    let rig = cut_over("hook-verify-restarted", true);
    rig.world.forget_host();
    assert_eq!(rig.host().state, "awaiting");
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.hello("1.4.0").unwrap();
        }
    });
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.host().epoch, 1);
    assert!(rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_unit_that_dies_or_restarts_during_verify_ends_it_as_the_releases_failure() {
    for (n, (active, sub)) in [
        ("failed", "failed"),
        ("activating", "auto-restart"),
        ("inactive", "dead"),
        ("deactivating", "stop-sigterm"),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = cut_over(&format!("hook-verify-crash-{n}"), false);
        let spy = Spy::new(&rig);
        let base = rig.world.ticks();
        spy.on_pause(move |spy, ticks| {
            if ticks == base + 2 {
                spy.world.seed_unit(UNIT, active, sub);
            }
        });
        let ticks = rig.world.ticks();
        let done = spy.verify(&rig);
        release_failure(&done, Reason::StartFailed);
        assert_eq!(
            rig.world.ticks() - ticks,
            2,
            "it ends at the look that saw it: {active}"
        );
        assert!(!rig.record().unwrap().verify_ok);
        let unit = rig.world.show(UNIT).unwrap();
        assert_eq!((unit.active.as_str(), unit.sub.as_str()), (active, sub));
        only_looked(&spy);
    }
}

#[test]
fn a_lease_server_that_never_answers_is_the_environments_failure_at_the_bound() {
    let rig = cut_over("hook-verify-lease-down", true);
    let spy = Spy::new(&rig);
    spy.fail("host");
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_lease_server_that_answers_again_in_time_is_not_a_failure() {
    let rig = cut_over("hook-verify-lease-blip", true);
    let spy = Spy::new(&rig);
    spy.fail("host");
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 4 {
            spy.heal("host");
        }
    });
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(rig.record().unwrap().verify_ok);
}

#[test]
fn a_unit_whose_state_cannot_be_read_is_the_environments_failure() {
    let rig = cut_over("hook-verify-show-down", true);
    let spy = Spy::new(&rig);
    spy.fail("show");
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(!rig.record().unwrap().verify_ok);
}

#[test]
fn verify_ends_at_the_declared_timeout_less_the_margin_and_not_before() {
    let rig = cut_over("hook-verify-bound", false);
    let spy = Spy::new(&rig);
    let (ticks, began) = (rig.world.ticks(), rig.world.now_ms());
    let done = spy.verify(&rig);
    release_failure(&done, Reason::NoHello);
    let waited = rig.world.now_ms() - began;
    assert_eq!(waited, bound_secs(&rig) * 1000);
    assert_eq!(
        rig.world.ticks() - ticks,
        usize::try_from(bound_secs(&rig) / POLL_SECS).unwrap()
    );
    assert!(
        rig.declaration.timeouts.verify * 1000 > waited,
        "inside the declared bound"
    );
    only_looked(&spy);
}

#[test]
fn a_rerun_after_verify_ok_succeeds_without_a_look() {
    let rig = cut_over("hook-verify-rerun", true);
    assert_eq!(Spy::new(&rig).verify(&rig).outcome, Outcome::Success);
    let record = rig.record_bytes();
    // Even a server that is down and a unit that failed since say nothing to a verify that passed.
    let spy = Spy::new(&rig);
    spy.fail("host");
    rig.world.seed_unit(UNIT, "failed", "failed");
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.log(), Vec::<String>::new());
    assert_eq!(rig.record_bytes(), record);
    assert_eq!(rig.world.ticks(), 1, "no pause but the setup's");
}

#[test]
fn a_verify_killed_before_its_mark_observes_again_and_marks_on_the_rerun() {
    let rig = cut_over("hook-verify-killed", true);
    rig.kill.arm(1);
    let before = rig.record_bytes();
    assert!(
        killable(|| Spy::new(&rig).verify(&rig)).is_none(),
        "the mark passes a kill point"
    );
    assert_eq!(
        rig.record_bytes(),
        before,
        "nothing is marked before the kill point"
    );
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    assert_eq!(spy.verify(&rig).outcome, Outcome::Success);
    assert!(rig.record().unwrap().verify_ok);
    assert!(!spy.log().is_empty(), "it looked again");
    assert_eq!(rig.kill.points(), 1, "the mark is the only change it makes");
}

#[test]
fn verify_without_a_record_or_before_the_unit_was_started_refuses_and_looks_at_nothing() {
    let rig = running(Rig::new("hook-verify-no-record"));
    // `prepare` ran and `cutover` did not.
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    release_failure(&done, Reason::StateInvalid);
    assert!(!rig.record().unwrap().start_issued);
    let empty = Rig::new("hook-verify-empty");
    let spy2 = Spy::new(&empty);
    release_failure(&spy2.verify(&empty), Reason::StateInvalid);
    assert_eq!(empty.record_bytes(), None);
    assert_eq!(spy.log(), Vec::<String>::new());
    assert_eq!(spy2.log(), Vec::<String>::new());
}

#[test]
fn verify_with_the_link_not_on_the_staged_release_refuses() {
    let rig = running(Rig::new("hook-verify-link"));
    let mut record = rig.record().unwrap();
    record.stop_begun = true;
    record.link_moved = true;
    record.start_issued = true;
    rig.put_record(&record);
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    release_failure(&done, Reason::StateInvalid);
    assert!(!rig.record().unwrap().verify_ok);
    assert_eq!(spy.log(), Vec::<String>::new());
}

#[test]
fn a_taint_during_verify_halts() {
    let rig = cut_over("hook-verify-taint", true);
    rig.taint();
    let spy = Spy::new(&rig);
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert!(!rig.record().unwrap().verify_ok);
    only_looked(&spy);
}

#[test]
fn a_unit_still_starting_is_no_proof_even_when_the_server_shows_a_hello() {
    let rig = cut_over("hook-verify-starting", true);
    rig.world.seed_unit(UNIT, "activating", "start");
    let spy = Spy::new(&rig);
    let base = rig.world.ticks();
    spy.on_pause(move |spy, ticks| {
        if ticks == base + 2 {
            spy.world.seed_unit(UNIT, "active", "running");
        }
    });
    let done = spy.verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        rig.world.ticks() - base,
        2,
        "it waited for the unit, not the hello"
    );
    assert!(rig.record().unwrap().verify_ok);
    only_looked(&spy);
}
