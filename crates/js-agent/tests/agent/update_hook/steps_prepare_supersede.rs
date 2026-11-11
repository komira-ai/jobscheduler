//! `prepare` and a record of another target (see `step::supersede`): a record of an update
//! that never took the host is moved aside and the new update goes on; one that did still
//! guards the host. The record that is set aside keeps its bytes, under a name that says
//! whose it was.

use std::fs;
use std::path::PathBuf;

use js_agent::update_hook::{
    Cordon, Done, Lease, Outcome, Reason, Record, UnitRestart, UpdateInfo,
};

use super::step_rig::{Rig, START, Spy, UNIT, killable};

/// The older target the left record is of.
const OLD: &str = "0.1.214";

/// The record an attempt to install [`OLD`] left: its own release and digest, nothing done.
fn left_record(rig: &Rig) -> Record {
    let root = rig.tmp.path();
    let previous = rig.params.previous_version.is_some();
    Record::new(UpdateInfo {
        target: OLD.to_owned(),
        previous: previous.then(|| "0.1.200".to_owned()),
        staged: root.join("releases").join(OLD),
        previous_target: previous.then(|| root.join("releases/0.1.200")),
        unit: rig.unit.clone(),
        artifact_digest: format!("sha256:{}", "b".repeat(64)),
    })
}

/// Where `prepare` moves the record of [`OLD`] at the world's start.
fn aside_path(rig: &Rig) -> PathBuf {
    rig.state_dir()
        .join(format!("record.superseded-{OLD}-{START}.json"))
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

/// The detail of a step that superseded the record of [`OLD`].
fn superseded(rig: &Rig) -> String {
    format!(
        "superseded the record of {OLD}, which never took the host: moved to {}",
        aside_path(rig).display()
    )
}

/// The refusal of a record of [`OLD`] that is kept, saying why.
fn kept(why: &str) -> String {
    format!(
        "the record is not valid: target of the record is not what the params table says; the record is kept: {why}"
    )
}

/// Asserts that `done` refused the record with `detail` and left the state directory as it
/// was: the record's bytes, and no other file.
fn refused_as_is(rig: &Rig, done: &Done, before: Option<&Vec<u8>>, detail: &str) {
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(done.report.detail.as_deref(), Some(detail));
    assert_eq!(rig.record_bytes().as_ref(), before);
    assert_eq!(state_files(rig), ["record.json"]);
}

/// The live case: the first native install of 0.1.221 on a host whose state directory holds
/// the record an archive-era first install of 0.1.214 left, never installed (no mark set).
/// `prepare` moves that record aside, with its bytes, writes the record of 0.1.221 and
/// succeeds, saying in its last line where the old record went.
///
/// Catches a `prepare` that refuses that record `state_invalid` (the failure the host showed)
/// or that deletes it instead of keeping it.
#[test]
fn a_never_installed_first_install_of_an_older_target_is_superseded_by_the_native_install() {
    let mut rig = Rig::first_install("hook-supersede-live-case");
    rig.params.target_version = "0.1.221".to_owned();
    let staged = rig.params.staged_release.clone();
    fs::create_dir_all(staged.join("tree/usr/bin")).unwrap();
    fs::create_dir_all(staged.join("tree/usr/lib/systemd/system")).unwrap();
    fs::rename(
        staged.join("units").join(UNIT),
        staged.join("tree/usr/lib/systemd/system").join(UNIT),
    )
    .unwrap();
    fs::rename(
        staged.join("bin/js-agent"),
        staged.join("tree/usr/bin/js-agent"),
    )
    .unwrap();
    rig.put_record(&left_record(&rig));
    let before = rig.record_bytes().unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert_eq!(done.report.detail, Some(superseded(&rig)));
    assert_eq!(fs::read(aside_path(&rig)).unwrap(), before);
    assert_eq!(rig.record(), Some(rig.fresh_record()));
    assert_eq!(rig.record().unwrap().update.target, "0.1.221");
    assert_eq!(
        state_files(&rig),
        [
            "record.json".to_owned(),
            format!("record.superseded-{OLD}-{START}.json")
        ]
    );
    // The first install of the new target looks at no unit and no lease server.
    assert_eq!(
        spy.log(),
        [format!(
            "run({}/tree/usr/bin/js-agent --version)",
            staged.display()
        )]
    );
}

/// A record of an update to another target that never stopped the agent is superseded, and
/// the update goes on to cordon and drain. A cordon that record marked as its own is not this
/// update's: it is found in force, neither lifted nor marked, and left to lapse.
#[test]
fn an_update_of_another_target_that_never_stopped_the_agent_is_superseded() {
    let rig = Rig::new("hook-supersede-update");
    let mut left = left_record(&rig);
    let expiry = rig
        .world
        .cordon(300)
        .unwrap()
        .host
        .cordon
        .unwrap()
        .expires_at_ms;
    left.cordon = Cordon {
        placed: true,
        provisional: false,
        first_ms: START,
        expires_ms: expiry,
    };
    left.unit_restart = Some(UnitRestart {
        changed: vec!["Service.ExecStart".to_owned()],
    });
    rig.put_record(&left);
    let before = rig.record_bytes().unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail, Some(superseded(&rig)));
    assert_eq!(fs::read(aside_path(&rig)).unwrap(), before);
    let record = rig.record().unwrap();
    assert_eq!(record.update, rig.fresh_record().update);
    assert_eq!(record.cordon, Cordon::default(), "the cordon is not ours");
    assert_eq!(record.unit_restart, None, "the plan is the new update's");
    assert!(
        !spy.log().contains(&"uncordon".to_owned()),
        "{:?}",
        spy.log()
    );
    assert_eq!(rig.cordon_expiry(), Some(expiry));
}

/// A first install of another target that moved the link (its commit) but whose unit is down,
/// `failed` or never installed (`inactive`), is superseded: its agent does not run, and its
/// `rollback` would make no act.
#[test]
fn a_committed_first_install_of_another_target_whose_unit_is_down_is_superseded() {
    for (name, unit) in [
        ("failed", Some(("failed", "failed"))),
        ("inactive", Some(("inactive", "dead"))),
        ("not-installed", None),
    ] {
        let rig = Rig::first_install(&format!("hook-supersede-down-{name}"));
        let mut left = left_record(&rig);
        left.link_moved = true;
        left.start_issued = true;
        rig.put_record(&left);
        if let Some((active, sub)) = unit {
            rig.world.seed_unit(UNIT, active, sub);
        }
        let before = rig.record_bytes().unwrap();
        let spy = Spy::new(&rig);
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(done.report.detail, Some(superseded(&rig)), "{name}");
        assert_eq!(fs::read(aside_path(&rig)).unwrap(), before, "{name}");
        assert_eq!(rig.record(), Some(rig.fresh_record()), "{name}");
        assert_eq!(spy.log()[0], format!("units.show({UNIT})"), "{name}");
    }
}

/// A first install of another target that moved the link and whose agent's unit is up, on its
/// way up or stopping still guards the host: `state_invalid`, the record kept as it was and
/// the unit's state named.
#[test]
fn a_committed_first_install_of_another_target_whose_unit_is_not_down_is_kept() {
    for (active, sub) in [
        ("active", "running"),
        ("activating", "start"),
        ("deactivating", "stop-sigterm"),
    ] {
        let rig = Rig::first_install(&format!("hook-supersede-kept-{active}"));
        let mut left = left_record(&rig);
        left.link_moved = true;
        left.start_issued = true;
        rig.put_record(&left);
        rig.world.seed_unit(UNIT, active, sub);
        let before = rig.record_bytes();
        let spy = Spy::new(&rig);
        let done = spy.prepare(&rig);
        refused_as_is(
            &rig,
            &done,
            before.as_ref(),
            &kept(&format!(
                "the first install of {OLD} moved the link and {UNIT} is {active}"
            )),
        );
        assert_eq!(spy.log(), [format!("units.show({UNIT})")], "{active}");
    }
}

/// A first install of another target that moved the link, on a host whose unit manager cannot
/// say what the unit is, is kept: the hook cannot tell that its agent does not run.
#[test]
fn a_committed_first_install_whose_unit_cannot_be_read_is_kept() {
    let rig = Rig::first_install("hook-supersede-kept-unreadable");
    let mut left = left_record(&rig);
    left.link_moved = true;
    rig.put_record(&left);
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    spy.fail("show");
    let done = spy.prepare(&rig);
    refused_as_is(
        &rig,
        &done,
        before.as_ref(),
        &kept(&format!(
            "the first install of {OLD} moved the link and the state of {UNIT} cannot be read: show is down"
        )),
    );
}

/// An update to another target that has stopped the previous agent (its commit) is kept,
/// whatever the unit shows: its `rollback` is what restores the previous agent, and the detail
/// says to run it.
#[test]
fn an_update_of_another_target_past_its_stop_is_kept_until_it_is_rolled_back() {
    let rig = Rig::new("hook-supersede-kept-stopped");
    let mut left = left_record(&rig);
    left.stop_begun = true;
    rig.put_record(&left);
    rig.world.seed_unit(UNIT, "inactive", "dead");
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    refused_as_is(
        &rig,
        &done,
        before.as_ref(),
        &kept(&format!(
            "the update to {OLD} has stopped the previous agent: roll it back first"
        )),
    );
    assert_eq!(spy.log(), Vec::<String>::new());
}

/// A record of another target that breaks a rule of its own (here a mark set without the one
/// it follows from, and a unit that is not the hook's) is not one this hook wrote: it is
/// refused as before, with no word of superseding.
#[test]
fn a_record_of_another_target_that_breaks_a_rule_of_its_own_is_refused_as_before() {
    let rig = Rig::new("hook-supersede-broken-marks");
    let mut left = left_record(&rig);
    left.start_issued = true;
    rig.put_record(&left);
    let before = rig.record_bytes();
    let done = Spy::new(&rig).prepare(&rig);
    refused_as_is(
        &rig,
        &done,
        before.as_ref(),
        "the record is not valid: target of the record is not what the params table says",
    );

    let rig = Rig::first_install("hook-supersede-broken-unit");
    let mut left = left_record(&rig);
    left.update.unit = "js-agent-other.service".to_owned();
    rig.put_record(&left);
    let before = rig.record_bytes();
    let done = Spy::new(&rig).prepare(&rig);
    refused_as_is(
        &rig,
        &done,
        before.as_ref(),
        "the record is not valid: unit \"js-agent-other.service\" is not the hook's unit",
    );
}

/// A record of another target is set aside only when the new record is about to be written:
/// a taint, a staged agent of the wrong version, a missing join token and a lease server that
/// does not answer each end the step with the record where it was and no other file.
#[test]
fn a_record_of_another_target_is_kept_when_prepare_ends_before_it_writes() {
    let rig = Rig::first_install("hook-supersede-order-taint");
    rig.put_record(&left_record(&rig));
    rig.taint();
    let before = rig.record_bytes();
    assert_eq!(Spy::new(&rig).prepare(&rig).outcome, Outcome::Halt);
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(state_files(&rig), ["record.json"]);

    let rig = Rig::first_install("hook-supersede-order-version");
    rig.put_record(&left_record(&rig));
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    *spy.version.borrow_mut() = Ok(format!("js-agent {OLD}\n"));
    let done = spy.prepare(&rig);
    assert_eq!(done.report.reason, Some(Reason::VersionMismatch));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(state_files(&rig), ["record.json"]);

    let rig = Rig::first_install("hook-supersede-order-token");
    rig.put_record(&left_record(&rig));
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    spy.token.set(super::step_rig::TokenFile::Missing);
    let done = spy.prepare(&rig);
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(state_files(&rig), ["record.json"]);

    // An update reads the host before it writes; a server that does not answer ends it after
    // the record of the other target went aside, and with no record of its own.
    let rig = Rig::new("hook-supersede-order-lease");
    rig.put_record(&left_record(&rig));
    let before = rig.record_bytes().unwrap();
    let spy = Spy::new(&rig);
    spy.fail("host");
    let done = spy.prepare(&rig);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(
        done.report
            .detail
            .as_deref()
            .map(|d| d.starts_with(&superseded(&rig))),
        Some(true),
        "{done:?}"
    );
    assert_eq!(fs::read(aside_path(&rig)).unwrap(), before);
    assert_eq!(rig.record_bytes(), None);
}

/// The set-aside never replaces a file: a name that is taken fails the step `state_invalid`
/// naming it, with the record and that file kept as they were.
#[test]
fn a_set_aside_onto_a_name_that_is_taken_fails_and_keeps_both() {
    let rig = Rig::first_install("hook-supersede-taken");
    rig.put_record(&left_record(&rig));
    fs::write(aside_path(&rig), "earlier").unwrap();
    let before = rig.record_bytes();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{}: File exists (os error 17)",
            aside_path(&rig).display()
        ))
    );
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(fs::read_to_string(aside_path(&rig)).unwrap(), "earlier");
}

/// A `prepare` killed at the set-aside leaves the record where it was; one killed just after
/// it leaves no record, and the rerun is a first `prepare` of the new target. Both reruns end
/// with the record of the new target and the old one kept once.
#[test]
fn a_prepare_killed_at_or_after_the_set_aside_is_finished_by_its_rerun() {
    for (name, point) in [("at", 1), ("after", 2)] {
        let rig = Rig::first_install(&format!("hook-supersede-kill-{name}"));
        rig.put_record(&left_record(&rig));
        let before = rig.record_bytes().unwrap();
        rig.kill.arm(point);
        assert!(
            killable(|| Spy::new(&rig).prepare(&rig)).is_none(),
            "{name}"
        );
        if point == 1 {
            assert_eq!(rig.record_bytes(), Some(before.clone()), "{name}");
        } else {
            assert_eq!(rig.record_bytes(), None, "{name}");
            assert_eq!(fs::read(aside_path(&rig)).unwrap(), before, "{name}");
        }
        rig.kill.disarm();
        let done = Spy::new(&rig).prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(rig.record(), Some(rig.fresh_record()), "{name}");
        assert_eq!(fs::read(aside_path(&rig)).unwrap(), before, "{name}");
        assert_eq!(
            state_files(&rig).len(),
            2,
            "{name}: {:?}",
            state_files(&rig)
        );
    }
}

/// Only `prepare` supersedes: `cutover`, `verify`, `rollback` and `cleanup` of the new target
/// on a record of another target that never took the host still refuse it, as before.
#[test]
fn the_other_steps_still_refuse_a_record_of_another_target() {
    let rig = Rig::new("hook-supersede-other-steps");
    rig.put_record(&left_record(&rig));
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    for done in [
        spy.cutover_as_is(&rig),
        spy.verify(&rig),
        spy.rollback_as_is(&rig),
        spy.cleanup(&rig),
    ] {
        assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some("the record is not valid: target of the record is not what the params table says")
        );
    }
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(state_files(&rig), ["record.json"]);
}
