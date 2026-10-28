//! The probe steps: each prints `PROBE <name> ok` or `PROBE <name> errno=<n>`, and the
//! processes that hold a listener or a name for the test to look at.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use js_testkit::reaped;

use super::{act, dir, eventually, output, text};

/// The one line `args` (a single probe step) print, the run having exited 0 with nothing
/// on stderr, in `cwd` when given.
fn probe_in(cwd: Option<&Path>, args: &[&str]) -> String {
    let mut command = act(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let out = command.output().expect("kbf-act runs");
    assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
    assert_eq!(text(&out.stderr), "", "{args:?}");
    let said = text(&out.stdout);
    assert_eq!(said.lines().count(), 1, "{args:?}: {said:?}");
    said.trim_end().to_owned()
}

fn probe(args: &[&str]) -> String {
    probe_in(None, args)
}

fn errno(name: &str, n: i32) -> String {
    format!("PROBE {name} errno={n}")
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// Starts `kbf-act` with `args` in `cwd`, stdout piped, and waits for the line it
/// prints first. The guard stops it when dropped.
fn start(cwd: &Path, args: &[&str]) -> (reaped::Reaped, String) {
    let mut command = act(args);
    command.current_dir(cwd).stdout(Stdio::piped());
    let mut child = reaped::spawn(&mut command, 3).expect("starts");
    let stdout = child.stdout.take().expect("stdout");
    let (first, _reader) =
        reaped::first_line(stdout, Duration::from_secs(10)).expect("a first line");
    (child, first.trim_end().to_owned())
}

/// Catches: a file probe that prints other than `PROBE <step> ok` / `errno=<n>` with
/// the OS number (a test of slot_reach maps the lines by position to its rows), that
/// exits non-zero when the attempt fails, or that prints two lines or none;
/// `try-write` creating the file it was asked to try, truncating or writing to one
/// that is there; `try-list` of a file, or of a directory that is not there, reporting
/// `ok`; a refusal by permission mode (not root: root is refused nothing) reported as
/// `ok`; probes in one run printing out of order.
#[test]
fn file_probes_print_ok_or_the_errno_and_exit_0() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("act-probe-files");
    let (file, missing, sub) = (dir.join("f"), dir.join("missing"), dir.join("d"));
    std::fs::write(&file, "hello").expect("file");
    std::fs::create_dir(&sub).expect("sub");
    let (file_arg, missing_arg, sub_arg) = (display(&file), display(&missing), display(&sub));
    assert_eq!(probe(&["try-read", &file_arg]), "PROBE try-read ok");
    assert_eq!(
        probe(&["try-read", &missing_arg]),
        errno("try-read", libc::ENOENT)
    );
    assert_eq!(probe(&["try-write", &file_arg]), "PROBE try-write ok");
    assert_eq!(
        std::fs::read_to_string(&file).expect("file"),
        "hello",
        "try-write truncated or wrote"
    );
    assert_eq!(
        probe(&["try-write", &missing_arg]),
        errno("try-write", libc::ENOENT)
    );
    assert!(!missing.exists(), "try-write created the file");
    assert_eq!(probe(&["try-list", &sub_arg]), "PROBE try-list ok");
    std::fs::write(sub.join("entry"), "").expect("entry");
    assert_eq!(probe(&["try-list", &sub_arg]), "PROBE try-list ok");
    assert_eq!(
        probe(&["try-list", &missing_arg]),
        errno("try-list", libc::ENOENT)
    );
    assert_eq!(
        probe(&["try-list", &file_arg]),
        errno("try-list", libc::ENOTDIR)
    );

    let out = output(&[
        "try-read",
        &missing_arg,
        ";",
        "try-read",
        &file_arg,
        ";",
        "try-list",
        &sub_arg,
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        format!(
            "{}\nPROBE try-read ok\nPROBE try-list ok\n",
            errno("try-read", libc::ENOENT)
        )
    );

    // A mode that refuses the owner: root is refused nothing, so there is nothing to see.
    if !rustix::process::geteuid().is_root() {
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).expect("locked");
        std::fs::write(locked.join("inside"), "").expect("inside");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let locked_arg = display(&locked);
        let seen = [
            probe(&["try-read", &file_arg]),
            probe(&["try-write", &file_arg]),
            probe(&["try-list", &locked_arg]),
        ];
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            seen,
            [
                errno("try-read", libc::EACCES),
                errno("try-write", libc::EACCES),
                errno("try-list", libc::EACCES)
            ]
        );
    }
}

/// Catches: `tcp-listen` that does not hold the listener for its seconds (the port
/// refuses at once) or holds it after a bind that failed; a port file that is not the
/// bound port (a connect to it must reach the listener), that is there before it is
/// whole (the part file left behind), or written when the bind failed; `tcp-connect`
/// reporting `ok` for a port nothing listens on, or any failure other than the OS's
/// number; a probe that exits non-zero.
#[test]
fn tcp_probes_listen_hold_and_connect() {
    let dir = dir("act-probe-tcp");
    let portfile = display(&dir.join("port"));
    let (listener, first) = start(&dir, &["tcp-listen", "127.0.0.1:0", &portfile, "60"]);
    assert_eq!(first, "PROBE tcp-listen ok");
    let port: u16 = std::fs::read_to_string(&portfile)
        .expect("port file")
        .trim()
        .parse()
        .expect("a port");
    assert_ne!(
        port, 0,
        "the port file holds the requested port, not the bound one"
    );
    assert!(!dir.join("port.part").exists(), "the part file stays");
    let addr = format!("127.0.0.1:{port}");
    assert_eq!(probe(&["tcp-connect", &addr]), "PROBE tcp-connect ok");
    drop(listener);
    let dead = js_testkit::port::dead_addr().to_string();
    assert_eq!(
        probe(&["tcp-connect", &dead]),
        errno("tcp-connect", libc::ECONNREFUSED)
    );

    // It holds for its seconds, then goes (its port is no longer taken).
    let held = display(&dir.join("held"));
    let began = Instant::now();
    assert_eq!(
        probe(&["tcp-listen", "127.0.0.1:0", &held, "0.5"]),
        "PROBE tcp-listen ok"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(450),
        "tcp-listen let go after {:?}",
        began.elapsed()
    );
    assert!(Path::new(&held).exists());

    // A failed bind says why, writes no file and does not hold.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let taken_addr = taken.local_addr().expect("addr").to_string();
    let none = dir.join("none");
    let began = Instant::now();
    assert_eq!(
        probe(&["tcp-listen", &taken_addr, &display(&none), "30"]),
        errno("tcp-listen", libc::EADDRINUSE)
    );
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "held after a failed bind"
    );
    assert!(!none.exists(), "a port file for a failed bind");
}

/// Catches: `unix-listen` leaving the socket at the umask's mode (a connect by another
/// user is then refused by the file, not by what a test probes), not holding the
/// listener, or holding after a failed bind; `unix-connect` reporting `ok` for a
/// socket nobody listens on or for a path that is not there. (The paths are relative:
/// a socket path is short.)
#[test]
fn unix_probes_listen_hold_and_connect() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("act-probe-unix");
    let (listener, first) = start(&dir, &["unix-listen", "s", "60"]);
    assert_eq!(first, "PROBE unix-listen ok");
    let mode = std::fs::metadata(dir.join("s"))
        .expect("socket")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o777, "the socket's mode is {mode:o}");
    let here = Some(dir.path());
    assert_eq!(
        probe_in(here, &["unix-connect", "s"]),
        "PROBE unix-connect ok"
    );
    assert_eq!(
        probe_in(here, &["unix-connect", "missing"]),
        errno("unix-connect", libc::ENOENT)
    );
    // The path is taken.
    let began = Instant::now();
    assert_eq!(
        probe_in(here, &["unix-listen", "s", "30"]),
        errno("unix-listen", libc::EADDRINUSE)
    );
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "held after a failed bind"
    );
    // The socket file stays when its listener is gone; nobody answers.
    drop(listener);
    assert_eq!(
        probe_in(here, &["unix-connect", "s"]),
        errno("unix-connect", libc::ECONNREFUSED)
    );
    // And it holds for its seconds.
    let began = Instant::now();
    assert_eq!(
        probe_in(here, &["unix-listen", "t", "0.5"]),
        "PROBE unix-listen ok"
    );
    assert!(
        began.elapsed() >= Duration::from_millis(450),
        "unix-listen let go after {:?}",
        began.elapsed()
    );
}

/// Catches: `ps-sees` that counts what it must not: the asker (its own arguments name
/// the marker), or a process of the asker's user that holds the marker (listed by
/// `ps`, shown here); `ps` run with an environment, or not run (its error is
/// reported as `errno`, so that is `ESRCH` only when the listing was read). Also a
/// malformed probe step: exit 2, nothing printed, nothing run.
#[test]
fn ps_sees_counts_only_another_users_process_and_usage_errors_exit_2() {
    let marker = format!("kbf-probe-{}-needle", std::process::id());
    let mut command = act(&["sleep", "60", ";", "echo", &marker]);
    command.stdout(Stdio::null());
    let _same_user = reaped::spawn(&mut command, 3).expect("starts");
    eventually("ps lists the marker", || {
        Command::new("/bin/ps")
            .args(["-axww", "-o", "pid,uid,args"])
            .output()
            .is_ok_and(|out| text(&out.stdout).contains(&marker))
    });
    assert_eq!(
        probe(&["ps-sees", &marker]),
        errno("ps-sees", libc::ESRCH),
        "a process of the asker's own user, or the asker, was counted"
    );
    for bad in [&["try-read"][..], &["ps-sees"], &["procargs", "0"]] {
        let out = output(bad);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {out:?}");
        assert_eq!(text(&out.stdout), "", "{bad:?}");
    }
}

/// Catches (off macOS): `procargs` and `proc-sees` that claim an answer a platform
/// without `KERN_PROCARGS2` and `proc_listpids` cannot give, or that exit non-zero.
#[cfg(not(target_os = "macos"))]
#[test]
fn process_probes_say_enosys_off_macos() {
    assert_eq!(probe(&["procargs", "1"]), errno("procargs", libc::ENOSYS));
    assert_eq!(probe(&["proc-sees", "1"]), errno("proc-sees", libc::ENOSYS));
}

/// Catches (macOS): `procargs` that cannot read the arguments of a process of the
/// asker's own user, or says `ok` for a process that is not there; `proc-sees` that
/// misses a live process, or lists one that is not there (`ESRCH`).
#[cfg(target_os = "macos")]
#[test]
fn process_probes_see_a_process_of_the_same_user_and_not_a_missing_one() {
    let mut command = act(&["sleep", "60"]);
    command.stdout(Stdio::null());
    let sleeper = reaped::spawn(&mut command, 3).expect("starts");
    let pid = sleeper.id().to_string();
    assert_eq!(probe(&["procargs", &pid]), "PROBE procargs ok");
    assert_eq!(probe(&["proc-sees", &pid]), "PROBE proc-sees ok");
    let gone = i32::MAX.to_string();
    assert!(
        probe(&["procargs", &gone]).starts_with("PROBE procargs errno="),
        "procargs of a missing process"
    );
    assert_eq!(
        probe(&["proc-sees", &gone]),
        errno("proc-sees", libc::ESRCH)
    );
}

/// Catches: `burn` returning at once (the leases' CPU-usage tests would measure nothing),
/// running far past its time, or exiting non-zero.
#[test]
fn burn_runs_for_its_time_and_succeeds() {
    let started = Instant::now();
    let out = output(&["burn", "0.3"]);
    let took = started.elapsed();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(20), "{took:?}");
}
