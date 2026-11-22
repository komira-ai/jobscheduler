//! A record of an earlier update whose rollback was begun and never finished: the update hands
//! the runs over, the start of the new agent is refused (a systemd 258 or newer that will not
//! start a unit whose cgroup still holds the runs the stop left), the rollback stops the unit
//! that waits to restart and its start is refused too, and the previous agent comes back by
//! itself once the last run ends. Its record keeps every mark up to the rollback's start and
//! no `rb.hello_ok`, and the next update names another target.
//!
//! `prepare` of the next target finishes such a record when its rollback is complete in effect
//! (it is moved aside) and goes on; one whose rollback is not complete is kept, and the detail
//! names the first thing left. `rollback` of the next target rolls back the update on record
//! with the params that record was written under (see `step::supersede`).

use std::fs;

use js_agent::update_hook::step;
use js_agent::update_hook::{
    Clock, Done, Outcome, Params, Reason, Record, Rollback, StateView, Units,
};
use serde_json::json;

use super::rollback_entries::WAIT;
use super::step_rig::{Rig, Spy, UNIT, as_root, line};

/// What an agent that takes the handover prints to `--version` (no `in-place`).
const TAKES: &str = "js-agent 1.4.0\nruntimes: docker crun\nstops: drain handover\n";

/// The detail of a `prepare` that hands the runs over by stopping and starting the unit.
const HANDOVER_NOTE: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";

/// The target of the record left on the host.
const LEFT: &str = "1.4.0";

/// The target of the next order: a release after the one on record.
const NEXT: &str = "1.5.0";

/// A spy whose agents take the handover.
fn spy(rig: &Rig) -> Spy<'_> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(TAKES.to_owned());
    spy
}

/// The host as the earlier update to 1.4.0 left it: its record with the rollback's start
/// issued and refused, and the previous agent (1.3.0) running again on the previous release,
/// a session of its own on the lease server.
fn left_by_a_refused_rollback(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.world.seed_unit(UNIT, "active", "running");
    rig.set_leases(2);
    assert_eq!(spy(&rig).prepare(&rig).outcome, Outcome::Success);
    rig.kill.disarm();

    let done = {
        let cutover = spy(&rig);
        cutover.fail("start");
        cutover.cutover(&rig)
    };
    assert_eq!(done.report.reason, Some(Reason::StartFailed), "{done:?}");
    rig.kill.disarm();
    // The unit manager starts the unit again and again; each start is refused.
    rig.world.seed_unit(UNIT, "activating", "auto-restart");
    rig.world.pause(WAIT);

    let done = {
        let rollback = spy(&rig);
        rollback.fail("start");
        rollback.rollback(&rig)
    };
    assert_eq!(done.report.reason, Some(Reason::StartFailed), "{done:?}");
    rig.kill.disarm();
    rig.world.pause(WAIT);

    // The last run ends; the unit's next start is the previous release's agent, which says
    // hello.
    rig.world.seed_unit(UNIT, "active", "running");
    rig.world.hello("1.3.0").unwrap();
    rig.set_leases(1);
    rig
}

/// The marks the record holds: those of the record the host was left with.
fn assert_left_marks(record: &Record) {
    assert!(record.handover && !record.in_place);
    assert!(record.stop_begun && record.link_moved && record.start_issued);
    assert!(!record.verify_ok);
    assert_eq!(
        record.rb,
        Rollback {
            drained: false,
            stop_begun: true,
            link_restored: true,
            start_issued: true,
            hello_ok: false,
        }
    );
    assert!(!record.cordon.placed);
}

/// The params of the order of [`NEXT`], with its own staged release, whose agent is there.
fn next_order(rig: &Rig) -> Params {
    let staged = rig.tmp.path().join("releases").join(NEXT);
    fs::create_dir_all(staged.join("bin")).unwrap();
    fs::create_dir_all(staged.join("units")).unwrap();
    let old = &rig.params.staged_release;
    fs::copy(old.join("bin/js-agent"), staged.join("bin/js-agent")).unwrap();
    fs::copy(
        old.join("units").join(UNIT),
        staged.join("units").join(UNIT),
    )
    .unwrap();
    Params {
        target_version: NEXT.to_owned(),
        staged_release: staged,
        artifact_digest: format!("sha256:{}", "c".repeat(64)),
        ..rig.params.clone()
    }
}

/// A spy whose agents take the handover and whose staged agent is of [`NEXT`].
fn next_spy(rig: &Rig) -> Spy<'_> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(TAKES.replace(LEFT, NEXT));
    spy
}

fn prepare(spy: &Spy<'_>, rig: &Rig, params: &Params) -> Done {
    step::prepare(&spy.env(rig, &as_root), params)
}

fn rollback(spy: &Spy<'_>, rig: &Rig, params: &Params) -> Done {
    step::rollback(&spy.env(rig, &as_root), params)
}

/// The names in the state directory, sorted, without the lock.
fn state_files(rig: &Rig) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(rig.state_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "lock")
        .collect();
    names.sort();
    names
}

/// The detail of a refusal of the record of [`LEFT`] that is kept, saying why.
fn kept(why: &str) -> String {
    format!(
        "the record is not valid: target of the record is not what the params table says; the record is kept: {why}"
    )
}

/// What a `prepare` of [`NEXT`] says of a record of [`LEFT`] whose rollback is not complete.
fn not_complete(what: &str) -> String {
    kept(&format!(
        "the update to {LEFT} has stopped the previous agent and its rollback is not complete: {what}; the rollback step finishes it"
    ))
}

/// The note every ending of a `rollback` of [`NEXT`] on the record of [`LEFT`] carries.
fn on_record() -> String {
    format!(
        "the record is of the update to {LEFT}, not {NEXT}: rolled back with its own target and releases"
    )
}

/// Asserts that `done` refused the record with `detail`, made no act, and left the state
/// directory as it was: the record's bytes, and no other file.
fn refused_as_is(rig: &Rig, spy: &Spy<'_>, done: &Done, before: &[u8], detail: &str) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(detail));
    assert_eq!(rig.record_bytes().as_deref(), Some(before));
    assert_eq!(state_files(rig), ["record.json"]);
    assert_eq!(spy.act_names(), Vec::<String>::new());
}

/// The step an operator runs by hand on such a host with the released hook: `rollback` with
/// the params the record was written under (its own target, previous version, digest and
/// releases). With the previous agent running on the previous release and saying hello, it
/// makes no act on the unit: it marks the hello, deletes the record and ends in success, the
/// unit and the agent's session as they were.
///
/// Catches a rollback that stops, signals or starts the agent that runs, which would cut the
/// jobs the agent holds.
#[test]
fn the_rollback_of_the_record_with_its_own_params_finishes_it_without_an_act() {
    let rig = left_by_a_refused_rollback("hook-stale-by-hand");
    assert_left_marks(&rig.record().unwrap());
    let session = rig.host().session_started_ms;
    let spy = spy(&rig);
    let done = rollback(&spy, &rig, &rig.params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        line(&done),
        json!({"class": "release", "promotion_pending": false, "reversible": false})
    );
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record(), None);
    assert_eq!(state_files(&rig), Vec::<String>::new());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
    assert_eq!(rig.host().session_started_ms, session);
    assert_eq!(rig.host().leases, 1);
}

/// `prepare` of the next target on that host moves the record aside, bytes kept, under a name
/// that says whose it was and that it was rolled back, writes the record of the next target
/// and goes on (here to a hand-over), with no act on the unit and the jobs left running.
///
/// Catches the halt the host showed (`state_invalid: ... roll it back first`), a `prepare`
/// that deletes the record, and one that touches the agent that runs.
#[test]
fn prepare_of_the_next_target_finishes_a_record_whose_rollback_is_complete_and_goes_on() {
    let rig = left_by_a_refused_rollback("hook-stale-prepare");
    let before = rig.record_bytes().unwrap();
    let params = next_order(&rig);
    let spy = next_spy(&rig);
    let aside = format!("record.rolled-back-{LEFT}-{}.json", spy.now_ms());
    let done = prepare(&spy, &rig, &params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "finished the record of {LEFT}, whose rollback is complete in effect (the previous agent runs on the previous release): moved to {}; {HANDOVER_NOTE}",
            rig.state_dir().join(&aside).display()
        ))
    );
    assert_eq!(fs::read(rig.state_dir().join(&aside)).unwrap(), before);
    assert_eq!(state_files(&rig), ["record.json".to_owned(), aside]);
    let record = rig.record().unwrap();
    assert_eq!(record.update.target, NEXT);
    assert_eq!(record.update.staged, params.staged_release);
    assert!(record.handover && !record.stop_begun && record.rb == Rollback::default());
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
    assert_eq!(rig.host().leases, 1);
}

/// The record of an update that handed the runs back in place, with the request taken (no
/// handover file left), is finished as well.
#[test]
fn prepare_finishes_an_in_place_record_whose_request_was_taken() {
    let rig = left_by_a_refused_rollback("hook-stale-in-place-taken");
    let mut record = rig.record().unwrap();
    record.in_place = true;
    rig.put_record(&record);
    let spy = next_spy(&rig);
    let done = prepare(&spy, &rig, &next_order(&rig));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.record().unwrap().update.target, NEXT);
    assert_eq!(state_files(&rig).len(), 2, "{:?}", state_files(&rig));
}

/// The updater's `rollback` of the next order, run after its `prepare` failed, rolls back the
/// update on record with the params that record was written under: on that host it makes no
/// act, deletes the record and says whose update it rolled back.
///
/// Catches the second failure the host showed (`rollback failed: state_invalid`).
#[test]
fn the_rollback_of_the_next_order_rolls_back_the_update_on_record() {
    let rig = left_by_a_refused_rollback("hook-stale-rollback-next");
    let spy = next_spy(&rig);
    let done = rollback(&spy, &rig, &next_order(&rig));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        line(&done),
        json!({
            "class": "release",
            "promotion_pending": false,
            "reversible": false,
            "detail": on_record(),
        })
    );
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(rig.record(), None);
    assert_eq!(state_files(&rig), Vec::<String>::new());
}

/// A rollback on record that is not complete (the unit is down) is finished by the next
/// order's `rollback`: the previous release's agent is started from the link the updater left
/// on it and its hello awaited, and the ending carries the note; a failed start carries it too.
#[test]
fn the_rollback_of_the_next_order_finishes_a_rollback_on_record_that_is_not_complete() {
    let rig = left_by_a_refused_rollback("hook-stale-rollback-start");
    rig.world.seed_unit(UNIT, "failed", "failed");
    let failing = next_spy(&rig);
    failing.fail("start");
    let done = rollback(&failing, &rig, &next_order(&rig));
    assert_eq!(done.report.reason, Some(Reason::StartFailed), "{done:?}");
    assert!(
        done.report
            .detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with(&on_record())),
        "{done:?}"
    );
    assert!(rig.record().is_some());

    let spy = next_spy(&rig);
    let done = rollback(&spy, &rig, &next_order(&rig));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail, Some(on_record()));
    assert_eq!(spy.act_names(), [format!("units.start({UNIT})")]);
    assert_eq!(rig.record(), None);
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
}

/// A record whose rollback is not complete is kept by `prepare` of the next target, with no
/// act and no file moved, and the detail names the first thing left: the release link, the
/// unit, the agent the update started, an in-place request not taken, and the lease server
/// (unreadable, another version, or no session of the previous agent since the rollback's first
/// act).
#[test]
fn prepare_keeps_a_record_whose_rollback_is_not_complete_and_says_what_is_left() {
    type Make = fn(&Rig, &Spy<'_>, &mut Record);
    let cases: [(&str, Make, fn(&Rig) -> String); 10] = [
        (
            "unit-down",
            |rig, _, _| rig.world.seed_unit(UNIT, "failed", "failed"),
            |_| format!("{UNIT} is failed (failed): no agent runs"),
        ),
        (
            "unit-unread",
            |_, spy, _| spy.fail("show"),
            |_| format!("the state of {UNIT} cannot be read: show is down"),
        ),
        (
            "link-staged",
            |rig, _, _| rig.updater_links_staged(),
            |rig| {
                format!(
                    "the release link {} is not on the previous release {}",
                    rig.link.display(),
                    rig.params.previous_release.as_ref().unwrap().display()
                )
            },
        ),
        (
            "link-unread",
            |rig, _, _| {
                fs::remove_file(&rig.link).unwrap();
                fs::write(&rig.link, b"not a link").unwrap();
            },
            |rig| {
                format!(
                    "cannot read {}: Invalid argument (os error 22)",
                    rig.link.display()
                )
            },
        ),
        (
            "new-agent",
            |_, _, record| {
                record.rb = Rollback::default();
                record.rb_pre_stop_session_ms = None;
            },
            |_| format!("{UNIT} runs the agent the update to {LEFT} started"),
        ),
        (
            "in-place-pending",
            |_, spy, record| {
                record.in_place = true;
                StateView::request_handover(spy).unwrap();
            },
            |_| {
                "the request to hand the runs over in place is still in the agent's state root"
                    .to_owned()
            },
        ),
        (
            "in-place-unread",
            |_, spy, record| {
                record.in_place = true;
                spy.fail("pending");
            },
            |_| "cannot look at the handover request: pending is down".to_owned(),
        ),
        (
            "lease-unread",
            |_, spy, _| spy.fail("host"),
            |_| "the lease server cannot be read: host is down".to_owned(),
        ),
        (
            "other-version",
            |rig, _, _| rig.world.hello(LEFT).unwrap(),
            |rig| {
                let runtime = rig.host().runtime.unwrap_or_default();
                format!("the session is {LEFT} on {runtime}, not 1.3.0 on {runtime}")
            },
        ),
        (
            "old-session",
            |rig, _, record| record.rb_pre_stop_session_ms = rig.host().session_started_ms,
            |_| {
                "the lease server does not show the live session of the previous agent (1.3.0) the rollback waits for"
                    .to_owned()
            },
        ),
    ];
    for (name, make, what) in cases {
        let rig = left_by_a_refused_rollback(&format!("hook-stale-left-{name}"));
        let params = next_order(&rig);
        let spy = next_spy(&rig);
        let mut record = rig.record().unwrap();
        make(&rig, &spy, &mut record);
        rig.put_record(&record);
        spy.acts.borrow_mut().clear();
        let before = rig.record_bytes().unwrap();
        let done = prepare(&spy, &rig, &params);
        let what = what(&rig);
        refused_as_is(&rig, &spy, &done, &before, &not_complete(&what));
    }
}

/// A record of another target that returns the host to another previous release than the
/// order's is kept by `prepare` and by `rollback`, with no act: neither knows that release to be
/// the one that runs, and the detail says the rollback needs the record's own params. A first
/// install's record under the params of an update is refused by `rollback` the same way: its
/// rollback would stop the agent the updater says runs now.
///
/// Catches a `rollback` of the next order that stops or starts the unit for an update the
/// order does not return the host from.
#[test]
fn a_record_that_returns_the_host_elsewhere_is_kept_by_prepare_and_rollback() {
    let rig = left_by_a_refused_rollback("hook-stale-elsewhere");
    let previous = rig.params.previous_release.clone().unwrap();
    let mut params = next_order(&rig);
    params.previous_version = Some("1.3.5".to_owned());
    params.previous_release = Some(rig.tmp.path().join("releases/1.3.5"));
    let before = rig.record_bytes().unwrap();
    let spy = next_spy(&rig);
    let done = prepare(&spy, &rig, &params);
    refused_as_is(
        &rig,
        &spy,
        &done,
        &before,
        &kept(&format!(
            "the update to {LEFT} returns the host to 1.3.0 at {}, not to the previous release the params table names: run its rollback with the params it was written under",
            previous.display()
        )),
    );
    let done = rollback(&spy, &rig, &params);
    refused_as_is(
        &rig,
        &spy,
        &done,
        &before,
        &kept(&format!(
            "the update to {LEFT} returns the host to 1.3.0, not to the previous release the params table names: run its rollback with the params it was written under"
        )),
    );

    let rig = Rig::first_install("hook-stale-first-install-elsewhere");
    let mut record = rig.fresh_record();
    record.link_moved = true;
    record.start_issued = true;
    rig.put_record(&record);
    rig.world.seed_unit(UNIT, "active", "running");
    let mut params = next_order(&rig);
    params.previous_version = Some("1.3.0".to_owned());
    params.previous_release = Some(rig.tmp.path().join("releases/1.3.0"));
    let before = rig.record_bytes().unwrap();
    let spy = next_spy(&rig);
    let done = rollback(&spy, &rig, &params);
    refused_as_is(
        &rig,
        &spy,
        &done,
        &before,
        &kept(&format!(
            "the update to {LEFT} returns the host to no previous version, not to the previous release the params table names: run its rollback with the params it was written under"
        )),
    );
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
}

/// `rollback` rolls back only a record this hook wrote of another target: a record of the same
/// target that disagrees with the params (another digest), and a record of another target that
/// breaks a rule of its own (a mark without the one it follows from), are refused as before,
/// with no act and the record kept.
#[test]
fn rollback_still_refuses_a_record_it_did_not_write_for_another_target() {
    let rig = left_by_a_refused_rollback("hook-stale-rollback-refuses");
    let before = rig.record_bytes().unwrap();
    let spy = next_spy(&rig);
    let mut same_target = rig.params.clone();
    same_target.artifact_digest = format!("sha256:{}", "d".repeat(64));
    let done = rollback(&spy, &rig, &same_target);
    refused_as_is(
        &rig,
        &spy,
        &done,
        &before,
        "the record is not valid: artifact_digest of the record is not what the params table says",
    );

    let mut record = rig.record().unwrap();
    record.rb.hello_ok = true;
    record.rb.start_issued = false;
    rig.put_record(&record);
    let before = rig.record_bytes().unwrap();
    let done = rollback(&spy, &rig, &next_order(&rig));
    refused_as_is(
        &rig,
        &spy,
        &done,
        &before,
        "the record is not valid: target of the record is not what the params table says",
    );
}
