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

/// Asserts that a shell that is not there is refused (status 2, nothing run or written), and
/// that one that passes the checks and still cannot be started (a file that is not
/// executable) is a failure (status 1) that names it. It catches a spawn error swallowed as
/// a success, and a shell that was not looked at.
#[test]
fn a_shell_that_is_missing_or_cannot_start_is_not_run() {
    let mut rig = Rig::new("enroll_no_shell");
    rig.host.shell = PathBuf::from("/nonexistent/bash");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("/nonexistent/bash"), "{ran:?}");
    assert_eq!(rig.setup_args(), None);
    assert_eq!(rig.names(), Vec::<String>::new());

    let plain = rig.dir.path().join("not-a-program");
    fs::write(&plain, "x").unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
    rig.host.shell = plain.clone();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(1), "{ran:?}");
    assert!(ran.err.contains("cannot run"), "{ran:?}");
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that a shell that others can write, or that is in a directory they can write, or
/// that is not root's, is refused (status 2) like the script. It catches a root process that
/// starts whatever program is at the shell's path.
#[test]
fn a_shell_others_can_replace_is_refused() {
    let mut rig = Rig::new("enroll_unfit_shell");
    let shell = rig.dir.path().join("shell");
    fs::copy("/usr/bin/true", &shell).unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o757)).unwrap();
    rig.host.shell = shell.clone();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("writable by group or others"), "{ran:?}");
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    rig.host.owner += 1;
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("is not owned by uid"), "{ran:?}");
    assert_eq!(rig.setup_args(), None);
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
    assert!(ran.err.contains("symbolic link"), "{ran:?}");
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that a script others can write (0757), a script in a directory others can write,
/// and a script that is not owned by the user the verb treats as root are refused (status 2)
/// and not run. It catches a script that can be swapped between the check and the run, and
/// an ownership check that only the real root could ever fail.
#[test]
fn a_script_others_can_replace_is_refused() {
    let mut rig = Rig::new("enroll_replaceable_script");
    let path = rig.host.setup_script.clone();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o757)).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("writable by group or others"), "{ran:?}");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

    fs::set_permissions(rig.dir.path(), fs::Permissions::from_mode(0o775)).unwrap();
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    let dir = rig.dir.path().display().to_string();
    assert!(
        ran.err.contains(&dir) && ran.err.contains("writable by group or others"),
        "{ran:?}"
    );
    fs::set_permissions(rig.dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

    rig.host.owner += 1;
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(2), "{ran:?}");
    assert!(ran.err.contains("is not owned by uid"), "{ran:?}");
    assert_eq!(rig.setup_args(), None, "an unfit script was run");
    assert_eq!(rig.names(), Vec::<String>::new());
}

/// Asserts that the script that is run is the file that was checked: it is started as
/// `/proc/self/fd/N` of the open, checked file, not by its path. It catches a check by path
/// followed by a run by path, which a swap in between defeats.
#[test]
fn the_script_runs_from_the_file_that_was_checked() {
    let rig = Rig::new("enroll_script_fd");
    rig.script_with(&format!(
        "printf '%s' \"$0\" > '{}'\nexit 0",
        rig.beside_script("zero")
    ));
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    let zero = fs::read_to_string(rig.beside_script("zero")).unwrap();
    assert!(zero.starts_with("/proc/self/fd/"), "{zero}");
}

/// Asserts that the script runs with umask 022 whatever the operator's was. (The test
/// process has the umask of the machine, usually the same; this pins the value the script
/// sees, not the cause.)
#[test]
fn the_script_runs_with_umask_022() {
    let rig = Rig::new("enroll_script_umask");
    rig.script_with(&format!("umask > '{}'\nexit 0", rig.beside_script("umask")));
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        fs::read_to_string(rig.beside_script("umask"))
            .unwrap()
            .trim(),
        "0022"
    );
}

/// Asserts that what the script prints reaches standard error as one stream in the order it
/// was printed (standard output and standard error of the script interleaved), and not the
/// whole of one after the whole of the other. It catches a buffer that is shown at the end.
#[test]
fn the_scripts_words_come_in_the_order_it_printed_them() {
    let rig = Rig::new("enroll_script_order");
    rig.script_with("echo third\nexit 0");
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    assert_eq!(
        ran.err, "stand-in setup output\nstand-in setup complaint\nthird\n",
        "{ran:?}"
    );
}

/// Asserts that a script interrupted (exit 130 and 143, as the script exits on SIGINT and
/// SIGTERM) is a failure with status 1 that names the manifest.
#[test]
fn an_interrupted_script_is_a_failure() {
    for code in [130, 143] {
        let rig = Rig::new("enroll_script_interrupted");
        rig.script(code);
        let ran = rig.go(&["--apply"], TOKEN);
        assert_eq!(ran.status, status(1), "{code}: {ran:?}");
        assert!(
            ran.err.contains(&format!("exit status Some({code})")),
            "{ran:?}"
        );
        assert_eq!(rig.names(), Vec::<String>::new());
    }
}

/// Asserts that the script runs as a root process should: with an empty environment but a
/// fixed `PATH` (and the shell's own `PWD`, `SHLVL`, `_`), not the caller's. It catches a
/// `PATH` or a `LD_PRELOAD` of the operator's session reaching a root script.
#[test]
fn the_script_runs_with_a_clean_environment() {
    let rig = Rig::new("enroll_env");
    rig.script_with(&format!("env > '{}'\nexit 0", rig.beside_script("env")));
    let ran = rig.go(&["--apply"], TOKEN);
    assert_eq!(ran.status, status(0), "{ran:?}");
    let text = fs::read_to_string(rig.beside_script("env")).unwrap();
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
