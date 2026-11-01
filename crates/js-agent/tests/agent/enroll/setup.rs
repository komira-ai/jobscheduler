//! The packaged setup script: how its exit statuses are read, what is passed to it, and what
//! is refused when it is missing or not fit to run as root.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::rig::{Rig, TOKEN, status};

/// Asserts that the script's exit 0 (done) and exit 3 (nothing to do) both let the verb go
/// on to the files, with the word each has in the report. It catches a 3 read as a failure,
/// which would make a second run of the script's own idempotence fail the verb.
#[test]
fn setup_status_0_and_3_go_on() {
    for (code, word) in [(0, "done"), (3, "nothing-to-do")] {
        let rig = Rig::new("enroll_setup_continue");
        rig.script(code);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(0), "{code}: {ran:?}");
        assert!(
            ran.out
                .starts_with(&format!("enroll: setup status={word}\n")),
            "{code}: {ran:?}"
        );
        assert_eq!(rig.names(), ["instance", "token"]);
    }
}

/// Asserts that the script's exit 2 (refused, nothing changed) ends the verb with status 2
/// before either file is written, and that the script's own words reach standard error. It
/// catches a refusal that is ignored, and one that is reported as a failure.
#[test]
fn setup_status_2_is_a_refusal_and_writes_nothing() {
    let rig = Rig::new("enroll_setup_refused");
    rig.script(2);
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert_eq!(ran.out, "");
    assert!(
        ran.err.contains("stand-in setup complaint") && ran.err.contains("the host setup refused"),
        "{ran:?}"
    );
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that the script's exit 1, any other status, and an end by a signal are failures
/// (status 1) that write neither file and name the manifest the script keeps. It catches a
/// failed setup followed by a token write.
#[test]
fn a_failed_setup_is_status_1_and_writes_nothing() {
    for last in ["exit 1", "exit 7", "kill -KILL $$"] {
        let rig = Rig::new("enroll_setup_failed");
        rig.script_with(last);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(1), "{last}: {ran:?}");
        assert_eq!(ran.out, "", "{last}");
        assert!(
            ran.err.contains("the host setup failed")
                && ran.err.contains("/var/lib/js-crun-host-setup/manifest"),
            "{last}: {ran:?}"
        );
        assert_eq!(rig.names(), Vec::<String>::new(), "{last}");
    }
}

/// Asserts that a dry run whose setup would be refused (exit 2) is a refusal too, so a dry
/// run shows what an apply would stop at. It catches a dry run that says `planned` for a
/// host the apply would refuse.
#[test]
fn a_dry_run_shows_the_setups_refusal() {
    let rig = Rig::new("enroll_setup_dry_refused");
    rig.script(2);
    let ran = rig.go(&[], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert_eq!(ran.out, "");
}

/// Asserts that a shell that cannot be started is a failure (status 1) that names the shell.
/// It catches a spawn error that is swallowed as a success.
#[test]
fn a_shell_that_cannot_start_is_a_failure() {
    let mut rig = Rig::new("enroll_no_shell");
    rig.host.shell = PathBuf::from("/nonexistent/bash");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert!(ran.err.contains("/nonexistent/bash"), "{ran:?}");
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that a script that is not there is refused (status 2) with its path and the
/// package that ships it, before anything is run or written, in a dry run as well. It
/// catches a verb that goes on without the setup, and an unhelpful message.
#[test]
fn a_missing_script_is_refused() {
    for flags in [&["--apply"][..], &[][..]] {
        let rig = Rig::new("enroll_no_script");
        fs::remove_file(&rig.host.setup_script).unwrap();
        let ran = rig.go(flags, TOKEN);
        assert_eq!(ran.status, status(2), "{ran:?}");
        let path = rig.host.setup_script.display().to_string();
        assert!(
            ran.err.contains(&path) && ran.err.contains("js-agent deb"),
            "{ran:?}"
        );
        assert_eq!(rig.names(), Vec::<String>::new());
    }
}

/// Asserts that a script others can write, a directory and a symbolic link in its place are
/// refused (status 2) and not run: a script that root runs must not be one anybody else can
/// change. It catches a verb that runs whatever is at the path.
#[test]
fn a_script_that_is_not_fit_to_run_as_root_is_refused() {
    let rig = Rig::new("enroll_unfit_script");
    let path = rig.host.setup_script.clone();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o775)).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("writable by group or others"), "{ran:?}");
    assert_eq!(rig.setup_args(), None, "an unfit script was run");

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("not a regular file"), "{ran:?}");

    fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", &path).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("not a regular file"), "{ran:?}");
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that the script runs as a root process should: with an empty environment but a
/// fixed `PATH` (and the shell's own `PWD`, `SHLVL`, `_`), not the caller's. It catches a
/// `PATH` or a `LD_PRELOAD` of the operator's session reaching a root script.
#[test]
fn the_script_runs_with_a_clean_environment() {
    let rig = Rig::new("enroll_env");
    rig.script_with("env > \"$0.env\"\nexit 0");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    let text = fs::read_to_string(format!("{}.env", rig.host.setup_script.display())).unwrap();
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        .collect();
    let own = ["PATH", "LC_ALL", "PWD", "SHLVL", "_", "OLDPWD"];
    assert!(names.iter().all(|name| own.contains(name)), "{names:?}");
    assert!(
        text.contains("PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"),
        "{text}"
    );
}
