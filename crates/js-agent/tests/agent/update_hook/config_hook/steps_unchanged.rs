//! A config update whose files did not change, on the fakes: `cutover` compares the files in
//! place with this update's kept copy and restarts nothing when every one is as it was, and
//! `verify` then checks the agent that runs instead of waiting for a hello no restart causes. A
//! change of bytes, mode or presence, a copy of an earlier update and a copy that cannot be read
//! are each a change, and the unit is restarted as before.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use js_agent::update_hook::config::{Reason, cleanup, cutover, rollback, verify};
use js_agent::update_hook::{Class, Outcome};

use super::rig::{Rig, Say};

/// The flags the host runs on, and the package places again.
const FLAGS: &str = "--server=https://js.example:7300\n--pool=farm\n";

/// Other flags.
const OTHER: &str = "--server=https://js.example:7300\n--pool=farm-2\n";

/// What `cutover` says when the files did not change.
const UNCHANGED: &str =
    "the files the package placed are as they were before the update: nothing was restarted";

/// Writes `text` at `path` with `mode`, its directories made.
fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Stages `text` (mode 0644) as the package's file `rel`.
fn stage(rig: &Rig, rel: &str, text: &str) {
    write(&rig.staged.join("tree").join(rel), text, 0o644);
}

/// The installer's activate: each staged file copied beside its path below the rig's root and
/// renamed over it, its mode with it.
fn install(rig: &Rig) {
    let tree = rig.staged.join("tree");
    for entry in fs::read_dir(&tree).unwrap() {
        let entry = entry.unwrap();
        let target = rig.root.join(entry.file_name());
        let temp = rig.root.join(".installing");
        fs::copy(entry.path(), &temp).unwrap();
        fs::rename(&temp, &target).unwrap();
    }
}

/// The kept copy's directory.
fn kept(rig: &Rig) -> PathBuf {
    rig.params.state_dir.join("kept")
}

/// `path` as the hook names it: its directory's links resolved.
fn named(path: &Path) -> PathBuf {
    let dir = fs::canonicalize(path.parent().unwrap()).unwrap();
    dir.join(path.file_name().unwrap())
}

/// The update up to its `cutover`: the host runs on `FLAGS` (mode 0644), the package stages
/// `staged` as the flags file, `prepare` keeps the copy, the installer places the package, and
/// the server shows the host live in session 1000.
fn update_to_cutover(rig: &Rig, staged: &str) {
    write(&rig.params.flags_file, FLAGS, 0o644);
    stage(rig, "flags", staged);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
}

/// Asserts that a package that places the flags the host already runs on is applied at once:
/// `cutover` restarts nothing and asks the server nothing, the record says the files are
/// unchanged, `verify` succeeds at its first look on the running agent's own session (the one
/// from before the update), the kept copy is removed and `cleanup` finishes.
///
/// Catches a re-versioned config package whose `verify` waits out its bound for a new session
/// that no restart causes and fails `no_hello`, and one that restarts the agent for nothing.
#[test]
fn unchanged_files_restart_nothing_and_verify_passes_at_the_first_look() {
    let rig = Rig::update("config-unchanged-whole");
    update_to_cutover(&rig, FLAGS);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(UNCHANGED));
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert_eq!(rig.lease.looks.get(), 0);
    let record = rig.record().unwrap();
    assert_eq!(
        (
            record.unchanged,
            record.session_read,
            record.restart_begun,
            record.restarted,
            record.verified
        ),
        (true, false, false, false, false)
    );

    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert_eq!(rig.clock.pauses.get(), 0);
    assert_eq!(rig.lease.looks.get(), 1);
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert!(rig.record().unwrap().verified);
    assert!(!kept(&rig).exists());
    assert_eq!(rig.run(cleanup).outcome, Outcome::Success);
    assert!(rig.record().is_none());
}

/// Asserts that a package whose flags differ from the ones in place restarts the unit as before,
/// and that `verify` then waits past the running agent's session for the new one.
///
/// Catches a comparison that takes changed files for unchanged ones, and a `verify` that takes
/// the old agent for the new one after a restart.
#[test]
fn changed_files_restart_and_verify_waits_for_the_new_session() {
    let rig = Rig::update("config-unchanged-changed");
    update_to_cutover(&rig, OTHER);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert_eq!(rig.calls(), ["stop", "start"]);
    let record = rig.record().unwrap();
    assert_eq!(
        (
            record.unchanged,
            record.restarted,
            record.session_started_ms
        ),
        (false, true, Some(1000))
    );
    rig.lease.say(&[
        Say::Host("live", Some(1000)),
        Say::Host("live", Some(1000)),
        Say::Host("live", Some(2000)),
    ]);
    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert!(rig.record().unwrap().verified);
}

/// Asserts that a change of the mode alone, a file the package places where none was, and a
/// file that was kept and is gone are each a change: the unit is restarted.
///
/// Catches a comparison of bytes only (a certificate's key made readable to all is a change the
/// agent must be restarted on), and one that skips a path the copy says held nothing.
#[test]
fn a_mode_a_new_file_or_a_file_gone_is_a_change() {
    type Setup = fn(&Rig);
    let cases: [(&str, Setup); 3] = [
        ("config-unchanged-mode", |rig| {
            write(&rig.params.flags_file, FLAGS, 0o600);
            stage(rig, "flags", FLAGS);
            assert_eq!(rig.prepare().outcome, Outcome::Success);
            install(rig);
        }),
        ("config-unchanged-new-file", |rig| {
            write(&rig.params.flags_file, FLAGS, 0o644);
            stage(rig, "flags", FLAGS);
            stage(rig, "ca.pem", "a certificate\n");
            assert_eq!(rig.prepare().outcome, Outcome::Success);
            install(rig);
        }),
        ("config-unchanged-gone", |rig| {
            write(&rig.params.flags_file, FLAGS, 0o644);
            write(&rig.root.join("ca.pem"), "a certificate\n", 0o644);
            stage(rig, "ca.pem", "a certificate\n");
            assert_eq!(rig.prepare().outcome, Outcome::Success);
            install(rig);
            fs::remove_file(&rig.params.flags_file).unwrap();
        }),
    ];
    for (name, setup) in cases {
        let rig = Rig::update(name);
        setup(&rig);
        rig.lease.say(&[Say::Host("live", Some(1000))]);
        let done = rig.run(cutover);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(rig.calls(), ["stop", "start"], "{name}");
        let record = rig.record().unwrap();
        assert_eq!(
            (record.unchanged, record.restarted),
            (false, true),
            "{name}"
        );
    }
}

/// Asserts that a path where nothing was before the update and nothing is after it counts as
/// unchanged: a package that places only a certificate that is already there, on a host with no
/// flags file, restarts nothing.
///
/// Catches a comparison that takes every path the copy holds nothing for as a change.
#[test]
fn nothing_before_and_nothing_after_is_unchanged() {
    let rig = Rig::update("config-unchanged-nothing");
    write(&rig.root.join("ca.pem"), "a certificate\n", 0o644);
    stage(&rig, "ca.pem", "a certificate\n");
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(UNCHANGED));
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert!(rig.record().unwrap().unchanged);
}

/// Asserts that a `cutover` run again after it found the files unchanged restarts nothing, asks
/// nothing and leaves the record as it was, even when the files have changed since.
///
/// Catches a retried cutover that restarts the agent after the first run found nothing to do,
/// which would make `verify` check the running agent of a restart it does not wait for.
#[test]
fn a_cutover_run_again_after_finding_the_files_unchanged_restarts_nothing() {
    let rig = Rig::update("config-unchanged-rerun");
    update_to_cutover(&rig, FLAGS);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    let record = rig.record();
    write(&rig.params.flags_file, OTHER, 0o644);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(UNCHANGED));
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert_eq!(rig.lease.looks.get(), 0);
    assert_eq!(rig.record(), record);
}

/// Asserts that a `cutover` run again after a restart it began and could not finish (the stop
/// failed) restarts the unit again, even when the files in place are by then as they were before
/// the update: the stop may have reached the agent.
///
/// Catches a rerun that compares the files after a restart was begun and leaves an agent that
/// was stopped, or restarted onto other files, for `verify` to take as the one that runs.
#[test]
fn a_cutover_run_again_after_a_restart_began_restarts_whatever_the_files() {
    let rig = Rig::update("config-unchanged-restart-begun");
    update_to_cutover(&rig, OTHER);
    rig.units.fail.borrow_mut().push("stop");
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::StopFailed), "{done:?}");
    assert!(rig.record().unwrap().restart_begun);
    rig.units.fail.borrow_mut().clear();
    rig.units.log.borrow_mut().clear();
    write(&rig.params.flags_file, FLAGS, 0o644);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["stop", "start"]);
    let record = rig.record().unwrap();
    assert_eq!((record.unchanged, record.restarted), (false, true));
}

/// Asserts that a flags file the agent cannot use ends `cutover` as `bad_config` before the
/// files are compared, even when they did not change.
///
/// Catches an unchanged update that skips the check `cutover` makes of every flags file.
#[test]
fn an_unchanged_flags_file_the_agent_cannot_use_is_still_bad_config() {
    let rig = Rig::update("config-unchanged-bad-flags");
    update_to_cutover(&rig, FLAGS);
    *rig.flags.borrow_mut() = Some("flags_file: not a flag".to_owned());
    let done = rig.run(cutover);
    assert_eq!(done.report.reason, Some(Reason::BadConfig), "{done:?}");
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    assert!(rig.record().is_none());
}

/// Asserts that, when `prepare` finds the copy of an earlier update that did not finish, it
/// marks it (twice over, for a rerun of `prepare`), and `cutover` restarts the unit even though
/// the files in place equal that copy: the agent may run on the earlier update's files.
///
/// Catches a `cutover` that skips the restart after an update that placed other flags, restarted
/// the agent onto them and never rolled back, when the next update places the first flags again.
#[test]
fn a_copy_kept_from_an_earlier_update_is_never_unchanged() {
    let rig = Rig::update("config-unchanged-earlier");
    update_to_cutover(&rig, OTHER);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    rig.units.log.borrow_mut().clear();
    stage(&rig, "flags", FLAGS);
    for _ in 0..2 {
        let done = rig.prepare();
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some("kept as it is the copy of the files from before an update that did not finish")
        );
        assert!(kept(&rig).join("earlier").is_file());
    }
    install(&rig);
    assert_eq!(fs::read_to_string(&rig.params.flags_file).unwrap(), FLAGS);
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["stop", "start"]);
    assert!(!rig.record().unwrap().unchanged);
}

/// Asserts that a kept copy whose index cannot be read is taken as a change: the unit is
/// restarted and the detail says why the files were not compared.
///
/// Catches a comparison whose failure either halts the update or skips the restart.
#[test]
fn a_copy_that_cannot_be_compared_is_a_change() {
    let rig = Rig::update("config-unchanged-unreadable");
    update_to_cutover(&rig, FLAGS);
    let index = kept(&rig).join("index.json");
    fs::write(&index, "not an index").unwrap();
    let done = rig.run(cutover);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.calls(), ["stop", "start"]);
    let detail = done.report.detail.unwrap();
    let opening = format!(
        "the files in place could not be compared with the kept copy ({}: ",
        index.display()
    );
    assert!(detail.starts_with(&opening), "{detail}");
    assert!(detail.ends_with("): the unit was restarted"), "{detail}");
    assert!(rig.record().unwrap().restarted);
}

/// Asserts that, with the files unchanged, a unit that is down ends `verify` at once as
/// `agent_down`, class environment, naming the agent's unit, without asking the server; and that
/// the `rollback` that follows writes nothing and restarts nothing.
///
/// Catches a down agent reported as `no_hello` (a fault of the config) after a wait of the whole
/// bound, and a rollback that restarts an agent the update never touched.
#[test]
fn a_down_agent_with_unchanged_files_is_agent_down() {
    for (active, sub) in [("failed", "failed"), ("inactive", "dead")] {
        let rig = Rig::update("config-unchanged-down");
        update_to_cutover(&rig, FLAGS);
        assert_eq!(rig.run(cutover).outcome, Outcome::Success);
        rig.units.is(active, sub);
        let done = rig.run(verify);
        assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
        assert_eq!(done.report.reason, Some(Reason::AgentDown));
        assert_eq!(done.report.class, Class::Environment);
        assert_eq!(
            done.report.detail,
            Some(format!(
                "the agent's unit {} is {active} ({sub}): the files did not change, so nothing \
                 was restarted, and the agent is not running",
                rig.params.unit
            ))
        );
        assert_eq!(rig.lease.looks.get(), 0);
        assert_eq!(rig.clock.pauses.get(), 0);
        assert!(!rig.record().unwrap().verified);

        let done = rig.run(rollback);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(
            done.report.detail,
            Some(format!(
                "already as before the update: {}; cutover restarted nothing: the unit is left \
                 as it is",
                named(&rig.params.flags_file).display()
            ))
        );
        assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    }
}

/// Asserts that, with the files unchanged, a running unit whose host the server shows other than
/// live (or does not know) through the bound ends `verify` as `agent_not_live`, class
/// environment, after the whole bound; and that a server that could not be asked at the last
/// look ends it `lease_unreachable`.
///
/// Catches an agent that runs and is not in the job service reported as `no_hello`, and a
/// server outage blamed on the agent.
#[test]
fn a_running_agent_the_server_does_not_show_live_is_agent_not_live() {
    for say in [Say::Host("awaiting", Some(1000)), Say::Unknown] {
        let rig = Rig::update("config-unchanged-not-live");
        update_to_cutover(&rig, FLAGS);
        assert_eq!(rig.run(cutover).outcome, Outcome::Success);
        rig.lease.say(&[say]);
        let done = rig.run(verify);
        assert_eq!(done.outcome, Outcome::Failure, "{say:?}: {done:?}");
        assert_eq!(done.report.reason, Some(Reason::AgentNotLive), "{say:?}");
        assert_eq!(done.report.class, Class::Environment, "{say:?}");
        assert_eq!(
            done.report.detail,
            Some(format!(
                "the agent's unit {} runs, but the lease server did not show the host live in \
                 285 s: the files did not change, so nothing was restarted",
                rig.params.unit
            )),
            "{say:?}"
        );
        assert_eq!(rig.clock.pauses.get(), 29, "{say:?}");
        assert!(!rig.record().unwrap().verified, "{say:?}");
    }
    let rig = Rig::update("config-unchanged-server-down");
    update_to_cutover(&rig, FLAGS);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    rig.lease.say(&[Say::Down("connection refused")]);
    let done = rig.run(verify);
    assert_eq!(
        done.report.reason,
        Some(Reason::LeaseUnreachable),
        "{done:?}"
    );
    assert_eq!(done.report.class, Class::Environment);
}

/// Asserts that, with the files unchanged, a unit that is starting (launchd between two starts)
/// and a host that is not yet live are waited for, and the step succeeds when both are.
///
/// Catches a `verify` of an unchanged update that fails an agent which is coming back on its own.
#[test]
fn unchanged_files_wait_for_a_starting_unit_and_a_host_coming_live() {
    let rig = Rig::update("config-unchanged-starting");
    update_to_cutover(&rig, FLAGS);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    rig.units
        .follow(&[("activating", "auto-restart"), ("active", "running")]);
    rig.lease
        .say(&[Say::Host("awaiting", None), Say::Host("live", Some(3000))]);
    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 2);
    assert_eq!(rig.lease.looks.get(), 2);
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
}

/// Asserts that the record of an update whose files changed is written without the `unchanged`
/// mark, as the hook wrote it before the mark existed, and that one whose files did not change
/// carries it.
///
/// Catches a record an earlier js-agent, which refuses an unknown field, could not read for an
/// update it could have finished.
#[test]
fn the_unchanged_mark_is_written_only_when_it_is_true() {
    let changed = Rig::update("config-unchanged-file-changed");
    update_to_cutover(&changed, OTHER);
    assert_eq!(changed.run(cutover).outcome, Outcome::Success);
    let text = fs::read_to_string(changed.record_path()).unwrap();
    assert!(!text.contains("unchanged"), "{text}");

    let same = Rig::update("config-unchanged-file-same");
    update_to_cutover(&same, FLAGS);
    assert_eq!(same.run(cutover).outcome, Outcome::Success);
    let text = fs::read_to_string(same.record_path()).unwrap();
    assert!(text.contains("\"unchanged\": true"), "{text}");
}

/// Asserts that a record written before the `unchanged` mark existed (no such field) is read as
/// one whose files changed: `verify` waits for the new session of the restart it records.
///
/// Catches a hook that cannot read the record an earlier js-agent's `cutover` left for an update
/// still under way.
#[test]
fn a_record_without_the_unchanged_mark_is_read_as_a_change() {
    let rig = Rig::update("config-unchanged-old-record");
    fs::create_dir_all(&rig.params.state_dir).unwrap();
    let old = format!(
        "{{\"schema\":1,\"unit\":\"{}\",\"session_read\":true,\"session_started_ms\":1000,\
         \"restart_begun\":true,\"restarted\":true,\"verified\":false}}",
        rig.params.unit
    );
    fs::write(rig.record_path(), old).unwrap();
    assert!(!rig.record().unwrap().unchanged);
    rig.lease
        .say(&[Say::Host("live", Some(1000)), Say::Host("live", Some(2000))]);
    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.clock.pauses.get(), 1);
    assert!(rig.record().unwrap().verified);
}
