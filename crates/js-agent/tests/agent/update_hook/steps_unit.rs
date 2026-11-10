//! What `prepare` makes of the unit the staged release carries and `cutover` does about it: a
//! unit identical to the installed one changes nothing; a different one is recorded by the keys
//! that differ, and `cutover` on systemd waits, after the stop, for the unit's cgroup to hold no
//! process before it starts the agent. And the join token a first install needs.

use std::fs;

use js_agent::update_hook::step::TokenFacts;
use js_agent::update_hook::{
    Class, INSTALLED_MISSING, Outcome, Reason, Record, StateView, UnitRestart, Units,
};
use serde_json::json;

use super::step_rig::{
    RETRY_AFTER_SECS, Rig, Spy, TokenFile, UNIT, end_state, euid, good_token, killable, line,
    marks, running,
};

const SHOW: &str = "/usr/bin/systemctl show --property=TasksCurrent --value js-agent-crun.service";

/// The `systemctl show` calls of the log, in order.
fn cgroup_reads(spy: &Spy<'_>) -> usize {
    spy.log()
        .iter()
        .filter(|call| *call == &format!("run({SHOW})"))
        .count()
}

fn changed(restart: Option<UnitRestart>) -> Vec<String> {
    restart.expect("a restart plan").changed
}

/// A unit identical to the installed one: nothing is recorded, prepare says nothing of it, and
/// cutover never asks about the cgroup.
///
/// Catches a restart plan recorded for an unchanged unit, and a cutover that waits on a cgroup it
/// has no reason to.
#[test]
fn an_identical_staged_unit_records_no_restart_and_cutover_makes_no_cgroup_read() {
    let rig = Rig::new("hook-unit-identical");
    rig.world.seed_unit(UNIT, "active", "running");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail, None);
    assert_eq!(rig.record().unwrap().unit_restart, None);
    assert_eq!(cgroup_reads(&spy), 0);
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(cgroup_reads(&spy), 0);
    assert!(spy.log().iter().all(|call| !call.contains("systemctl")));
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
}

/// A different unit: the record holds the keys that differ, and the success of prepare says
/// them.
///
/// Catches a plan that names the file and not the keys, a detail that does not say them, and a
/// plan that is not in the record `cutover` reads.
#[test]
fn a_different_staged_unit_is_recorded_by_the_keys_that_differ_and_prepare_says_them() {
    let rig = Rig::new("hook-unit-differs");
    rig.stage_changed_unit();
    rig.world.seed_unit(UNIT, "active", "running");
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            "the unit changes (Service.DelegateSubgroup, Service.ExecStart): cutover starts the agent on the new unit"
        )
    );
    assert_eq!(
        changed(rig.record().unwrap().unit_restart),
        ["Service.DelegateSubgroup", "Service.ExecStart"]
    );
    // The plan is in the file the way the earlier hook can read around: one more key.
    let value: serde_json::Value = serde_json::from_slice(&rig.record_bytes().unwrap()).unwrap();
    assert_eq!(
        value["unit_restart"],
        json!({"changed": ["Service.DelegateSubgroup", "Service.ExecStart"]})
    );
    // A rerun of prepare says it again and keeps the plan.
    rig.kill.disarm();
    let again = Spy::new(&rig).prepare(&rig);
    assert_eq!(again.report.detail, done.report.detail);
}

/// A deferral is not a success: its detail is the drain's, not the plan's.
///
/// Catches a note about the unit written over the reason a prepare was deferred.
#[test]
fn a_deferred_prepare_keeps_its_own_detail_whatever_the_unit_does() {
    let rig = Rig::new("hook-unit-differs-deferred");
    rig.stage_changed_unit();
    rig.world.seed_unit(UNIT, "active", "running");
    rig.set_leases(1);
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert!(
        done.report
            .detail
            .unwrap()
            .starts_with("the host still held")
    );
    assert!(rig.record().unwrap().unit_restart.is_some());
}

/// A previous release that carries no unit (the agent was installed some other way) is a change
/// named so; one whose unit cannot be read is the host's fault; neither changes anything.
///
/// Catches the first release under hostkeeper being taken for an unchanged unit.
#[test]
fn a_previous_release_without_a_unit_is_a_change_and_an_unreadable_one_is_state_invalid() {
    let rig = Rig::new("hook-unit-installed-missing");
    rig.world.seed_unit(UNIT, "active", "running");
    fs::remove_file(rig.unit_file(rig.params.previous_release.as_ref().unwrap())).unwrap();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        changed(rig.record().unwrap().unit_restart),
        [INSTALLED_MISSING]
    );
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            "the unit changes ((installed unit missing)): cutover starts the agent on the new unit"
        )
    );

    let rig = Rig::new("hook-unit-installed-unreadable");
    let installed = rig.unit_file(rig.params.previous_release.as_ref().unwrap());
    fs::remove_file(&installed).unwrap();
    fs::create_dir(&installed).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert!(
        done.report
            .detail
            .unwrap()
            .starts_with("cannot read the installed unit ")
    );
    assert_eq!(rig.record_bytes(), None);
}

/// A release that does not carry the unit it is to run is no artifact, for an update, for a
/// first install and on a Mac, and nothing is changed.
///
/// Catches a release without its unit let through to a cutover that restarts on nothing.
#[test]
fn a_staged_release_without_its_unit_is_a_bad_artifact_and_nothing_is_changed() {
    for (n, rig) in [
        Rig::new("hook-unit-staged-missing-update"),
        Rig::first_install("hook-unit-staged-missing-first"),
        Rig::mac("hook-unit-staged-missing-mac"),
    ]
    .into_iter()
    .enumerate()
    {
        fs::remove_file(rig.unit_file(&rig.params.staged_release)).unwrap();
        let spy = Spy::new(&rig);
        *spy.version.borrow_mut() = Ok("js-agent 1.4.0\n".to_owned());
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{n}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::BadArtifact), "{n}");
        assert!(done.report.reversible, "{n}");
        assert!(
            done.report
                .detail
                .unwrap()
                .starts_with("the staged release carries no readable unit "),
            "{n}"
        );
        assert_eq!(rig.record_bytes(), None, "{n}");
        assert_eq!(rig.world.ticks(), 0, "{n}");
        assert_eq!(rig.cordon_expiry(), None, "{n}");
    }
}

/// A record the earlier hook wrote has no plan; this hook reads it, and a prepare that finds it
/// does not add a plan to it.
///
/// Catches a record of the current release that this one refuses.
#[test]
fn a_record_written_without_a_plan_is_read_and_stays_without_one() {
    let rig = Rig::new("hook-unit-old-record");
    rig.stage_changed_unit();
    rig.world.seed_unit(UNIT, "active", "running");
    let mut value = serde_json::to_value(rig.fresh_record()).unwrap();
    assert!(value.get("unit_restart").is_none());
    value["cordon"] = json!({"placed": false, "first_ms": 0, "expires_ms": 0});
    let record: Record = serde_json::from_value(value).unwrap();
    rig.put_record(&record);
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail, None);
    assert_eq!(rig.record().unwrap().unit_restart, None);
}

/// With a plan on systemd, cutover stops the unit and then reads the unit's task count until it
/// is zero (or `[not set]`), pausing a poll between reads, and only then starts.
///
/// Catches a start into a cgroup that still holds strays (the new `Delegate` layout needs an empty
/// one), and a wait that does not look again.
#[test]
fn cutover_with_a_changed_unit_waits_after_the_stop_for_an_empty_cgroup_before_it_starts() {
    let rig = Rig::new("hook-unit-cutover-waits");
    rig.stage_changed_unit();
    let rig = running(rig);
    let spy = Spy::new(&rig);
    spy.tasks.borrow_mut().extend([
        Ok("3\n".to_owned()),
        Ok("1\n".to_owned()),
        Ok("[not set]\n".to_owned()),
    ]);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(cgroup_reads(&spy), 3);
    assert_eq!(rig.world.ticks(), 2, "a poll between the reads");
    let log = spy.log();
    let at = |what: &str| log.iter().position(|call| call.contains(what)).unwrap();
    assert!(at("units.stop") < at("systemctl"), "{log:?}");
    assert!(at("systemctl") < at("units.start"), "{log:?}");
    assert_eq!(
        spy.act_names(),
        [
            format!("units.stop({UNIT})"),
            format!("units.start({UNIT})")
        ]
    );
}

/// A cgroup that is still busy at the stop's bound fails the cutover `unit_not_empty`, class
/// environment, irreversibly (the stop is the commit), with the count and the changed keys in
/// the words, nothing started; the rerun starts when the cgroup is empty and stops nothing.
///
/// Catches a cutover that starts into a busy cgroup, one that waits for ever, and one that
/// blames the release.
#[test]
fn a_cgroup_that_stays_busy_fails_unit_not_empty_at_the_bound_and_a_rerun_goes_on() {
    let rig = Rig::new("hook-unit-cutover-busy");
    rig.stage_changed_unit();
    let rig = running(rig);
    let spy = Spy::new(&rig);
    spy.tasks
        .borrow_mut()
        .extend(std::iter::repeat_n(Ok("5\n".to_owned()), 600));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::UnitNotEmpty));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(line(&done)["reversible"], json!(false));
    let said = done.report.detail.as_deref().unwrap();
    assert!(said.contains("holds 5 process(es) after 4140 s"), "{said}");
    assert!(
        said.contains("Service.DelegateSubgroup, Service.ExecStart"),
        "{said}"
    );
    // The declared bound of cutover (4320 s) less the start wait (120 s) and a margin (60 s).
    assert_eq!(rig.world.ticks(), 414);
    assert_eq!(spy.act_names(), [format!("units.stop({UNIT})")]);
    assert_eq!(marks(&rig), [true, false, false]);
    let rerun = Spy::new(&rig);
    let done = rerun.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), [format!("units.start({UNIT})")]);
}

/// A `systemctl` that fails or prints what is no count is the host's fault, said so, and the
/// start waits.
///
/// Catches a read that fails taken for an empty cgroup.
#[test]
fn a_cgroup_that_cannot_be_read_fails_the_cutover_as_an_environment_matter() {
    for (n, (printed, said)) in [
        (
            Err("no systemctl".to_owned()),
            "cannot read the tasks of js-agent-crun.service: no systemctl",
        ),
        (
            Ok("many\n".to_owned()),
            "cannot read the tasks of js-agent-crun.service: systemctl printed \"many\"",
        ),
        (
            Ok("-1\n".to_owned()),
            "cannot read the tasks of js-agent-crun.service: systemctl printed \"-1\"",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(&format!("hook-unit-cutover-unreadable-{n}"));
        rig.stage_changed_unit();
        let rig = running(rig);
        let spy = Spy::new(&rig);
        spy.tasks.borrow_mut().push_back(printed);
        let done = spy.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{n}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::StopFailed), "{n}");
        assert_eq!(done.report.class, Class::Environment, "{n}");
        assert_eq!(done.report.detail.as_deref(), Some(said), "{n}");
        assert_eq!(spy.act_names(), [format!("units.stop({UNIT})")], "{n}");
    }
}

/// Once the start is issued the agent that runs is the new one, and a rerun does not wait for an
/// empty cgroup it is in: a changed unit's cutover run twice does nothing the second time.
///
/// Catches a rerun that waits for ever on the cgroup of the agent it started.
#[test]
fn a_rerun_after_the_start_does_not_read_the_cgroup_of_the_agent_it_started() {
    let rig = Rig::new("hook-unit-cutover-twice");
    rig.stage_changed_unit();
    let rig = running(rig);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let before = end_state(&rig);
    let spy = Spy::new(&rig);
    spy.tasks.borrow_mut().push_back(Ok("9\n".to_owned()));
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(cgroup_reads(&spy), 0);
    assert_eq!(spy.act_names(), Vec::<String>::new());
    assert_eq!(end_state(&rig), before);
}

/// A cutover with a plan killed at every kill point and run again ends as an uninterrupted one.
///
/// Catches a wait whose state is in the process and not the record: a kill in or after it must
/// lead to the same end.
#[test]
fn a_cutover_with_a_plan_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let build = |name: &str| {
        let rig = Rig::new(name);
        rig.stage_changed_unit();
        running(rig)
    };
    let baseline = build("hook-unit-sweep-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    let end = end_state(&baseline);
    for k in 1..=points {
        let rig = build(&format!("hook-unit-sweep-{k}"));
        rig.kill.arm(k);
        assert!(killable(|| Spy::new(&rig).cutover(&rig)).is_none(), "{k}");
        rig.kill.disarm();
        let done = Spy::new(&rig).cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at {k}: {done:?}");
        assert_eq!(end_state(&rig), end, "killed at {k}");
    }
}

/// On a Mac the plan is recorded and reported and nothing more: `bootout` empties the job, so
/// cutover reads no cgroup. A changed plist is named by its file.
///
/// Catches `systemctl` run on a Mac, and a plist change that goes unrecorded.
#[test]
fn on_a_mac_a_changed_plist_is_recorded_and_reported_and_cutover_reads_no_cgroup() {
    let rig = Rig::mac("hook-unit-mac");
    rig.stage_changed_unit();
    rig.world
        .seed_unit(super::step_rig::MAC_UNIT, "active", "running");
    let spy = Spy::new(&rig);
    *spy.version.borrow_mut() = Ok("js-agent 1.4.0\nruntimes: docker native\n".to_owned());
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the unit changes (js-agent.plist): cutover starts the agent on the new unit")
    );
    assert_eq!(
        changed(rig.record().unwrap().unit_restart),
        ["js-agent.plist"]
    );
    rig.kill.disarm();
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(spy.log().iter().all(|call| !call.contains("systemctl")));
}

// The join token a first install needs.

/// Asserts that a first install whose token file shows `file` defers and changes nothing.
#[track_caller]
fn deferred_for_the_token(name: &str, file: TokenFile, said: &str) {
    let rig = Rig::first_install(name);
    let spy = Spy::new(&rig);
    spy.token.set(file);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred, "{name}: {done:?}");
    assert_eq!(done.outcome.code(), 75);
    assert_eq!(done.report.reason, Some(Reason::JoinTokenMissing), "{name}");
    assert_eq!(done.report.class, Class::Environment, "{name}");
    assert_eq!(
        done.report.retry_after_secs,
        Some(RETRY_AFTER_SECS),
        "{name}"
    );
    assert!(done.report.reversible, "{name}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the join token file {} {said}; place it, root-owned and mode 0600, to install the agent",
            rig.params.token_file.display()
        )),
        "{name}"
    );
    assert_eq!(rig.record_bytes(), None, "{name}: nothing is written");
    assert_eq!(rig.world.ticks(), 0, "{name}");
    assert_eq!(rig.cordon_expiry(), None, "{name}");
    assert!(
        spy.log().iter().all(|call| call.starts_with("run(")),
        "{name}: nothing is asked of the host"
    );
}

/// A first install defers (75, `join_token_missing`, retry in `defer_retry_secs`) until the
/// token file is there, a regular file, mode 0600 or 0400, the hook's user's; it changes nothing
/// meanwhile, and says which of the four is wrong, with the file's name and never its content.
///
/// Catches an install that goes on without a token (the agent would start and be refused), one
/// that takes a world-readable token for a token, and a deferral that writes anything.
#[test]
fn a_first_install_without_a_good_token_file_defers_and_changes_nothing() {
    deferred_for_the_token("hook-token-missing", TokenFile::Missing, "is not there");
    deferred_for_the_token(
        "hook-token-denied",
        TokenFile::Unreadable,
        "cannot be read: permission denied",
    );
    for mode in [0o644, 0o640, 0o660, 0o700, 0o4600, 0o000] {
        deferred_for_the_token(
            &format!("hook-token-mode-{mode:o}"),
            TokenFile::Facts(TokenFacts {
                mode,
                ..good_token()
            }),
            &format!("must be mode 0600 or 0400, not {mode:04o}"),
        );
    }
    deferred_for_the_token(
        "hook-token-owner",
        TokenFile::Facts(TokenFacts {
            uid: euid() + 1,
            ..good_token()
        }),
        &format!("must be owned by uid {}, not {}", euid(), euid() + 1),
    );
    deferred_for_the_token(
        "hook-token-not-regular",
        TokenFile::Facts(TokenFacts {
            regular: false,
            ..good_token()
        }),
        "is not a regular file",
    );
}

/// A token file of mode 0600 or 0400 owned by the hook's user is the token; once it is there the
/// install goes on.
///
/// Catches a check that refuses the 0400 file a secret mount gives, or that never lets an
/// install through.
#[test]
fn a_first_install_with_a_good_token_file_goes_on() {
    for mode in [0o600, 0o400] {
        let rig = Rig::first_install(&format!("hook-token-good-{mode:o}"));
        let spy = Spy::new(&rig);
        spy.token.set(TokenFile::Facts(TokenFacts {
            mode,
            ..good_token()
        }));
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{mode:o}: {done:?}");
        assert!(rig.record().is_some());
    }
    // The operator places the file after a deferral: the next prepare goes on.
    let rig = Rig::first_install("hook-token-placed-later");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Deferred);
    spy.token.set(TokenFile::Facts(good_token()));
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
}

/// A host credential in the state root is what the agent sends: a first install on a host that
/// has one needs no token file. A credential file that holds no credential is none.
///
/// Catches a host that joined once being held up for a token it no longer needs, and a damaged
/// credential file taken for a credential.
#[test]
fn a_host_credential_in_the_state_root_stands_in_for_the_token_file() {
    let rig = Rig::first_install("hook-token-credential");
    let root = rig.tmp.path().join("world/state");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    rig.state_root();
    assert!(!StateView::has_credential(&rig.state_root()));
    fs::write(root.join("host-credential"), "not a credential, has spaces").unwrap();
    assert!(!StateView::has_credential(&rig.state_root()));
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Deferred);
    fs::write(root.join("host-credential"), "cred-0123456789abcdefghij").unwrap();
    assert!(StateView::has_credential(&rig.state_root()));
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(rig.record().is_some());
}

/// Only a first install needs the token: an update's host has an agent that runs, and the token
/// file is the agent's business.
///
/// Catches the token check applied to an update, which would hold every update on a host whose
/// agent holds a host credential.
#[test]
fn an_update_does_not_look_at_the_token_file() {
    let rig = Rig::new("hook-token-update");
    rig.world.seed_unit(UNIT, "active", "running");
    let spy = Spy::new(&rig);
    spy.token.set(TokenFile::Missing);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    // The stop is the one thing nothing here reads: the unit is untouched.
    assert_eq!(
        rig.world.show(UNIT).unwrap().active,
        "active",
        "prepare stops nothing"
    );
}

/// Gives the staged release of `rig` the native package layout: its unit moves from `units/` to
/// `tree/usr/lib/systemd/system/` and its agent from `bin/` to `tree/usr/bin/`.
fn make_staged_native(rig: &Rig) {
    let staged = &rig.params.staged_release;
    let archive_unit = rig.unit_file(staged);
    let archive_agent = staged.join("bin/js-agent");
    let tree = staged.join("tree");
    fs::create_dir_all(tree.join("usr/bin")).unwrap();
    fs::create_dir_all(tree.join("usr/lib/systemd/system")).unwrap();
    fs::rename(&archive_unit, rig.unit_file(staged)).unwrap();
    fs::rename(&archive_agent, tree.join("usr/bin/js-agent")).unwrap();
}

/// A staged release that holds `tree/` is read in the native layout: the version check runs
/// `tree/usr/bin/js-agent` and its unit is `tree/usr/lib/systemd/system/<unit>`, compared with the
/// archive release that runs now (the first native update of a host that ran the archive).
///
/// Catches a native release read at the archive paths, which finds no agent and no unit, and a
/// comparison that does not read the previous release in its own layout.
#[test]
fn a_native_staged_release_is_read_from_its_tree_and_compared_with_an_archive_previous() {
    let rig = Rig::new("hook-unit-native-staged");
    // The archive unit is read before `tree/` exists, so the move below finds it.
    make_staged_native(&rig);
    let staged = rig.params.staged_release.clone();
    assert_eq!(
        rig.unit_file(&staged),
        staged.join("tree/usr/lib/systemd/system").join(UNIT)
    );
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        spy.log()[0],
        format!("run({}/tree/usr/bin/js-agent --version)", staged.display())
    );
    assert_eq!(rig.record().unwrap().unit_restart, None);

    let rig = Rig::new("hook-unit-native-staged-changed");
    make_staged_native(&rig);
    rig.stage_changed_unit();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        changed(rig.record().unwrap().unit_restart),
        ["Service.DelegateSubgroup", "Service.ExecStart"]
    );
}

/// A native staged release with no unit below `tree/` is refused as a release that carries no
/// unit, though an archive-style `units/` directory is beside it.
///
/// Catches a native release that falls back to the archive paths for its unit.
#[test]
fn a_native_staged_release_without_its_unit_in_the_tree_is_refused() {
    let rig = Rig::new("hook-unit-native-no-unit");
    let staged = rig.params.staged_release.clone();
    fs::create_dir_all(staged.join("tree/usr/bin")).unwrap();
    fs::rename(
        staged.join("bin/js-agent"),
        staged.join("tree/usr/bin/js-agent"),
    )
    .unwrap();
    let done = Spy::new(&rig).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::BadArtifact));
}
