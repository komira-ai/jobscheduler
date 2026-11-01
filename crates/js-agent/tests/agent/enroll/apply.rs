//! The verb's two modes and its order: a dry run changes nothing, an apply does setup, then
//! the instance file, then the token, and a second apply finds nothing to do.

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::rig::{INSTANCE, LINE, PROVIDER, Rig, TOKEN, status};

/// Asserts that without `--apply` the host is read and nothing is written: the setup script
/// is run without `--apply` (its dry run), neither file is made, nothing is left in the
/// directory (no staged file), and the report says `planned`. It catches a dry run that
/// writes, or one that passes `--apply` on.
#[test]
fn a_dry_run_changes_nothing() {
    let rig = Rig::new("enroll_dry");
    let ran = rig.go(&[], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        ran.out,
        rig.report(["planned"; 3], "planned", TOKEN),
        "{ran:?}"
    );
    assert_eq!(rig.setup_args(), Some(vec![]), "the dry run has no --apply");
    assert_eq!(rig.names(), Vec::<String>::new());
    assert!(ran.err.contains("stand-in setup output"), "{ran:?}");
}

/// Asserts that an apply runs the setup with `--apply`, and only then writes the instance
/// file (`PROVIDER:INSTANCE`, mode 0644) and the token file (the token and a line break,
/// mode 0600), that the setup saw neither file, that nothing but the two files is left in
/// the directory, and that the report names the sha256 of the token and not the token. It
/// catches a wrong order, a wrong content or mode, and a staged file left behind.
#[test]
fn an_apply_runs_the_setup_then_writes_the_instance_file_then_the_token() {
    let rig = Rig::new("enroll_apply");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        ran.out,
        rig.report(["done", "written", "written"], "enrolled", TOKEN),
        "{ran:?}"
    );
    assert_eq!(rig.setup_args(), Some(vec!["--apply".to_owned()]));
    assert_eq!(rig.seen_by_setup(), Vec::<String>::new());
    assert_eq!(rig.read("instance"), LINE);
    assert_eq!(rig.mode("instance"), 0o644);
    assert_eq!(rig.read("token"), format!("{TOKEN}\n"));
    assert_eq!(rig.mode("token"), 0o600);
    assert_eq!(rig.names(), ["instance", "token"]);
}

/// Asserts that the token is also taken without a line break after it (`printf` into the
/// pipe) and is written with one. It catches a token that is refused for lacking the
/// newline `mint` prints, or one written without it.
#[test]
fn a_token_without_a_line_break_is_taken() {
    let rig = Rig::new("enroll_no_newline");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    let again = Rig::new("enroll_with_newline");
    let ran = again.go(&["--apply"], &format!("{TOKEN}\n"));
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(rig.read("token"), again.read("token"));
}

/// Asserts that a second apply on an enrolled host (the setup now finds nothing to do and
/// exits 3) writes nothing, exits 0 and says `already-enrolled`, that the files are the same
/// files (the same inodes), and that a dry run on it says the same. It catches a rerun that
/// rewrites, one that fails on the files being there, and one that reports a change.
#[test]
fn a_second_apply_finds_nothing_to_do() {
    let rig = Rig::new("enroll_idempotent");
    assert_eq!(rig.go(&["--apply"], TOKEN).status, status(0));
    let inodes = |rig: &Rig| {
        ["instance", "token"].map(|name| fs::metadata(rig.etc().join(name)).unwrap().ino())
    };
    let before = inodes(&rig);
    rig.script(3);
    let want = rig.report(
        ["nothing-to-do", "present", "present"],
        "already-enrolled",
        TOKEN,
    );
    for flags in [&["--apply"][..], &[][..]] {
        let ran = rig.go(flags, TOKEN);
        assert_eq!(ran.status, status(0), "{ran:?}");
        assert_eq!(ran.out, want, "{flags:?}");
    }
    assert_eq!(inodes(&rig), before);
    assert_eq!(rig.names(), ["instance", "token"]);
}

/// Asserts that when only the token is missing (the instance file is already right and the
/// setup is done) an apply writes the token alone and says `enrolled`. It catches a verb
/// that treats a half-enrolled host as done or as a conflict.
#[test]
fn a_half_enrolled_host_gets_what_is_missing() {
    let rig = Rig::new("enroll_half");
    rig.put("instance", LINE, 0o644);
    rig.script(3);
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        ran.out,
        rig.report(["nothing-to-do", "present", "written"], "enrolled", TOKEN)
    );
    assert_eq!(rig.read("token"), format!("{TOKEN}\n"));
}

/// Asserts that a failure to write the instance file (its directory is not there) ends the
/// run with status 1 after the setup, writes no token, and names the file and not the
/// token. It catches a token written before the instance, and a swallowed write error.
#[test]
fn a_failed_instance_write_stops_before_the_token() {
    let mut rig = Rig::new("enroll_instance_unwritable");
    rig.host.instance_file = rig.dir.path().join("nowhere").join("instance");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert!(
        ran.err.contains("nowhere/instance") && ran.err.contains("cannot write"),
        "{ran:?}"
    );
    assert_eq!(ran.out, "");
    assert_eq!(rig.names(), Vec::<String>::new());
    assert!(rig.setup_args().is_some());
}

/// Asserts that a failure to write the token (its directory is not there) ends the run with
/// status 1 after the instance file was written, which stays. It catches a token written
/// first, and an instance file that is rolled back or never written.
#[test]
fn a_failed_token_write_leaves_the_instance_file_written() {
    let mut rig = Rig::new("enroll_token_unwritable");
    rig.host.token_file = rig.dir.path().join("nowhere").join("token");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert!(ran.err.contains("nowhere/token"), "{ran:?}");
    assert_eq!(rig.read("instance"), LINE);
    assert_eq!(rig.names(), ["instance"]);
}

/// Asserts that the provider and the instance of the report are the ones given.
#[test]
fn the_report_names_the_host() {
    let rig = Rig::new("enroll_names");
    let ran = rig.go(&[], TOKEN);
    let last = ran.out.lines().last().unwrap();
    assert_eq!(
        last,
        format!("enroll: status=planned provider={PROVIDER} instance={INSTANCE}")
    );
}

/// Asserts that when only the instance file is missing (the token is right and the setup is
/// done) an apply writes the instance file alone and says `enrolled`. It catches a host that
/// is reported enrolled because its token is there.
#[test]
fn a_missing_instance_file_alone_is_written() {
    let rig = Rig::new("enroll_half_instance");
    rig.put("token", &format!("{TOKEN}\n"), 0o600);
    rig.script(3);
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        ran.out,
        rig.report(["nothing-to-do", "written", "present"], "enrolled", TOKEN)
    );
    assert_eq!(rig.read("instance"), LINE);
}
