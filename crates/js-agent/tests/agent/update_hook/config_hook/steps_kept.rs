//! The kept copy on the fakes: `prepare` keeps what is at each path the staged package places,
//! `rollback` puts it back before it restarts the unit, `verify` and `cleanup` remove it, and a
//! first install over a file a person placed rolls back onto that file.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use js_agent::update_hook::config::{Reason, Record, cleanup, cutover, rollback, verify};
use js_agent::update_hook::{Class, Outcome};

use super::rig::{Rig, Say};

/// A case of a test over many rigs: the rig's name, and what sets it up, which returns the
/// detail the step is expected to give.
type Case = (&'static str, fn(&Rig) -> String);

/// The flags the host ran on before the update.
const BEFORE: &str = "--server=https://before.example:7300\n";

/// The flags the package places.
const AFTER: &str = "--server=https://after.example:7300\n";

/// What a rollback of a host whose flags file names no host certificate says of the hello.
const NO_HELLO: &str = "not waiting for a hello: flags_file: the flags file names no --host-cert and --host-key: a host is identified by its certificate";

/// `path` as the hook names it: its directory's links resolved.
fn named(path: &Path) -> PathBuf {
    let dir = fs::canonicalize(path.parent().unwrap()).unwrap();
    dir.join(path.file_name().unwrap())
}

/// Writes `text` at `path` with `mode`, its directories made.
fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Stages `text` as the package's file `rel`.
fn stage(rig: &Rig, rel: &str, text: &str) {
    write(&rig.staged.join("tree").join(rel), text, 0o644);
}

/// Copies every file below `from` to the same path below `to`, each written beside its path and
/// renamed over it, as the installer replaces a file.
fn place(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            place(&entry.path(), &target);
            continue;
        }
        fs::create_dir_all(to).unwrap();
        let temp = to.join(".installing");
        fs::copy(entry.path(), &temp).unwrap();
        fs::rename(&temp, &target).unwrap();
    }
}

/// The installer's activate: the staged package's files placed below the rig's root.
fn install(rig: &Rig) {
    place(&rig.staged.join("tree"), &rig.root);
}

/// The kept copy's directory.
fn kept(rig: &Rig) -> PathBuf {
    rig.params.state_dir.join("kept")
}

/// What a first install's rollback says of the copy it leaves.
fn stays(rig: &Rig) -> String {
    format!(
        "; the copy stays in {}, as removing the package deletes the files it placed",
        kept(rig).display()
    )
}

/// The flags file's text.
fn flags_text(rig: &Rig) -> String {
    fs::read_to_string(&rig.params.flags_file).unwrap()
}

/// A rig over which `prepare` kept `BEFORE` (mode 0600) at the flags file, the installer placed
/// `AFTER`, and `cutover` restarted the unit onto it.
fn cut_over(rig: Rig) -> Rig {
    write(&rig.params.flags_file, BEFORE, 0o600);
    stage(&rig, "flags", AFTER);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    assert_eq!(flags_text(&rig), AFTER);
    rig
}

/// Asserts the whole of a failed first install over a flags file a person placed (mode 0640):
/// `cutover` restarts the unit on the package's flags, `verify` fails `no_hello`, and `rollback`
/// puts the person's bytes, mode, owner and group back before it restarts the unit, so the
/// unit's second start reads the original bytes; it says what it put back and that the copy
/// stays, since the package's removal that follows deletes the file.
///
/// Catches the rollback that restarted the agent on the new flags file and reported success,
/// leaving the host out of service.
#[test]
fn a_failed_first_install_over_a_hand_placed_flags_file_restarts_the_unit_on_the_original_bytes() {
    let rig = Rig::new("config-kept-first-install");
    write(&rig.params.flags_file, BEFORE, 0o640);
    let before = fs::metadata(&rig.params.flags_file).unwrap();
    stage(&rig, "flags", AFTER);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    let done = rig.run(verify);
    assert_eq!(done.report.reason, Some(Reason::NoHello), "{done:?}");

    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; the unit was restarted; {NO_HELLO}{}",
            named(&rig.params.flags_file).display(),
            stays(&rig)
        ))
    );
    assert_eq!(
        *rig.units.started_on.borrow(),
        [
            Some(AFTER.as_bytes().to_vec()),
            Some(BEFORE.as_bytes().to_vec())
        ]
    );
    let after = fs::symlink_metadata(&rig.params.flags_file).unwrap();
    assert!(after.file_type().is_file());
    assert_eq!(
        (after.mode() & 0o7777, after.uid(), after.gid()),
        (0o640, before.uid(), before.gid())
    );
    assert_eq!(flags_text(&rig), BEFORE);
    assert!(rig.record().is_none());
    assert!(kept(&rig).join("index.json").is_file());
}

/// Asserts that a rollback after the hook was killed between `cutover` and `verify` puts the
/// original flags back before it restarts the unit, and that a rerun of the rollback restarts
/// nothing and finds the file already as it was.
///
/// Catches a rollback that trusts the files in place after a crash, and one whose rerun
/// restarts the unit or writes again.
#[test]
fn a_rollback_after_a_crash_between_cutover_and_verify_puts_the_files_back_before_the_restart() {
    let rig = cut_over(Rig::new("config-kept-crash"));
    let flags = named(&rig.params.flags_file);
    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; the unit was restarted; {NO_HELLO}{}",
            flags.display(),
            stays(&rig)
        ))
    );
    assert_eq!(
        rig.units.started_on.borrow().last(),
        Some(&Some(BEFORE.as_bytes().to_vec()))
    );
    let done = rig.run(rollback);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "already as before the update: {}; cutover restarted nothing: the unit is left as it is{}",
            flags.display(),
            stays(&rig)
        ))
    );
    assert_eq!(rig.calls(), ["stop", "start", "stop", "start"]);
    assert_eq!(flags_text(&rig), BEFORE);
}

/// Asserts that a restore killed after it wrote its file beside the flags file, and before the
/// rename, is finished by the rerun: the file it left is replaced, and renamed over the flags.
///
/// Catches a rollback that a killed run wedges, refusing the file it left itself.
#[test]
fn a_rollback_replaces_the_file_a_killed_restore_left_beside_the_path() {
    let rig = cut_over(Rig::update("config-kept-leftover"));
    let leftover = rig.root.join(".flags.js-agent-config-restore");
    fs::write(&leftover, "--server=https://half").unwrap();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(flags_text(&rig), BEFORE);
    assert!(!leftover.exists());
    assert!(!kept(&rig).exists());
}

/// Asserts that a `verify` that sees the hello removes the kept copy and leaves the package's
/// files, and that `cleanup` then ends the update.
///
/// Catches a copy left behind after an update that took, which a later update's rollback
/// would put back.
#[test]
fn a_verify_that_succeeds_removes_the_kept_copy_and_leaves_the_new_files() {
    let rig = cut_over(Rig::new("config-kept-verified"));
    assert!(kept(&rig).join("index.json").is_file());
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    let done = rig.run(verify);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report, Default::default());
    assert!(!kept(&rig).exists());
    assert!(!rig.params.state_dir.join("kept.making").exists());
    assert_eq!(flags_text(&rig), AFTER);
    assert_eq!(rig.run(cleanup).outcome, Outcome::Success);
    assert!(rig.record().is_none());
}

/// Asserts that a copy `verify` cannot remove does not fail it (told in the detail, a rerun too),
/// that `cleanup` then fails `keep_failed` (class environment) keeping the record, and removes
/// the copy once nothing is in the way.
///
/// Catches a verify that rolls back an update that took because of its own bookkeeping, and a
/// cleanup that forgets the copy.
#[test]
fn a_copy_verify_cannot_remove_is_left_for_cleanup_which_removes_it() {
    let rig = cut_over(Rig::new("config-kept-in-the-way"));
    let making = rig.params.state_dir.join("kept.making");
    fs::write(&making, "in the way").unwrap();
    let left = format!(
        "the kept copy is left for cleanup: {}: Not a directory (os error 20)",
        making.display()
    );
    rig.lease.say(&[Say::Host("live", Some(2000))]);
    for _ in 0..2 {
        let done = rig.run(verify);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(done.report.detail.as_deref(), Some(left.as_str()));
    }
    assert!(rig.record().unwrap().verified);
    let done = rig.run(cleanup);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::KeepFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "{}: Not a directory (os error 20)",
            making.display()
        ))
    );
    assert!(rig.record().is_some());
    assert!(kept(&rig).exists());
    fs::remove_file(&making).unwrap();
    assert_eq!(rig.run(cleanup).outcome, Outcome::Success);
    assert!(!kept(&rig).exists());
    assert!(rig.record().is_none());
}

/// Asserts that a first install where no file was at the flags file says there was nothing to put
/// back, leaves the package's file for the installer's removal, restarts the unit as before and
/// removes the copy (it holds nothing the host could lose).
///
/// Catches a rollback that deletes the only flags file the agent can start on, and one that says
/// it restored a file that never was.
#[test]
fn a_first_install_with_no_file_before_says_there_is_nothing_to_put_back() {
    let rig = Rig::new("config-kept-nothing-before");
    stage(&rig, "flags", AFTER);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "not there before the update, so nothing to put back: {}; the unit was restarted; {NO_HELLO}",
            named(&rig.params.flags_file).display()
        ))
    );
    assert_eq!(flags_text(&rig), AFTER);
    assert_eq!(rig.calls(), ["stop", "start", "stop", "start"]);
    assert!(!kept(&rig).exists());
}

/// Asserts that the rollback of an update from a packaged version, whose previous files the
/// installer has put back, writes nothing (the file is the same inode), restarts the unit onto
/// those files as before, and removes the copy.
///
/// Catches a rollback that rewrites a package's file it has no reason to touch, and one that
/// keeps an update's copy after it is done.
#[test]
fn an_update_from_a_packaged_version_rolls_back_as_before_and_writes_nothing() {
    let rig = Rig::update("config-kept-update");
    write(&rig.params.flags_file, BEFORE, 0o644);
    stage(&rig, "flags", AFTER);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.lease.say(&[Say::Host("live", Some(1000))]);
    assert_eq!(rig.run(cutover).outcome, Outcome::Success);
    // The installer puts the previous version back before `rollback`.
    let previous = rig.root.join("previous");
    write(&previous.join("flags"), BEFORE, 0o644);
    place(&previous, &rig.root);
    let inode = fs::metadata(&rig.params.flags_file).unwrap().ino();
    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "already as before the update: {}; the unit was restarted; {NO_HELLO}",
            named(&rig.params.flags_file).display()
        ))
    );
    assert_eq!(fs::metadata(&rig.params.flags_file).unwrap().ino(), inode);
    assert_eq!(
        rig.units.started_on.borrow().last(),
        Some(&Some(BEFORE.as_bytes().to_vec()))
    );
    assert!(!kept(&rig).exists());
    assert!(rig.record().is_none());
}

/// Asserts that `prepare` keeps every path the staged package places, not only the flags file:
/// a file whose mode differs from the package's, one of the same mode with other bytes, a link,
/// and paths where nothing was (one in a directory the host does not have). A rollback after a
/// `cutover` that restarted nothing (`bad_config`) puts each back as it was without touching
/// the unit, and a rerun after one of them was deleted writes that one again and finds the
/// others as they were.
///
/// Catches a copy of the flags file alone (the certificate a package replaces stays the new
/// one), a link put back as a file, and a path that held nothing filled with something.
#[test]
fn prepare_keeps_every_path_the_package_places_and_rollback_puts_each_back_as_it_was() {
    let rig = Rig::new("config-kept-every-path");
    write(&rig.params.flags_file, BEFORE, 0o600);
    write(&rig.root.join("certs/ca.pem"), "old ca\n", 0o644);
    symlink("pools/a", rig.root.join("pool")).unwrap();
    stage(&rig, "flags", AFTER);
    stage(&rig, "certs/ca.pem", "new ca\n");
    stage(&rig, "certs/extra.pem", "extra\n");
    stage(&rig, "pool", "--pool=b\n");
    stage(&rig, "other/x.conf", "x\n");
    let ca = named(&rig.root.join("certs/ca.pem"));
    let extra = named(&rig.root.join("certs/extra.pem"));
    let flags = named(&rig.params.flags_file);
    let pool = named(&rig.root.join("pool"));
    let other = rig.root.join("other/x.conf");
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);

    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let mut absent = [extra.display().to_string(), other.display().to_string()];
    absent.sort();
    let absent = absent.join(", ");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}, {}, {}; not there before the update, so nothing to put back: {absent}; cutover restarted nothing: the unit is left as it is{}",
            ca.display(),
            flags.display(),
            pool.display(),
            stays(&rig)
        ))
    );
    assert!(rig.units.log.borrow().is_empty());
    assert_eq!(fs::read_to_string(&ca).unwrap(), "old ca\n");
    assert_eq!(fs::metadata(&ca).unwrap().mode() & 0o7777, 0o644);
    assert_eq!(flags_text(&rig), BEFORE);
    assert_eq!(fs::metadata(&flags).unwrap().mode() & 0o7777, 0o600);
    assert_eq!(fs::read_link(&pool).unwrap(), Path::new("pools/a"));
    assert_eq!(fs::read_to_string(&extra).unwrap(), "extra\n");
    assert_eq!(fs::read_to_string(&other).unwrap(), "x\n");

    fs::remove_file(&flags).unwrap();
    let done = rig.run(rollback);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; already as before the update: {}, {}; not there before the update, so nothing to put back: {absent}; cutover restarted nothing: the unit is left as it is{}",
            flags.display(),
            ca.display(),
            pool.display(),
            stays(&rig)
        ))
    );
    assert_eq!(flags_text(&rig), BEFORE);
}

/// Asserts that a path `prepare` cannot keep ends it `keep_failed`, class environment, with the
/// path and the reason, and keeps nothing: a directory where the package places a file, a path
/// below a file, a staged tree that is not a directory, and a copy's leftover that cannot be
/// removed.
///
/// Catches a prepare that lets the installer replace what it could not keep, and so a rollback
/// that cannot put it back.
#[test]
fn a_path_prepare_cannot_keep_fails_it_keep_failed_and_keeps_nothing() {
    let cases: [Case; 4] = [
        ("config-kept-refuse-dir", |rig| {
            stage(rig, "flags", AFTER);
            fs::create_dir(&rig.params.flags_file).unwrap();
            format!(
                "{}: is neither a file nor a link",
                named(&rig.params.flags_file).display()
            )
        }),
        ("config-kept-refuse-below-file", |rig| {
            fs::write(rig.root.join("certs"), "a file").unwrap();
            stage(rig, "certs/ca.pem", "new ca\n");
            let path = rig.root.join("certs/ca.pem");
            format!("{}: Not a directory (os error 20)", path.display())
        }),
        ("config-kept-refuse-tree", |rig| {
            write(&rig.staged.join("tree"), "a file", 0o644);
            format!(
                "{}: Not a directory (os error 20)",
                rig.staged.join("tree").display()
            )
        }),
        ("config-kept-refuse-leftover", |rig| {
            stage(rig, "flags", AFTER);
            let making = rig.params.state_dir.join("kept.making");
            write(&making, "a file", 0o600);
            format!("{}: Not a directory (os error 20)", making.display())
        }),
    ];
    for (name, set_up) in cases {
        let rig = Rig::new(name);
        let detail = set_up(&rig);
        let done = rig.prepare();
        assert_eq!(done.outcome, Outcome::Failure, "{name}");
        assert_eq!(done.report.reason, Some(Reason::KeepFailed), "{name}");
        assert_eq!(done.report.class, Class::Environment, "{name}");
        assert_eq!(done.report.detail, Some(detail), "{name}");
        assert!(!kept(&rig).exists(), "{name}");
    }
}

/// Asserts that a `prepare` that finds the copy of an earlier update that neither verified nor
/// rolled back (its `cutover` restarted the unit, then the update stopped) keeps that copy as it
/// is, and so a rollback of the new update puts back the files from before the first one.
///
/// Catches a prepare that takes the earlier update's files for the host's own, losing the only
/// copy of the file a person placed.
#[test]
fn prepare_keeps_the_copy_of_an_update_that_neither_verified_nor_rolled_back() {
    let rig = cut_over(Rig::new("config-kept-unfinished"));
    stage(&rig, "flags", "--server=https://next.example:7300\n");
    let done = rig.prepare();
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("kept as it is the copy of the files from before an update that did not finish")
    );
    assert!(rig.record().is_none());
    install(&rig);
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(flags_text(&rig), BEFORE);
}

/// Asserts that a `prepare` that finds a copy beside a record that says `verify` succeeded (the
/// earlier update took, and was killed before it removed its copy) replaces the copy with the
/// files now in place, so a rollback of the new update puts back the earlier update's files.
///
/// Catches a prepare that keeps a copy from before an update that took, and so a rollback that
/// goes back two versions.
#[test]
fn prepare_replaces_the_copy_of_an_update_whose_verify_succeeded() {
    let rig = Rig::update("config-kept-replaced");
    write(&rig.params.flags_file, BEFORE, 0o644);
    stage(&rig, "flags", AFTER);
    assert_eq!(rig.prepare().outcome, Outcome::Success);
    install(&rig);
    rig.put(&Record {
        session_read: true,
        restart_begun: true,
        restarted: true,
        verified: true,
        ..rig.fresh()
    });
    let done = rig.prepare();
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report, Default::default());
    write(
        &rig.params.flags_file,
        "--server=https://next.example:7300\n",
        0o644,
    );
    let done = rig.run(rollback);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; cutover restarted nothing: the unit is left as it is",
            named(&rig.params.flags_file).display()
        ))
    );
    assert_eq!(flags_text(&rig), AFTER);
}

/// Asserts that a rollback whose copy cannot be put back fails `restore_failed`, class
/// environment, with the path and the reason, before it touches the unit, and leaves the record
/// and the copy for a rerun: an index that is not JSON, one of a newer schema, one that names no
/// file in a directory, a directory where the restore writes its file, and a path whose directory
/// became a file.
///
/// Catches a rollback that restarts the unit on the package's files when it could not put the
/// host's back, and so reports success on the wrong config.
#[test]
fn a_rollback_that_cannot_put_the_files_back_fails_before_it_touches_the_unit() {
    let cases: [Case; 5] = [
        ("config-kept-index-json", |rig| {
            let index = kept(rig).join("index.json");
            fs::write(&index, "not json").unwrap();
            format!("{}: expected ident at line 1 column 2", index.display())
        }),
        ("config-kept-index-schema", |rig| {
            let index = kept(rig).join("index.json");
            fs::write(&index, r#"{"schema": 2, "entries": []}"#).unwrap();
            format!(
                "{}: is of a schema this hook does not read",
                index.display()
            )
        }),
        ("config-kept-index-root", |rig| {
            let index = kept(rig).join("index.json");
            let text = r#"{"schema": 1, "entries": [{"path": "/", "was": {"kind": "link", "target": "x", "uid": 0, "gid": 0}}]}"#;
            fs::write(&index, text).unwrap();
            "/: names no file in a directory".to_owned()
        }),
        ("config-kept-temp-dir", |rig| {
            let temp = named(&rig.root.join(".flags.js-agent-config-restore"));
            fs::create_dir(&temp).unwrap();
            format!("{}: ", temp.display())
        }),
        ("config-kept-dir-gone", |rig| {
            let certs = rig.root.join("certs");
            fs::remove_dir_all(&certs).unwrap();
            fs::write(&certs, "a file").unwrap();
            format!(
                "{}: Not a directory (os error 20)",
                named(&certs).join("ca.pem").display()
            )
        }),
    ];
    for (name, break_it) in cases {
        let rig = Rig::new(name);
        write(&rig.root.join("certs/ca.pem"), "old ca\n", 0o644);
        stage(&rig, "certs/ca.pem", "new ca\n");
        let rig = cut_over(rig);
        let detail = break_it(&rig);
        rig.units.log.borrow_mut().clear();
        let done = rig.run(rollback);
        assert_eq!(done.outcome, Outcome::Failure, "{name}");
        assert_eq!(done.report.reason, Some(Reason::RestoreFailed), "{name}");
        assert_eq!(done.report.class, Class::Environment, "{name}");
        let told = done.report.detail.unwrap();
        assert!(told.starts_with(&detail), "{name}: {told}");
        assert!(rig.calls().is_empty(), "{name}");
        assert!(rig.record().unwrap().restart_begun, "{name}");
        assert!(kept(&rig).exists(), "{name}");
    }
}

/// Asserts that a rollback whose copy cannot be removed at its end succeeds, having put the files
/// back and restarted the unit, and tells it in the detail.
///
/// Catches a rollback that fails, and so halts the host, over bookkeeping after its work is done.
#[test]
fn a_copy_a_rollback_cannot_remove_is_told_and_does_not_fail_it() {
    let rig = cut_over(Rig::update("config-kept-rollback-in-the-way"));
    let making = rig.params.state_dir.join("kept.making");
    fs::write(&making, "in the way").unwrap();
    rig.without_a_host_certificate();
    let done = rig.run(rollback);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail,
        Some(format!(
            "put back as before the update: {}; the unit was restarted; {NO_HELLO}; the kept copy could not be removed: {}: Not a directory (os error 20)",
            named(&rig.params.flags_file).display(),
            making.display()
        ))
    );
    assert_eq!(flags_text(&rig), BEFORE);
    assert!(rig.record().is_none());
}
