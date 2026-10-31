//! Starting an action's process as the lease user (`run`).
//!
//! The daemon passes the argument vector and environment, the three standard streams
//! and the lease directory as descriptors. The child gets the lease user's uid and the
//! lease group's gid, no supplementary groups (the standard library drops them when
//! root sets a uid), its own process group, a clean environment with `HOME`, `USER`
//! and `LOGNAME` set by the helper, and the lease directory as its working directory
//! (`fchdir` after the uid change, so the lease user must be able to search it).
//!
//! In the lease user's GUI session (`Session::Gui`, macOS only) the helper starts
//! `launchctl asuser <uid>` as root instead, which runs the helper's own `as-user` mode
//! in the user's `gui/<uid>` bootstrap namespace; that mode drops to the lease user's
//! uid and gid, with no supplementary groups, and executes the argument vector, in the
//! working directory and environment above (`crate::macos::gui`). A process started so
//! reaches the user's Aqua services (the window server, Launch Services), as a process
//! the user started at the login window does.
//!
//! A `run` may carry limits (`crate::limits::Limits`): the child sets the largest file
//! (`RLIMIT_FSIZE`) and the most processes (`RLIMIT_NPROC`), soft and hard, with
//! `setrlimit`, after the fork and before the `exec`, so they bind the program however it
//! is started and no shell is needed to set them. They are not set in the GUI session,
//! where the child that executes is the root helper itself, and are refused there.
//!
//! Every other descriptor is closed across `exec`: the helper serves several leases at
//! once, and macOS cannot receive a descriptor already close-on-exec, so a lease
//! directory just received for one lease could otherwise leak into another lease's
//! process started at that moment.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use crate::limits::Limits;
use crate::proto::Session;

/// The highest descriptor number the child's close-on-exec pass reaches when the
/// descriptor limit is unlimited or higher.
const FD_CEILING: libc::c_int = 1 << 16;

/// The variables the helper sets itself, whatever the daemon passed.
pub const RESERVED_ENV: [&str; 3] = ["HOME", "USER", "LOGNAME"];

/// What to start, and as whom.
#[derive(Debug)]
pub struct Spawn {
    /// The uid to run as.
    pub uid: u32,
    /// The primary group to run as.
    pub gid: u32,
    /// The account name, for `USER` and `LOGNAME`.
    pub user: String,
    /// The home folder, for `HOME`.
    pub home: PathBuf,
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// The environment the daemon asked for, as (name, value) pairs.
    pub env: Vec<(String, String)>,
    /// The process's standard input.
    pub stdin: OwnedFd,
    /// The process's standard output.
    pub stdout: OwnedFd,
    /// The process's standard error.
    pub stderr: OwnedFd,
    /// The lease directory, made the working directory.
    pub cwd: OwnedFd,
    /// Where it runs.
    pub session: Session,
    /// The limits the child sets on itself before it executes; none for no limits.
    /// Refused in the GUI session ([`check_limits`]).
    pub limits: Option<Limits>,
}

/// Checks the limits of a start before anything starts.
///
/// # Errors
/// A value is out of range ([`Limits::check`]), or the session is the GUI session, where
/// the helper cannot set them on the process that runs as the lease user.
pub fn check_limits(session: Session, limits: Option<&Limits>) -> Result<(), String> {
    let Some(limits) = limits else {
        return Ok(());
    };
    limits.check()?;
    if session == Session::Gui {
        return Err("limits are not set in the GUI session".to_owned());
    }
    Ok(())
}

/// Checks an argument vector and environment before anything starts.
///
/// # Errors
/// The vector is empty, or a name is empty or holds `=` or NUL, or a value or argument
/// holds NUL.
pub fn check(argv: &[String], env: &[(String, String)]) -> Result<(), String> {
    if argv.is_empty() {
        return Err("the argument vector is empty".to_owned());
    }
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err("an argument holds NUL".to_owned());
    }
    for (name, value) in env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!("environment variable {name:?} is malformed"));
        }
    }
    Ok(())
}

/// Starts `spawn`'s process. The caller has run [`check`].
///
/// # Errors
/// The process could not be started (the program is missing, the uid change, `fchdir` or
/// a `setrlimit` failed, or the limits are refused by [`check_limits`]).
pub fn spawn(spawn: Spawn) -> io::Result<Child> {
    check_limits(spawn.session, spawn.limits.as_ref())
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let (program, args) = spawn
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argument vector"))?;
    let mut command = match spawn.session {
        Session::Background => {
            let mut command = Command::new(program);
            command.args(args).uid(spawn.uid).gid(spawn.gid);
            command
        }
        Session::Gui => gui(spawn.uid, spawn.gid, program, args)?,
    };
    command
        .env_clear()
        .envs(
            spawn
                .env
                .iter()
                .filter(|(name, _)| !RESERVED_ENV.contains(&name.as_str()))
                .map(|(name, value)| (name, value)),
        )
        .env("HOME", &spawn.home)
        .env("USER", &spawn.user)
        .env("LOGNAME", OsString::from(&spawn.user))
        .process_group(0)
        .stdin(Stdio::from(spawn.stdin))
        .stdout(Stdio::from(spawn.stdout))
        .stderr(Stdio::from(spawn.stderr));
    let cwd = spawn.cwd.as_raw_fd();
    // SAFETY: the closure runs in the child between fork and exec; `run_steps` calls
    // only async-signal-safe functions and allocates nothing. `cwd` stays open in the
    // parent until `spawn` returns, so the child's copy is the lease directory.
    unsafe {
        command.pre_exec(child_steps(cwd, spawn.limits));
    }
    let child = command.spawn();
    drop(spawn.cwd);
    child
}

/// The command that starts `program` in the GUI session of `uid` (see the module
/// documentation).
#[cfg(target_os = "macos")]
fn gui(uid: u32, gid: u32, program: &str, args: &[String]) -> io::Result<Command> {
    crate::macos::gui::command(uid, gid, program, args)
}

/// There is no GUI session to start a process in off macOS.
#[cfg(not(target_os = "macos"))]
fn gui(_: u32, _: u32, _: &str, _: &[String]) -> io::Result<Command> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a GUI session exists only on macOS",
    ))
}

/// The limits the child can set, as the kernel names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resource {
    /// `RLIMIT_FSIZE`, in bytes.
    FileSize,
    /// `RLIMIT_NPROC`, a count.
    Processes,
}

/// What the child asks of the operating system between fork and exec: the seam through
/// which [`run_steps`]'s order is tested. Every method is async-signal-safe in [`Os`].
trait ChildOs {
    /// Makes the directory `fd` the working directory.
    fn fchdir(&mut self, fd: libc::c_int) -> io::Result<()>;
    /// Marks every descriptor from `first` close-on-exec.
    fn close_on_exec_from(&mut self, first: libc::c_int);
    /// Sets `resource` to `value`, soft and hard.
    fn set_limit(&mut self, resource: Resource, value: libc::rlim_t) -> io::Result<()>;
}

/// The operating system itself.
struct Os;

impl ChildOs for Os {
    fn fchdir(&mut self, fd: libc::c_int) -> io::Result<()> {
        // SAFETY: fchdir takes any number; one that is not a directory fails.
        match unsafe { libc::fchdir(fd) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn close_on_exec_from(&mut self, first: libc::c_int) {
        close_on_exec_from(first);
    }

    fn set_limit(&mut self, resource: Resource, value: libc::rlim_t) -> io::Result<()> {
        let which = match resource {
            Resource::FileSize => libc::RLIMIT_FSIZE,
            Resource::Processes => libc::RLIMIT_NPROC,
        };
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: writable rlimit.
        unsafe { libc::getrlimit(which, &raw mut limit) };
        limit.rlim_cur = value;
        // SAFETY: `limit` is a readable rlimit for the call to copy.
        let done = unsafe { libc::setrlimit(which, &raw const limit) } == 0;
        done.then_some(()).ok_or_else(io::Error::last_os_error)
    }
}

/// The closure `pre_exec` runs in the child: [`run_steps`] on the operating system, with
/// the lease directory and the limits.
fn child_steps(
    cwd: libc::c_int,
    limits: Option<Limits>,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || run_steps(&mut Os, cwd, limits)
}

/// The child's last steps before `exec`, after the uid change, in this order: enter the
/// lease directory, mark every descriptor above the standard streams close-on-exec, then
/// set the largest file and the most processes. A step that fails ends the rest, so the
/// program is never executed with a limit missing. The process limit is last: from then
/// on the lease user may be unable to start another process. Async-signal-safe (with
/// [`Os`]): it allocates nothing.
fn run_steps(os: &mut impl ChildOs, cwd: libc::c_int, limits: Option<Limits>) -> io::Result<()> {
    os.fchdir(cwd)?;
    os.close_on_exec_from(3);
    if let Some(limits) = limits {
        os.set_limit(Resource::FileSize, limits.file_bytes)?;
        os.set_limit(Resource::Processes, libc::rlim_t::from(limits.processes))?;
    }
    Ok(())
}

/// Marks every descriptor from `first` up to the descriptor limit (at most
/// [`FD_CEILING`]) close-on-exec. Async-signal-safe.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // bounded by FD_CEILING
fn close_on_exec_from(first: libc::c_int) {
    // Left at the ceiling if getrlimit fails, so the pass never shrinks to nothing.
    let mut limit = libc::rlimit {
        rlim_cur: FD_CEILING as libc::rlim_t,
        rlim_max: FD_CEILING as libc::rlim_t,
    };
    // SAFETY: `limit` is a writable rlimit for the call to fill.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
    let ceiling = limit.rlim_cur.min(FD_CEILING as libc::rlim_t) as libc::c_int;
    for fd in first..ceiling {
        // SAFETY: F_SETFD on a number that is not an open descriptor fails with EBADF
        // and changes nothing.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;

    use super::*;

    fn pipe() -> (std::io::PipeReader, OwnedFd) {
        let (read, write) = std::io::pipe().unwrap();
        (read, write.into())
    }

    fn null() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    fn me() -> (u32, u32) {
        (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        )
    }

    fn base(argv: &[&str], cwd: &std::path::Path) -> (Spawn, std::io::PipeReader) {
        let (out, write) = pipe();
        let (uid, gid) = me();
        let spawn = Spawn {
            uid,
            gid,
            user: "kbf-lease-1-1".to_owned(),
            home: PathBuf::from("/var/empty/kbf-lease-1-1"),
            argv: argv.iter().map(|&a| a.to_owned()).collect(),
            env: vec![
                ("KEEP".to_owned(), "kept".to_owned()),
                ("HOME".to_owned(), "/root".to_owned()),
            ],
            stdin: null(),
            stdout: write,
            stderr: null(),
            cwd: std::fs::File::open(cwd).unwrap().into(),
            session: Session::Background,
            limits: None,
        };
        (spawn, out)
    }

    /// Catches a GUI run that falls back to a background one where there is no GUI
    /// session: off macOS it is refused, and nothing starts.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_gui_session_is_refused_off_macos() {
        let dir = std::env::temp_dir();
        let (spawn, _out) = base(&["/bin/true"], &dir);
        let spawn = Spawn {
            session: Session::Gui,
            ..spawn
        };
        let error = super::spawn(spawn).expect_err("started off macOS");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }

    /// Catches: the lease directory not made the working directory, the daemon's
    /// environment (or the helper's) passed through unchecked, the reserved names
    /// left to the daemon, or a process left in the helper's process group.
    #[test]
    fn the_process_runs_in_the_lease_directory_with_a_clean_environment() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-mac-session-spawn");
        // CARGO_MANIFEST_DIR is in the test's own environment, and must not leak.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let argv = crate::testing::act_argv(
            "pwd ; env HOME ; env USER ; env LOGNAME ; env KEEP ; env CARGO_MANIFEST_DIR clean ; \
             pgid",
        );
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (spawn, mut out) = base(&argv, &dir);
        let mut child = super::spawn(spawn).unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            std::fs::canonicalize(&dir).unwrap().to_str().unwrap()
        );
        assert_eq!(
            lines[1..6],
            [
                "/var/empty/kbf-lease-1-1",
                "kbf-lease-1-1",
                "kbf-lease-1-1",
                "kept",
                "clean"
            ]
        );
        assert_eq!(
            lines[6],
            pid.to_string(),
            "not the leader of its own process group"
        );
    }

    /// Catches: a descriptor of the helper (another lease's directory, say) inherited
    /// by the action, which the close-on-exec pass exists to prevent.
    #[test]
    fn no_other_descriptor_reaches_the_process() {
        let dir = std::env::temp_dir();
        // An inheritable descriptor, as a lease directory just received is on macOS.
        let raw = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let leaked = std::os::fd::IntoRawFd::into_raw_fd(raw);
        // `wait-for` with no time looks once: it fails (exit 1) where the descriptor is
        // closed, and the `echo` after it runs only where it is open.
        let argv = crate::testing::act_argv(&format!(
            "echo ran ; wait-for /dev/fd/{leaked} 0 ; echo leaked"
        ));
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (spawn, mut out) = base(&argv, &dir);
        let mut child = super::spawn(spawn).unwrap();
        let status = child.wait().unwrap();
        // SAFETY: `leaked` is the descriptor opened above, owned by nothing else.
        drop(unsafe { OwnedFd::from_raw_fd(leaked) });
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        assert_eq!(text, "ran\n", "the descriptor reached the process");
        assert_eq!(status.code(), Some(1), "{status}");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let (spawn, _out) = base(&["/no/such/program"], &std::env::temp_dir());
        assert_eq!(
            super::spawn(spawn).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    /// Catches: a working directory the lease user cannot enter silently replaced by
    /// another (the helper's own).
    #[test]
    fn a_lease_directory_that_cannot_be_entered_is_an_error() {
        let (mut spawn, _out) = base(&["/usr/bin/true"], &std::env::temp_dir());
        // A descriptor that is not a directory: fchdir fails in the child.
        spawn.cwd = null();
        assert_eq!(
            super::spawn(spawn).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
        let (mut spawn, _out) = base(&["/usr/bin/true"], &std::env::temp_dir());
        spawn.argv.clear();
        assert_eq!(
            super::spawn(spawn).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn malformed_vectors_are_refused() {
        let ok = |argv: &[&str], env: &[(&str, &str)]| {
            let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
            let env: Vec<(String, String)> = env
                .iter()
                .map(|&(n, v)| (n.to_owned(), v.to_owned()))
                .collect();
            check(&argv, &env)
        };
        assert_eq!(ok(&["/usr/bin/true"], &[("A", "b=c")]), Ok(()));
        assert!(ok(&[], &[]).unwrap_err().contains("empty"));
        assert!(ok(&["a\0b"], &[]).unwrap_err().contains("NUL"));
        for env in [("", "x"), ("A=B", "x"), ("A\0", "x"), ("A", "x\0")] {
            assert!(
                ok(&["/usr/bin/true"], &[env])
                    .unwrap_err()
                    .contains("malformed"),
                "{env:?}"
            );
        }
    }

    /// What the child asked of the operating system, in order.
    #[derive(Debug, PartialEq, Eq)]
    enum Asked {
        Chdir(libc::c_int),
        CloseOnExec(libc::c_int),
        Limit(Resource, libc::rlim_t),
    }

    /// A stand-in for the operating system that records what the child asks and fails
    /// the `fail_at`th request (counting from 0) with `EPERM`.
    #[derive(Default)]
    struct Recorder {
        asked: Vec<Asked>,
        fail_at: Option<usize>,
    }

    impl Recorder {
        fn ask(&mut self, what: Asked) -> io::Result<()> {
            let at = self.asked.len();
            self.asked.push(what);
            match self.fail_at == Some(at) {
                true => Err(io::Error::from_raw_os_error(libc::EPERM)),
                false => Ok(()),
            }
        }
    }

    impl ChildOs for Recorder {
        fn fchdir(&mut self, fd: libc::c_int) -> io::Result<()> {
            self.ask(Asked::Chdir(fd))
        }

        fn close_on_exec_from(&mut self, first: libc::c_int) {
            let _ = self.ask(Asked::CloseOnExec(first));
        }

        fn set_limit(&mut self, resource: Resource, value: libc::rlim_t) -> io::Result<()> {
            self.ask(Asked::Limit(resource, value))
        }
    }

    /// Catches: a limit set before the directory and the descriptors are dealt with (the
    /// lease directory could then not be entered under the process limit), the limits
    /// swapped, their units changed (bytes, and a count), or a limit set when the start
    /// has none.
    #[test]
    fn the_child_enters_the_directory_then_sets_the_limits() {
        let mut os = Recorder::default();
        run_steps(&mut os, 9, Some(Limits::new(4096, 8).unwrap())).unwrap();
        assert_eq!(
            os.asked,
            [
                Asked::Chdir(9),
                Asked::CloseOnExec(3),
                Asked::Limit(Resource::FileSize, 4096),
                Asked::Limit(Resource::Processes, 8),
            ]
        );
        let mut unlimited = Recorder::default();
        run_steps(&mut unlimited, 9, None).unwrap();
        assert_eq!(
            unlimited.asked,
            [Asked::Chdir(9), Asked::CloseOnExec(3)],
            "no limits asked for, none set"
        );
    }

    /// Catches: a step that fails not ending the child's steps (the program would execute
    /// with a limit missing, or in the wrong directory), or the failure's cause lost.
    #[test]
    fn a_step_that_fails_ends_the_steps_with_its_error() {
        let limits = Some(Limits::new(4096, 8).unwrap());
        // The requests are numbered from 0: the directory, the descriptors, the file size,
        // the process count.
        for (fail_at, ran) in [(0, 1), (2, 3), (3, 4)] {
            let mut os = Recorder {
                fail_at: Some(fail_at),
                ..Recorder::default()
            };
            let error = run_steps(&mut os, 9, limits).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{fail_at}");
            assert_eq!(os.asked.len(), ran, "{fail_at}: {:?}", os.asked);
        }
    }

    /// Catches: limits refused in no case, or accepted in the GUI session, where the
    /// child that executes is the root helper itself and the limits would bind the wrong
    /// process, or a bad value that reaches `setrlimit`.
    #[test]
    fn limits_are_checked_before_anything_starts() {
        let good = Limits::new(4096, 8).unwrap();
        assert_eq!(check_limits(Session::Background, Some(&good)), Ok(()));
        assert_eq!(check_limits(Session::Gui, None), Ok(()));
        assert!(
            check_limits(Session::Gui, Some(&good))
                .unwrap_err()
                .contains("GUI")
        );
        let zero = Limits {
            file_bytes: 0,
            processes: 8,
        };
        assert!(check_limits(Session::Background, Some(&zero)).is_err());
        let (mut spawn, _out) = base(&["/usr/bin/true"], &std::env::temp_dir());
        spawn.limits = Some(zero);
        assert_eq!(
            super::spawn(spawn).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "a bad limit reached the start"
        );
    }

    /// The limit is really set, in a real child: a write past the file limit is stopped
    /// (`SIGXFSZ`, or `EFBIG` where it is ignored), the file stops at the limit, and a
    /// program that writes less than the limit runs to the end. Catches: limits taken from
    /// the request and not applied to the child, set in blocks instead of bytes, or set
    /// after the program is executed.
    #[test]
    fn the_file_size_limit_stops_a_write_past_it() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-mac-session-fsize");
        let run = |blocks: u32| {
            let target = format!("of={}/file{blocks}", dir.display());
            let count = format!("count={blocks}");
            let argv = [
                "/bin/dd",
                "if=/dev/zero",
                "bs=1024",
                target.as_str(),
                count.as_str(),
            ];
            let (mut spawn, _out) = base(&argv, &dir);
            spawn.limits = Some(Limits::new(4096, 4096).unwrap());
            let status = super::spawn(spawn).unwrap().wait().unwrap();
            let len = std::fs::metadata(dir.join(format!("file{blocks}")))
                .unwrap()
                .len();
            (status, len)
        };
        let (status, len) = run(64);
        assert!(!status.success(), "the write past the limit succeeded");
        assert_eq!(len, 4096, "the file did not stop at the limit: {status}");
        let (status, len) = run(2);
        assert!(status.success(), "a write under the limit failed: {status}");
        assert_eq!(len, 2048);
    }

    /// The operating system's `setrlimit`, run in the test process (the child's own run
    /// leaves no coverage record), with the value the process already has as its hard limit,
    /// so that nothing in the test process changes but the soft limit rising to it: the
    /// limit is set soft and hard to the value given, for each of the two resources. It
    /// catches a limit set only soft (which the program could raise) or not set at all.
    #[test]
    fn the_operating_system_sets_both_limits_to_the_value_given() {
        let read = |which| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is a writable rlimit for the call to fill.
            assert_eq!(unsafe { libc::getrlimit(which, &raw mut limit) }, 0);
            limit
        };
        for (resource, which) in [
            (Resource::FileSize, libc::RLIMIT_FSIZE),
            (Resource::Processes, libc::RLIMIT_NPROC),
        ] {
            let hard = read(which).rlim_max;
            Os.set_limit(resource, hard).unwrap();
            let after = read(which);
            assert_eq!(
                (after.rlim_cur, after.rlim_max),
                (hard, hard),
                "{resource:?}"
            );
        }
    }

    /// The child's steps run in the test process (the child's own run leaves no coverage
    /// record: it ends in `exec`), on the real operating system, with no limits asked for.
    /// Entering the current directory leaves the process where it was; a descriptor that is
    /// not a directory is an error.
    #[test]
    fn the_child_steps_enter_the_directory_or_fail() {
        let here = std::fs::File::open(".").unwrap();
        child_steps(here.as_raw_fd(), None)().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            run_steps(&mut Os, file.as_raw_fd(), None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }
}
