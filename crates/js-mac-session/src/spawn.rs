//! Starting an action's process as the lease user (`run`).
//!
//! The daemon passes the argument vector and environment, the three standard streams
//! and the lease directory as descriptors. The child gets exactly the lease user's ids:
//! its uid, the lease group's gid and the supplementary groups the directory lists for it
//! (`crate::groups`), never the helper's, and never `wheel` (group 0) unless the user is
//! in it. The standard library's `uid` and `gid` are not used: it would call
//! `setgroups(0, NULL)` when root sets a uid, and on macOS that leaves the list `[0]`.
//! The parent resolves the list; the child calls `setgroups` with it, then `setgid`, then
//! `setuid` (an id change after the groups, which need root). The child also gets its own
//! process group, a clean environment with `HOME`, `USER` and `LOGNAME` set by the
//! helper, and the lease directory as its working directory (`fchdir` after the uid
//! change, so the lease user must be able to search it). A process that is not root
//! cannot change its groups, and keeps its own.
//!
//! A `run` may carry limits (`crate::limits::Limits`): the child sets the largest file
//! (`RLIMIT_FSIZE`) and the most processes (`RLIMIT_NPROC`), soft and hard, with
//! `setrlimit`, after the fork and before the `exec`, so they bind the program however it
//! is started and no shell is needed to set them.
//!
//! Every other descriptor is closed across `exec`: the helper serves several leases at
//! once, and macOS cannot receive a descriptor already close-on-exec, so a lease
//! directory just received for one lease could otherwise leak into another lease's
//! process started at that moment.
//!
//! The process is started through [`crate::disclaim::Program`]: on macOS the action's
//! program answers to the privacy controls (TCC) for itself, never for the helper, so a
//! grant the helper held would not reach an action; the helper holds none.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Stdio};

use crate::disclaim::Program;
use crate::limits::Limits;

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
    /// The limits the child sets on itself before it executes; none for no limits
    /// ([`check_limits`]).
    pub limits: Option<Limits>,
}

/// Checks the limits of a start before anything starts.
///
/// # Errors
/// A value is out of range ([`Limits::check`]).
pub fn check_limits(limits: Option<&Limits>) -> Result<(), String> {
    limits.map_or(Ok(()), Limits::check)
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
    check_limits(spawn.limits.as_ref())
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let (program, args) = spawn
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argument vector"))?;
    // Resolved here, in the parent: the lookup is not async-signal-safe.
    let groups = crate::groups::of_user(&spawn.user, spawn.gid)
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
    let ids = Ids::new(spawn.uid, spawn.gid, groups);
    // `cwd` stays open in the parent until `spawn` returns, so the child's copy is the
    // lease directory.
    let steps = child_steps(ids, spawn.cwd.as_raw_fd(), spawn.limits);
    let mut command = start(OsStr::new(program), args, steps);
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
    let child = command.spawn();
    drop(spawn.cwd);
    child
}

/// The [`Program`] that runs `program` with `args` after the child's `steps`, its
/// responsibility disclaimed (`crate::disclaim`).
fn start(
    program: &OsStr,
    args: &[impl AsRef<OsStr>],
    steps: impl FnMut() -> io::Result<()> + Send + Sync + 'static,
) -> Program {
    // SAFETY: the steps run in the child between fork and exec; they are
    // [`child_steps`]', which call only async-signal-safe functions and allocate nothing.
    unsafe { Program::with_steps(program, args, steps) }
}

/// The limits the child can set, as the kernel names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resource {
    /// `RLIMIT_FSIZE`, in bytes.
    FileSize,
    /// `RLIMIT_NPROC`, a count.
    Processes,
}

/// The ids a process runs as: the user's uid and primary gid, and exactly its groups.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ids {
    /// The uid.
    uid: u32,
    /// The primary gid.
    gid: u32,
    /// The supplementary groups, never empty: `setgroups` with none would leave group 0
    /// on macOS (see the module documentation), so a user in none holds `[gid]`.
    groups: Vec<u32>,
}

impl Ids {
    /// `groups` as the list to set, `[gid]` when there are none.
    fn new(uid: u32, gid: u32, mut groups: Vec<u32>) -> Self {
        if groups.is_empty() {
            groups.push(gid);
        }
        Self { uid, gid, groups }
    }
}

/// What the child asks of the operating system between fork and exec: the seam through
/// which [`run_steps`]'s order is tested. Every method is async-signal-safe in [`Os`].
trait ChildOs {
    /// Whether the process is root, the only one that may set its groups.
    fn is_root(&mut self) -> bool;
    /// Sets the supplementary groups to exactly `groups`, which is not empty.
    fn set_groups(&mut self, groups: &[u32]) -> io::Result<()>;
    /// Sets the real, effective and saved group id.
    fn set_gid(&mut self, gid: u32) -> io::Result<()>;
    /// Sets the real, effective and saved user id.
    fn set_uid(&mut self, uid: u32) -> io::Result<()>;
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
    fn is_root(&mut self) -> bool {
        // SAFETY: geteuid reads the process's own id.
        unsafe { libc::geteuid() == 0 }
    }

    fn set_groups(&mut self, groups: &[u32]) -> io::Result<()> {
        // An empty list is what XNU turns into `[0]`: never ask for it.
        if groups.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: `groups` is a readable list of `groups.len()` ids for the call to copy;
        // the length is at most `MAX_GROUPS` ([`Ids`] come from `crate::groups`), which
        // the count's type (`int` on macOS, `size_t` elsewhere) holds.
        done(unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) })
    }

    fn set_gid(&mut self, gid: u32) -> io::Result<()> {
        // SAFETY: setgid takes a plain id.
        done(unsafe { libc::setgid(gid) })
    }

    fn set_uid(&mut self, uid: u32) -> io::Result<()> {
        // SAFETY: setuid takes a plain id.
        done(unsafe { libc::setuid(uid) })
    }

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
        let limit = libc::rlimit {
            rlim_cur: value,
            rlim_max: value,
        };
        // SAFETY: `limit` is a readable rlimit for the call to copy.
        let done = unsafe { libc::setrlimit(which, &raw const limit) } == 0;
        done.then_some(()).ok_or_else(io::Error::last_os_error)
    }
}

/// The steps the child runs before the program executes ([`Program::with_steps`]):
/// [`run_steps`] on the operating system, with the ids, the lease directory and the
/// limits.
fn child_steps(
    ids: Ids,
    cwd: libc::c_int,
    limits: Option<Limits>,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || run_steps(&mut Os, &ids, cwd, limits)
}

/// The child's steps before `exec`, in this order: set the groups (when root;
/// another process cannot change them), then the gid, then the uid; enter the lease
/// directory (after the uid change, so the lease user must be able to search it); mark
/// every descriptor above the standard streams close-on-exec; then set the largest file
/// and the most processes. The groups come first, as they need root, which the gid and
/// uid changes take away. A step that fails ends the rest, so the program is never
/// executed with an id or a limit missing. The process limit is last: from then on the
/// lease user may be unable to start another process. Async-signal-safe (with [`Os`]): it
/// allocates nothing.
fn run_steps(
    os: &mut impl ChildOs,
    ids: &Ids,
    cwd: libc::c_int,
    limits: Option<Limits>,
) -> io::Result<()> {
    set_ids(os, ids)?;
    os.fchdir(cwd)?;
    os.close_on_exec_from(3);
    if let Some(limits) = limits {
        os.set_limit(Resource::FileSize, limits.file_bytes)?;
        os.set_limit(Resource::Processes, libc::rlim_t::from(limits.processes))?;
    }
    Ok(())
}

/// A system call's answer: 0 is done, anything else the error it left in `errno`.
/// Async-signal-safe.
fn done(answer: libc::c_int) -> io::Result<()> {
    match answer {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Sets `ids` in this order: the groups (when root; another process cannot change them),
/// then the gid, then the uid. Async-signal-safe (with [`Os`]).
fn set_ids(os: &mut impl ChildOs, ids: &Ids) -> io::Result<()> {
    if os.is_root() {
        os.set_groups(&ids.groups)?;
    }
    os.set_gid(ids.gid)?;
    os.set_uid(ids.uid)
}

/// The step that makes a child of root `uid` and `gid` with `[gid]` its only group, for a
/// lookup run as a user (`crate::helper::folders`): [`set_ids`], never `setgroups(0)`.
pub(crate) fn primary_group_only(
    uid: u32,
    gid: u32,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    let ids = Ids::new(uid, gid, vec![gid]);
    move || set_ids(&mut Os, &ids)
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

    /// `/dev/null`, open for reading and for writing, so that it can stand for any of the
    /// three standard streams: a write to a descriptor opened for reading alone fails with
    /// `EBADF`, and a program that checks its writes (GNU `dd` checks the statistics it
    /// prints on its error stream) then ends with a failure.
    fn null() -> OwnedFd {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap()
            .into()
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
            limits: None,
        };
        (spawn, out)
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
        Groups(Vec<u32>),
        Gid(u32),
        Uid(u32),
        Chdir(libc::c_int),
        CloseOnExec(libc::c_int),
        Limit(Resource, libc::rlim_t),
    }

    /// A stand-in for the operating system that records what the child asks and fails
    /// the `fail_at`th request (counting from 0) with `EPERM`.
    struct Recorder {
        asked: Vec<Asked>,
        fail_at: Option<usize>,
        root: bool,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                asked: Vec::new(),
                fail_at: None,
                root: true,
            }
        }
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
        fn is_root(&mut self) -> bool {
            self.root
        }

        fn set_groups(&mut self, groups: &[u32]) -> io::Result<()> {
            self.ask(Asked::Groups(groups.to_vec()))
        }

        fn set_gid(&mut self, gid: u32) -> io::Result<()> {
            self.ask(Asked::Gid(gid))
        }

        fn set_uid(&mut self, uid: u32) -> io::Result<()> {
            self.ask(Asked::Uid(uid))
        }

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
        let ids = Ids::new(604, 20, vec![20]);
        let mut os = Recorder::default();
        run_steps(&mut os, &ids, 9, Some(Limits::new(4096, 8).unwrap())).unwrap();
        assert_eq!(
            os.asked,
            [
                Asked::Groups(vec![20]),
                Asked::Gid(20),
                Asked::Uid(604),
                Asked::Chdir(9),
                Asked::CloseOnExec(3),
                Asked::Limit(Resource::FileSize, 4096),
                Asked::Limit(Resource::Processes, 8),
            ]
        );
        let mut unlimited = Recorder::default();
        run_steps(&mut unlimited, &ids, 9, None).unwrap();
        assert_eq!(
            unlimited.asked[3..],
            [Asked::Chdir(9), Asked::CloseOnExec(3)],
            "no limits asked for, none set"
        );
    }

    /// Catches: a step that fails not ending the child's steps (the program would execute
    /// with a limit missing, or in the wrong directory), or the failure's cause lost.
    #[test]
    fn a_step_that_fails_ends_the_steps_with_its_error() {
        let limits = Some(Limits::new(4096, 8).unwrap());
        let ids = Ids::new(604, 20, vec![20, 12, 61]);
        // The requests are numbered from 0: the groups, the gid, the uid, the directory,
        // the descriptors, the file size, the process count.
        for (fail_at, ran) in [(0, 1), (1, 2), (2, 3), (3, 4), (5, 6), (6, 7)] {
            let mut os = Recorder {
                fail_at: Some(fail_at),
                ..Recorder::default()
            };
            let error = run_steps(&mut os, &ids, 9, limits).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EPERM), "{fail_at}");
            assert_eq!(os.asked.len(), ran, "{fail_at}: {:?}", os.asked);
        }
    }

    /// Catches: limits refused in no case, or a bad value that reaches `setrlimit`.
    #[test]
    fn limits_are_checked_before_anything_starts() {
        let good = Limits::new(4096, 8).unwrap();
        assert_eq!(check_limits(Some(&good)), Ok(()));
        assert_eq!(check_limits(None), Ok(()));
        let zero = Limits {
            file_bytes: 0,
            processes: 8,
        };
        assert!(check_limits(Some(&zero)).is_err());
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
        let soft_process_limit = || {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is a writable rlimit for the call to fill.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &raw mut limit) },
                0
            );
            // An unlimited soft limit is above what `Limits` takes; its largest is as good.
            u32::try_from(limit.rlim_cur)
                .unwrap_or(u32::MAX)
                .min(crate::limits::MAX_PROCESSES)
        };
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
            // The process count the test already runs under, which the unprivileged user
            // may always set (a count above its hard limit is refused with `EPERM`) and
            // which leaves `dd` able to start, so only the file limit is under test.
            spawn.limits = Some(Limits::new(4096, soft_process_limit()).unwrap());
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

    /// A program that writes to its error stream, and fails when the write fails (as GNU
    /// `dd` fails when it cannot print its statistics there), runs to a successful end with
    /// the streams the other tests give theirs. Catches a standard stream given to the
    /// program open for reading alone, which the `dd` test catches only on a host whose `dd`
    /// checks the write (GNU `dd` does; uutils `dd` does not).
    #[test]
    fn a_program_that_writes_to_its_error_stream_succeeds() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-mac-session-stderr");
        let (spawn, _out) = base(&["/bin/sh", "-c", "echo report >&2"], &dir);
        let status = super::spawn(spawn).unwrap().wait().unwrap();
        assert!(
            status.success(),
            "writing to the error stream failed: {status}"
        );
    }

    /// The operating system's `setrlimit`, run in the test process (the child's own run
    /// leaves no coverage record), with the value the process already has as its soft limit
    /// (a value the kernel allows this user: macOS clamps a process count above
    /// `kern.maxprocperuid`, which can be below the hard limit it reports), so that nothing
    /// in the test process changes but the hard limit falling to it: the
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
            let value = read(which).rlim_cur;
            Os.set_limit(resource, value).unwrap();
            let after = read(which);
            assert_eq!(
                (after.rlim_cur, after.rlim_max),
                (value, value),
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
        let (uid, gid) = me();
        let ids = Ids::new(uid, gid, vec![gid]);
        child_steps(ids.clone(), here.as_raw_fd(), None)().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(
            run_steps(&mut Os, &ids, file.as_raw_fd(), None)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }

    /// The child sets exactly the groups it is given, then the gid, then the uid, and only
    /// then enters the lease directory. Catches an id change before the groups (the groups
    /// need root, which the uid change takes away), a gid or uid set after the directory is
    /// entered, and a list cut or reordered on its way to the kernel.
    #[test]
    fn the_child_sets_the_groups_then_the_gid_then_the_uid_before_the_directory() {
        let ids = Ids::new(604, 20, vec![20, 12, 61, 701, 100]);
        let mut os = Recorder::default();
        run_steps(&mut os, &ids, 9, None).unwrap();
        assert_eq!(
            os.asked,
            [
                Asked::Groups(vec![20, 12, 61, 701, 100]),
                Asked::Gid(20),
                Asked::Uid(604),
                Asked::Chdir(9),
                Asked::CloseOnExec(3),
            ]
        );
    }

    /// The lookup run as a user (`getconf DARWIN_USER_DIR`) holds its primary group alone.
    /// Catches ids left to std, whose `setgroups(0)` macOS turns into group 0 (`wheel`).
    #[test]
    fn a_lookup_as_the_user_holds_its_primary_group_alone() {
        let mut os = Recorder::default();
        set_ids(&mut os, &Ids::new(604, 20, vec![20])).unwrap();
        assert_eq!(
            os.asked,
            [Asked::Groups(vec![20]), Asked::Gid(20), Asked::Uid(604)]
        );
    }

    /// A user with no groups at all holds its primary group alone: the child never asks for
    /// an empty list, which macOS turns into group 0 (`wheel`). Catches an empty list
    /// passed on to `setgroups`.
    #[test]
    fn a_user_in_no_group_holds_its_primary_group_alone() {
        let ids = Ids::new(604, 20, Vec::new());
        assert_eq!(ids.groups, [20]);
        let mut os = Recorder::default();
        run_steps(&mut os, &ids, 9, None).unwrap();
        assert_eq!(os.asked[0], Asked::Groups(vec![20]));
        // The operating system's own step refuses an empty list outright.
        assert_eq!(
            Os.set_groups(&[]).unwrap_err().raw_os_error(),
            Some(libc::EINVAL)
        );
    }

    /// A child that is not root leaves its groups alone (it cannot change them) and still
    /// sets its ids. Catches a `setgroups` that an unprivileged child asks for and fails.
    #[test]
    fn a_child_that_is_not_root_does_not_ask_to_change_its_groups() {
        let ids = Ids::new(604, 20, vec![20, 12]);
        let mut os = Recorder {
            root: false,
            ..Recorder::default()
        };
        run_steps(&mut os, &ids, 9, None).unwrap();
        assert_eq!(
            os.asked[..2],
            [Asked::Gid(20), Asked::Uid(604)],
            "{:?}",
            os.asked
        );
    }

    /// A user whose group list cannot be resolved is refused before anything starts, and
    /// nothing is started with another list in its place (a list too long for the kernel is
    /// refused by `crate::groups` the same way, and by name). Catches a start that goes on
    /// with no list, or an emptied one, when the lookup fails.
    #[test]
    fn a_user_whose_groups_cannot_be_resolved_is_refused_before_anything_starts() {
        let (mut spawn, _out) = base(&["/usr/bin/true"], &std::env::temp_dir());
        spawn.user = "a\0b".to_owned();
        let error = super::spawn(spawn).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("NUL"), "{error}");
    }

    /// The operating system's own id steps, as a user: setting one's own gid and uid
    /// succeeds, and changing the groups needs root (`EPERM`), which is why the child skips
    /// it when it is not root. Catches a step that sets another id than it is given.
    #[test]
    fn the_operating_systems_ids_are_the_callers_own_when_not_root() {
        assert!(
            !rustix::process::geteuid().is_root(),
            "run this test as a user"
        );
        let (uid, gid) = me();
        assert!(!Os.is_root());
        // As the lookup run as a user drops: to its own gid and uid, the groups left alone.
        primary_group_only(uid, gid)().unwrap();
        // Another group needs root.
        assert_eq!(
            Os.set_groups(&[gid]).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
    }

    /// A process started through the real path by a caller that is not root holds the
    /// caller's groups and no other, as `id -G` lists them for the caller itself: the list
    /// resolved for the user it names is applied only by root. Catches a child started with
    /// an emptied or invented list, or with the primary group alone.
    #[test]
    fn the_process_holds_the_callers_own_groups() {
        let dir = std::env::temp_dir();
        let (spawn, mut out) = base(&["/usr/bin/id", "-G"], &dir);
        assert!(super::spawn(spawn).unwrap().wait().unwrap().success());
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        assert_eq!(groups_in(&text), groups_in(&callers_groups()));
    }

    /// The groups `id -G` printed, sorted, each once.
    fn groups_in(text: &str) -> Vec<u32> {
        let mut groups: Vec<u32> = text
            .split_whitespace()
            .map(|g| g.parse().unwrap())
            .collect();
        groups.sort_unstable();
        groups.dedup();
        groups
    }

    /// What `id -G` prints for the caller, run directly.
    fn callers_groups() -> String {
        let out = std::process::Command::new("/usr/bin/id")
            .arg("-G")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }
}
