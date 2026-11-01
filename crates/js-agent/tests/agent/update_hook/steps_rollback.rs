//! `rollback` (host-updates design, 4.2): a repair, not a restore. It decides by the state it
//! finds, from the record, the unit and the release link, and ends by lifting our cordon and
//! deleting the record. Each of its acts is marked in the record before it is made and is a
//! no-op when its effect is already there, so a rollback killed anywhere and run again ends as
//! an uninterrupted one. The waits and the endings that are not success are in
//! steps_rollback_waits.rs.

use std::path::PathBuf;
use std::time::Duration;

use js_agent::update_hook::{Clock, Link, Outcome, Reason, Rollback, Units};
use serde_json::{Value, json};

use super::step_rig::{
    MAC_UNIT, Rig, Spy, UNIT, end_state, killable, line, marks, pre_stop, running,
};

pub fn stop() -> String {
    format!("units.stop({UNIT})")
}

pub fn start() -> String {
    format!("units.start({UNIT})")
}

pub const RELINK: &str = "link.relink";

pub fn previous(rig: &Rig) -> PathBuf {
    rig.params.previous_release.clone().unwrap()
}

pub fn staged(rig: &Rig) -> PathBuf {
    rig.params.staged_release.clone()
}

pub fn unit_state(rig: &Rig) -> String {
    let state = rig.world.show(&rig.unit).unwrap();
    format!("{} {}", state.active, state.sub)
}

pub fn link_target(rig: &Rig) -> Option<PathBuf> {
    rig.world.target(&rig.link).unwrap()
}

/// Everything a rollback leaves behind, and the version the host's agent says hello with.
pub fn rolled_back(rig: &Rig) -> Value {
    json!({ "end": end_state(rig), "agent": rig.host().agent_version })
}

/// The host after a `cutover` whose start failed: the stop begun, the link on the staged
/// release, the unit failed. Nothing is serving.
pub fn failed_start(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    // Time passes between the placing of the cordon and the start of the new agent, so that
    // the sessions of the agents the server shows differ.
    rig.advance_clock(5);
    {
        let spy = Spy::new(&rig);
        *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
        assert_eq!(spy.cutover(&rig).outcome, Outcome::Failure);
    }
    rig.kill.disarm();
    rig
}

/// The host after a `cutover` that succeeded, with the new agent serving and no `verify` yet.
pub fn serving_new(name: &str) -> Rig {
    let rig = running(Rig::new(name));
    rig.advance_clock(5);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    rig
}

fn lifted(rig: &Rig) -> bool {
    rig.host().cordon.is_none()
}

#[test]
fn a_rollback_before_the_stop_lifts_our_cordon_deletes_the_record_and_touches_nothing_else() {
    let rig = running(Rig::new("hook-rollback-before-stop"));
    assert!(rig.host().cordon.is_some());
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(line(&done)["reversible"], json!(true));
    assert!(lifted(&rig));
    assert_eq!(rig.record(), None);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    assert!(
        spy.log().iter().all(|call| !call.starts_with("units.")),
        "the unit is not read when nothing was stopped: {:?}",
        spy.log()
    );
}

#[test]
fn a_cordon_that_is_an_operators_is_left_and_one_that_has_lapsed_is_not_lifted() {
    let rig = Rig::new("hook-rollback-operator");
    let expiry = rig.operator_cordons();
    let rig = running(rig);
    let done = Spy::new(&rig).rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(rig.cordon_expiry(), Some(expiry));
    assert_eq!(rig.record(), None);

    let rig = running(Rig::new("hook-rollback-lapsed"));
    rig.world.pause(Duration::from_secs(400));
    let spy = Spy::new(&rig);
    assert_eq!(spy.rollback(&rig).outcome, Outcome::Success);
    assert!(
        spy.log().iter().all(|call| call != "uncordon"),
        "a lapsed cordon is gone, and one in force now is someone else's"
    );
    assert_eq!(rig.record(), None);
}

#[test]
fn a_rollback_with_no_record_has_nothing_to_repair_and_does_nothing() {
    let rig = Rig::new("hook-rollback-no-record");
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(spy.log(), Vec::<String>::new());
}

#[test]
fn a_rollback_run_twice_does_the_second_time_nothing() {
    let rig = failed_start("hook-rollback-twice");
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Success);
    let before = rolled_back(&rig);
    let second = Spy::new(&rig);
    assert_eq!(second.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(second.log(), Vec::<String>::new());
    assert_eq!(rolled_back(&rig), before);
}

#[test]
fn a_rollback_before_the_stop_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-rollback-sweep-c", |name| running(Rig::new(name)));
}

#[test]
fn a_rollback_of_a_stopped_agent_relinks_the_previous_release_starts_it_and_waits_for_its_hello() {
    let rig = failed_start("hook-rollback-a");
    assert_eq!(unit_state(&rig), "failed failed");
    assert_eq!(link_target(&rig), Some(staged(&rig)));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [RELINK.to_owned(), start()]);
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    assert_eq!(unit_state(&rig), "active running");
    // The runs the stop ended are not brought back: the report says so.
    assert_eq!(line(&done)["reversible"], json!(false));
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(lifted(&rig));
    assert_eq!(rig.record(), None);
}

#[test]
fn a_rollback_of_a_serving_new_agent_drains_stops_relinks_starts_and_waits_for_the_old_one() {
    let rig = serving_new("hook-rollback-b");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [stop(), RELINK.to_owned(), start()]);
    assert_eq!(link_target(&rig), Some(previous(&rig)));
    assert_eq!(unit_state(&rig), "active running");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert_eq!(line(&done)["reversible"], json!(false));
    assert!(lifted(&rig));
    assert_eq!(rig.record(), None);
    // The drain looked at the host before the stop, and no unit was stopped before it.
    let log = spy.log();
    let looked = log
        .iter()
        .position(|c| c.starts_with("units.show"))
        .unwrap();
    assert!(looked < log.iter().position(|c| *c == stop()).unwrap());
}

#[test]
fn a_rollback_after_a_verify_that_passed_is_the_same_repair() {
    let rig = serving_new("hook-rollback-b-verified-once");
    assert_eq!(Spy::new(&rig).verify(&rig).outcome, Outcome::Success);
    let spy = Spy::new(&rig);
    assert_eq!(spy.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(spy.act_names(), [stop(), RELINK.to_owned(), start()]);
    assert_eq!(rig.record(), None);
}

#[test]
fn a_rollback_of_an_agent_that_is_up_on_the_previous_release_only_waits_for_its_hello() {
    let baseline = failed_start("hook-rollback-resumed-base");
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    // What a rollback killed just before it marked the hello leaves: the link back, the
    // previous agent started and heard, the marks of both acts on disk.
    let rig = failed_start("hook-rollback-resumed");
    rig.world.relink(&rig.link, &previous(&rig)).unwrap();
    rig.world.start(&rig.unit).unwrap();
    let mut record = rig.record().unwrap();
    record.rb.link_restored = true;
    record.rb.start_issued = true;
    rig.put_record(&record);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    assert_eq!(spy.rollback(&rig).outcome, Outcome::Success);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rolled_back(&rig), rolled_back(&baseline));
}

#[test]
fn each_act_of_a_rollback_is_marked_in_the_record_before_it_is_made() {
    for (label, build, expected) in [
        (
            "a",
            failed_start as fn(&str) -> Rig,
            vec![
                // Before the relink, then before the start.
                (RELINK, [false, false, true, false, false]),
                ("start", [false, false, true, true, false]),
            ],
        ),
        (
            "b",
            serving_new as fn(&str) -> Rig,
            vec![
                ("stop", [true, true, false, false, false]),
                (RELINK, [true, true, true, false, false]),
                ("start", [true, true, true, true, false]),
            ],
        ),
    ] {
        let baseline = build(&format!("hook-rollback-marks-{label}-base"));
        let spy = Spy::new(&baseline);
        assert_eq!(spy.rollback(&baseline).outcome, Outcome::Success);
        let points: Vec<u32> = spy.acts.borrow().iter().map(|act| act.points).collect();
        assert_eq!(points.len(), expected.len(), "{label}");
        for (k, (point, (act, want))) in points.iter().zip(&expected).enumerate() {
            let rig = build(&format!("hook-rollback-marks-{label}-{k}"));
            rig.kill.arm(*point);
            assert!(killable(|| Spy::new(&rig).rollback(&rig)).is_none());
            rig.kill.disarm();
            // drained, stop_begun, link_restored, start_issued, hello_ok.
            let Rollback {
                drained,
                stop_begun,
                link_restored,
                start_issued,
                hello_ok,
            } = rig.record().unwrap().rb;
            assert_eq!(
                [drained, stop_begun, link_restored, start_issued, hello_ok],
                *want,
                "{label}: killed before {act}"
            );
            // The act itself was not made; the rerun makes it and the rest.
            let rerun = Spy::new(&rig);
            assert_eq!(rerun.rollback(&rig).outcome, Outcome::Success);
            assert_eq!(rerun.act_names(), spy.act_names()[k..], "{label}: {act}");
            assert_eq!(rolled_back(&rig), rolled_back(&baseline), "{label}: {act}");
        }
    }
}

/// Runs `rollback` of `build`'s host to its end, then again from a fresh host once for each
/// kill point it passes, killed there and run again, and requires the same end every time.
pub fn sweep(label: &str, build: fn(&str) -> Rig) {
    let baseline = build(&format!("{label}-base"));
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    assert!(
        points >= 3,
        "the uncordon and the delete pass points: {points}"
    );
    let end = rolled_back(&baseline);
    assert_eq!(baseline.record(), None);
    for k in 1..=points {
        let rig = build(&format!("{label}-{k}"));
        rig.kill.arm(k);
        assert!(
            killable(|| Spy::new(&rig).rollback(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        rig.started_agent_speaks(unit_state(&rig) == "active running");
        let done = Spy::new(&rig).rollback(&rig);
        assert_eq!(
            done.outcome,
            Outcome::Success,
            "killed at point {k}: {done:?}"
        );
        assert_eq!(rolled_back(&rig), end, "killed at point {k}");
    }
}

#[test]
fn a_rollback_of_a_stopped_agent_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    sweep("hook-rollback-sweep-a", failed_start);
}

#[test]
fn a_rollback_of_a_serving_new_agent_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one()
 {
    sweep("hook-rollback-sweep-b", serving_new);
}

#[test]
fn a_rollback_of_a_first_install_that_has_not_linked_yet_deletes_the_record_and_is_reversible() {
    let rig = Rig::first_install("hook-rollback-first-early");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(line(&done)["reversible"], json!(true));
    assert_eq!(rig.record(), None);
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

#[test]
fn a_rollback_of_a_first_install_that_has_linked_cannot_restore_anything_and_says_so() {
    let rig = Rig::first_install("hook-rollback-first-late");
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Failure);
    assert_eq!(marks(&rig), [false, true, true]);
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NoPreviousRelease));
    assert_eq!(line(&done)["reversible"], json!(false));
    assert_eq!(spy.act_names(), Vec::<String>::new());
    // The record goes, so that the next order is not refused for it.
    assert_eq!(rig.record(), None);
    let again = Spy::new(&rig).rollback(&rig);
    assert_eq!(again.outcome, Outcome::Success);
}

#[test]
fn a_rollback_on_a_mac_boots_the_new_agent_out_and_the_old_one_in() {
    let rig = running(Rig::mac("hook-rollback-mac"));
    rig.advance_clock(5);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.act_names(),
        [
            format!("units.stop({MAC_UNIT})"),
            RELINK.to_owned(),
            format!("units.start({MAC_UNIT})")
        ]
    );
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert_eq!(rig.host().runtime.as_deref(), Some("sandbox-exec"));
}

#[test]
fn the_session_before_the_stop_is_the_one_the_old_agents_hello_must_differ_from() {
    let rig = serving_new("hook-rollback-session");
    let before = pre_stop(&rig);
    assert!(before.is_some());
    assert_eq!(Spy::new(&rig).rollback(&rig).outcome, Outcome::Success);
    assert_ne!(rig.host().session_started_ms, before);
}
