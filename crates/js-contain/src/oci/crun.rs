//! `crun`, driven through the `runc` crate (containerd/rust-extensions, 0.3, the async
//! client): the OCI runtime command line `crun run --bundle`, `kill` and `delete`.
//!
//! What the crate does and does not give this driver, checked in its source (0.3.0):
//! - `GlobalOpts::command(path)` sets the binary every call runs, so `crun` is used as
//!   it is for `runc` (`crun` takes the same global options `--root`, `--log` and
//!   `--log-format`, and the same `run`/`kill`/`delete` verbs). `--rootless` is left
//!   unset: `crun` finds out itself, and has no such option.
//! - It has no `--cgroup-manager` option; `crun`'s default is `cgroupfs`, which the
//!   daemon's delegated subtree needs.
//! - `Runc::run` waits for the process and returns `Error::CommandFailed { status, .. }`
//!   for a non-zero exit, so the container's exit status is `crun run`'s own: no
//!   `state` query and no window in which the container is stopped but its exit code is
//!   not recorded.
//! - It runs its commands with `tokio::process`, without `kill_on_drop`; [`KillOnDrop`]
//!   is the spawner that adds it, so a `run` that is dropped or aborted does not leave
//!   `crun` behind.
//! - Its `Io` hook sets the child's stdin/stdout/stderr; [`PipeIo`] points the
//!   container's stdout and stderr at pipes the caller drains (`logs`), instead of the
//!   crate's, which are read to the end into memory.
//! - The synchronous client is the same crate without its `async` feature, so the two
//!   cannot be built together: the blocking paths (a dropped lease's clean) use
//!   `std::process` with the same arguments ([`Crun::delete_blocking`]).

use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use runc::error::Error;
use runc::options::{CreateOpts, GlobalOpts};
use runc::{Command, Io, Runc, Spawner};

/// How the backend changes the environment `crun` inherits: called on every command
/// line it starts, so a caller removes what no container runtime should be handed.
pub type EnvScrub = fn(&mut std::process::Command);

/// The only `crun` a root daemon runs. AppArmor attaches the distribution's profile (which
/// grants the job user a user namespace) to this path, and a copy of the binary elsewhere
/// is unconfined and is refused the namespace.
pub const DISTRO_CRUN: &str = "/usr/bin/crun";

/// Whether the backend may run `runtime`: any program unless the backend drops to a job
/// user (`drops`, a root daemon), and then only [`DISTRO_CRUN`] by that path, unless
/// `stand_in` says the runtime is a test double.
pub(crate) fn runtime_allowed(runtime: &Path, drops: bool, stand_in: bool) -> bool {
    !drops || stand_in || runtime == Path::new(DISTRO_CRUN)
}

/// The default [`EnvScrub`]: nothing is removed.
pub fn no_scrub(_: &mut std::process::Command) {}

/// What a root daemon does to every command line it starts (`OciConfig::job_user`): the
/// program runs as the dedicated job user, with no supplementary group, from `/`, with
/// an environment of `PATH` and `HOME` only (so no `XDG_RUNTIME_DIR` and nothing of the
/// daemon's credentials is inherited). The drop is part of the backend: it does not
/// depend on the caller's [`EnvScrub`]. The lease's keeper is started with the same drop
/// ([`crate::keeper::Keeper::watch_as`]).
#[derive(Clone, Debug)]
pub struct JobDrop {
    uid: u32,
    gid: u32,
    /// The `PATH` the program gets: where it finds `newuidmap` and `newgidmap`.
    path: std::ffi::OsString,
}

impl JobDrop {
    /// The drop for job user `(uid, gid)`, or `None` when this process is not root (a
    /// user that is not root cannot change its ids, and runs `crun` as itself).
    #[must_use]
    pub fn new(
        job: Option<(u32, u32)>,
        path: Option<&std::ffi::OsStr>,
        daemon_uid: u32,
    ) -> Option<Self> {
        let (uid, gid) = job?;
        (daemon_uid == 0).then(|| Self {
            uid,
            gid,
            path: path.map(ToOwned::to_owned).unwrap_or_default(),
        })
    }

    /// Makes `command` start as the job user: `setgroups([])`, then the gid, then the uid
    /// (the order in which each call is still allowed), all three real, effective and saved.
    pub(crate) fn apply(&self, command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt as _;
        command
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", "/")
            .current_dir("/");
        // SAFETY: `drop_ids` makes three system calls and nothing else (no allocation, no
        // lock) in the forked child before it execs.
        unsafe {
            command.pre_exec(self.in_child());
        }
    }
}

impl JobDrop {
    /// The function run in the forked child before the exec.
    fn in_child(&self) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
        let (uid, gid) = (self.uid, self.gid);
        move || drop_ids(uid, gid, clear_groups)
    }
}

/// `setgroups([])`: the process keeps no supplementary group.
fn clear_groups() -> std::io::Result<()> {
    rustix::thread::set_thread_groups(&[]).map_err(Into::into)
}

/// The drop, in the order each call is still allowed: the groups (`groups`, which needs
/// the privilege the later calls take away), then the gid, then the uid, each as real,
/// effective and saved id. The first call that fails ends it, and later ones are not made.
fn drop_ids(
    uid: u32,
    gid: u32,
    groups: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let (uid, gid) = (
        rustix::process::Uid::from_raw(uid),
        rustix::process::Gid::from_raw(gid),
    );
    groups()
        .and_then(|()| rustix::thread::set_thread_res_gid(gid, gid, gid).map_err(Into::into))
        .and_then(|()| rustix::thread::set_thread_res_uid(uid, uid, uid).map_err(Into::into))
}

/// How the backend prepares each command line it starts: the caller's [`EnvScrub`] first,
/// then, for a root daemon, the [`JobDrop`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Launch<'a> {
    scrub: EnvScrub,
    job: Option<&'a JobDrop>,
}

impl Launch<'_> {
    fn apply(self, command: &mut std::process::Command) {
        (self.scrub)(command);
        if let Some(job) = self.job {
            job.apply(command);
        }
    }
}

/// How one `crun run` ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// `crun run` exited with this status: the container's exit code (`128 + n` if it
    /// was killed by signal `n`), or `crun`'s own failure, told apart by the log.
    Exited(i32),
    /// `crun` could not be run, or its task did not finish.
    Failed(String),
}

/// The exit status a shell would report for `status`.
pub fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// How long a dropped child is waited for after its `SIGKILL`. A killed process exits
/// once it leaves the system call it is in; a child still there at the limit is left
/// to tokio's orphan queue (`kill_on_drop`).
const REAP_ON_DROP: Duration = Duration::from_secs(5);

/// A running `crun`, killed and reaped when dropped. tokio's `kill_on_drop` alone sends
/// the `SIGKILL` and leaves the reaping to its orphan queue, so an aborted `crun run`
/// could still be there, dying or a zombie, when the lease clean went on to `crun
/// delete` its container: the delete then raced the run's own removal of its state.
/// Dropping blocks (microseconds for a process that has the signal pending) on the
/// thread that polled the future.
struct Reaped(tokio::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
        wait_exited(&mut self.0, REAP_ON_DROP);
    }
}

/// Polls `child` until it has exited (and so is reaped) or `limit` has passed.
fn wait_exited(child: &mut tokio::process::Child, limit: Duration) {
    let deadline = Instant::now() + limit;
    // `Ok(None)`: not exited yet. An error is not a process to wait for.
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The `Spawner` of this driver: [`runc::DefaultExecutor`], with `kill_on_drop` and a
/// child that is reaped when its future is dropped ([`Reaped`]).
#[derive(Debug)]
struct KillOnDrop(Crun);

#[async_trait]
impl Spawner for KillOnDrop {
    async fn execute(&self, mut cmd: Command) -> runc::Result<(ExitStatus, u32, String, String)> {
        cmd.kill_on_drop(true);
        self.0.launch().apply(cmd.as_std_mut());
        let mut child = cmd.spawn().map_err(Error::ProcessSpawnFailed)?;
        let pid = child.id().unwrap_or(0);
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut child = Reaped(child);
        let waiting = child.0.wait();
        let (status, out, err) = tokio::join!(waiting, read_all(stdout), read_all(stderr));
        let status = status.map_err(Error::InvalidCommand)?;
        Ok((
            status,
            pid,
            String::from_utf8_lossy(&out.map_err(Error::InvalidCommand)?).into_owned(),
            String::from_utf8_lossy(&err.map_err(Error::InvalidCommand)?).into_owned(),
        ))
    }
}

/// Everything `pipe` says until it closes; nothing for a stream that was not piped.
async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

/// `Io` that gives `crun run` the write ends of the pipes its output is drained from
/// (`logs`), and as stdin the file the container's init reports a program it could not
/// start in (`oci::init`). The write ends are closed when this is dropped, with the call.
#[derive(Debug)]
struct PipeIo {
    stdin: std::fs::File,
    stdout: OwnedFd,
    stderr: OwnedFd,
}

#[async_trait]
impl Io for PipeIo {
    async fn set(&self, cmd: &mut Command) -> std::io::Result<()> {
        cmd.stdin(self.stdin.try_clone()?);
        cmd.stdout(self.stdout.try_clone()?);
        cmd.stderr(self.stderr.try_clone()?);
        Ok(())
    }

    async fn close_after_start(&self) {}
}

/// The `crun` program and how the driver calls it.
#[derive(Clone, Debug)]
pub(crate) struct Crun {
    program: PathBuf,
    scrub: EnvScrub,
    job: Option<JobDrop>,
}

/// The state directory of a lease's `crun` (`--root`), inside the lease directory: no
/// state is shared between jobs, and removing the lease removes it.
pub(crate) fn state_dir(lease: &Path) -> PathBuf {
    lease.join("crun")
}

/// `crun`'s log of a lease (`--log`, JSON).
pub(crate) fn log_path(lease: &Path) -> PathBuf {
    lease.join("crun.log")
}

impl Crun {
    pub(crate) fn new(program: PathBuf, scrub: EnvScrub, job: Option<JobDrop>) -> Self {
        Self {
            program,
            scrub,
            job,
        }
    }

    /// The drop to the job user every command of this driver gets, if any.
    pub(crate) fn job(&self) -> Option<&JobDrop> {
        self.job.as_ref()
    }

    /// How every command of this driver is prepared.
    fn launch(&self) -> Launch<'_> {
        Launch {
            scrub: self.scrub,
            job: self.job.as_ref(),
        }
    }

    /// The client for the lease in `lease`: its own state directory and log.
    fn client(&self, lease: &Path) -> Result<Runc, String> {
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state_dir(lease))
            .log(log_path(lease))
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop(self.clone())));
        opts.build().map_err(|e| self.refused(&e))
    }

    /// Runs the container `id` of the bundle in `bundle` to its end (`crun run`: create,
    /// start and delete in one process), its stdout and stderr to the given pipes. The
    /// result is `crun`'s exit status. Dropping the future kills `crun`.
    pub(crate) async fn run(
        &self,
        lease: &Path,
        id: &str,
        bundle: &Path,
        stdin: std::fs::File,
        stdout: OwnedFd,
        stderr: OwnedFd,
    ) -> RunEnd {
        let client = match self.client(lease) {
            Ok(client) => client,
            Err(e) => return RunEnd::Failed(e),
        };
        let opts = CreateOpts::new().io(Arc::new(PipeIo {
            stdin,
            stdout,
            stderr,
        }));
        match client.run(id, bundle, Some(&opts)).await {
            Ok(()) => RunEnd::Exited(0),
            Err(Error::CommandFailed { status, .. }) => RunEnd::Exited(code_of(status)),
            Err(e) => RunEnd::Failed(format!("crun run: {e}")),
        }
    }

    /// Runs `bundle` as a helper (`crun run`, output captured, nothing in a log file):
    /// `Ok` once it exits 0, otherwise what it printed.
    pub(crate) async fn run_helper(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let log = state.with_extension("log");
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state)
            .debug(true)
            .log(&log)
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop(self.clone())));
        let client = opts.build().map_err(|e| self.refused(&e))?;
        match client.run(id, bundle, None).await {
            Ok(()) => Ok(()),
            Err(Error::CommandFailed {
                status,
                stdout,
                stderr,
            }) => {
                // What `crun` itself logged: the reason it failed, as opposed to the
                // helper's own words on stderr.
                let logged = std::fs::read_to_string(&log).unwrap_or_default();
                let tail = logged.len().saturating_sub(3000);
                Err(format!(
                    "crun run (helper {id}): {status}: {} {} {} [log: {}]",
                    stdout.trim(),
                    stderr.trim(),
                    super::words::crun_words(&logged),
                    logged.get(tail..).unwrap_or_default().replace('\n', " ")
                ))
            }
            Err(e) => Err(format!("crun run (helper {id}): {e}")),
        }
    }

    /// Sends signal `signal` (a number) to the container's main process.
    pub(crate) async fn kill(&self, lease: &Path, id: &str, signal: u32) -> Result<(), String> {
        let client = self.client(lease)?;
        client
            .kill(id, signal, None)
            .await
            .map_err(|e| format!("crun kill: {e}"))
    }

    /// Deletes the container `id`, killing it first if it runs (`delete --force`); one
    /// `crun` does not know is not an error with `--force`. `crun run` deletes its
    /// container as it exits, so this finds nothing on the normal path; it is what removes
    /// the container, its cgroup and its state after a `crun` that was killed. Blocking:
    /// a dropped lease cleans up from `Drop`.
    pub(crate) fn delete_blocking(&self, lease: &Path, id: &str) -> Result<(), String> {
        self.delete_waiting(lease, id, STATE_REMOVAL_WAIT)
    }

    /// [`Crun::delete_blocking`], waiting at most `wait` for a state that is being removed.
    fn delete_waiting(&self, lease: &Path, id: &str, wait: Duration) -> Result<(), String> {
        let state = state_dir(lease);
        let deleted = self.blocking(&state, &["delete", "--force", id], "delete");
        // A `crun run` killed with its container (a sweep's case) deletes its own state as
        // it exits, and a `crun delete` that lost that race fails on what it listed or
        // loaded (the directory, or the `config.json` the other removed first, with the
        // directory itself still there for a moment): once the state is gone there is
        // nothing left to delete. The state of a delete that failed for another reason
        // stays, and that failure is returned after the wait.
        if deleted.is_err() && wait_gone(&state.join(id), wait) {
            return Ok(());
        }
        deleted
    }

    /// Runs the helper bundle blocking: for the clean of a lease that has files only a
    /// user namespace can delete.
    pub(crate) fn run_helper_blocking(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let bundle = bundle.to_string_lossy();
        self.blocking(state, &["run", "--bundle", &bundle, id], "run (helper)")
    }

    /// Runs `crun --root <state> <args>` and waits: `Ok` if it exits 0, else its status
    /// and what it said on stderr.
    fn blocking(&self, state: &Path, args: &[&str], what: &str) -> Result<(), String> {
        self.stdout_of(state, args, what).map(drop)
    }

    /// [`Crun::blocking`], returning what `crun` printed on stdout.
    fn stdout_of(&self, state: &Path, args: &[&str], what: &str) -> Result<String, String> {
        let mut command = std::process::Command::new(&self.program);
        self.launch().apply(&mut command);
        let output = command
            .arg("--root")
            .arg(state)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => Err(format!(
                "crun {what}: {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("run {}: {e}", self.program.display())),
        }
    }

    /// The pid, on the host, of the first process of the running container `id` of the
    /// lease in `lease` (`crun state`). Blocking.
    pub(crate) fn pid(&self, lease: &Path, id: &str) -> Result<u64, String> {
        let state = self.stdout_of(&state_dir(lease), &["state", id], "state")?;
        serde_json::from_str::<serde_json::Value>(&state)
            .ok()
            .and_then(|state| state["pid"].as_u64())
            .ok_or_else(|| format!("crun state: no pid in {:?}", state.trim()))
    }

    /// The words for a client `crun` could not be found for.
    fn refused(&self, error: &Error) -> String {
        format!("crun {}: {error}", self.program.display())
    }
}

/// How long a failed `crun delete` waits for the container's state to be removed by
/// the `crun run` that owns it. That run is removing it already; the wait ends the moment
/// the directory is gone.
const STATE_REMOVAL_WAIT: Duration = Duration::from_secs(3);

/// Whether `path` is gone, polling until `limit` has passed.
fn wait_gone(path: &Path, limit: Duration) -> bool {
    wait_gone_by(path, limit, &Instant::now, &nap)
}

/// The pause between two looks at the path.
fn nap() {
    std::thread::sleep(Duration::from_millis(10));
}

/// [`wait_gone`] with the clock and the pause given, so a test drives each side of the loop.
fn wait_gone_by(path: &Path, limit: Duration, now: &dyn Fn() -> Instant, pause: &dyn Fn()) -> bool {
    let deadline = now() + limit;
    loop {
        if !path.exists() {
            return true;
        }
        if now() >= deadline {
            return false;
        }
        pause();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use super::*;

    /// Catches `crun`'s status being read as anything but a shell reads it: an exit code
    /// as it is, a death by signal `n` as `128 + n` (what the OOM classification looks for
    /// at 137), so a killed `crun` is not an exit 0.
    #[test]
    fn a_status_is_the_exit_code_or_128_plus_the_signal() {
        assert_eq!(code_of(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(code_of(ExitStatus::from_raw(0)), 0);
        assert_eq!(code_of(ExitStatus::from_raw(9)), 137);
        assert_eq!(code_of(ExitStatus::from_raw(15)), 143);
    }

    /// Catches a `crun` that is aborted (its future dropped, which `Running::force` does
    /// when the grace after `cgroup.kill` runs out) and is still there, alive or a
    /// zombie, when the drop returns: the clean then ran `crun delete` beside a
    /// `crun run` that had not been reaped. Each round polls `execute` once (which
    /// spawns `sleep`, no timer: a zero timeout polls its future first), finds the new
    /// child in the thread's `children` list, drops the future, and looks for the pid at
    /// once. Rounds repeat so a child that dies fast enough on its own cannot hide a
    /// driver that does not wait.
    #[test]
    fn a_dropped_crun_is_reaped_before_the_drop_returns() {
        let children = format!(
            "/proc/self/task/{}/children",
            rustix::thread::gettid().as_raw_nonzero()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut seen = std::collections::HashSet::new();
        for round in 0..25 {
            let mut command = Command::new("sleep");
            command.arg("600");
            let spawner = KillOnDrop(Crun::new(PathBuf::from("sleep"), super::no_scrub, None));
            let mut run = Box::pin(spawner.execute(command));
            let polled =
                runtime.block_on(async { tokio::time::timeout(Duration::ZERO, &mut run).await });
            assert!(polled.is_err(), "round {round}: sleep 600 ended");
            let listed = std::fs::read_to_string(&children).expect("the thread's children");
            let new: Vec<_> = listed
                .split_whitespace()
                .filter(|p| seen.insert((*p).to_owned()))
                .collect();
            assert_eq!(
                new.len(),
                1,
                "round {round}: new children {new:?} of {listed:?}"
            );
            let pid = new[0];
            drop(run);
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "round {round}: pid {pid} is still there after its future was dropped"
            );
        }
    }

    /// Catches a wait for a killed child that returns early or never returns: with the
    /// limit passed it gives up on a child that still runs (and leaves it running), and
    /// with room it returns once the killed child has been reaped.
    #[test]
    fn the_wait_for_a_child_is_bounded_and_ends_when_it_is_reaped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut child = Command::new("sleep").arg("600").spawn().expect("sleep");
            wait_exited(&mut child, Duration::from_millis(20));
            assert!(
                matches!(child.try_wait(), Ok(None)),
                "a child that was not killed is gone"
            );
            child.start_kill().expect("kill");
            wait_exited(&mut child, Duration::from_secs(30));
            assert!(
                matches!(child.try_wait(), Ok(Some(_))),
                "the killed child is not reaped"
            );
        });
    }

    /// Catches the caller's environment scrub missing from a path that starts a program:
    /// the async spawner (`run`, `kill` and the helper run go through it, and the child
    /// sees the change) and the blocking calls (`delete`, the blocking helper run). A
    /// stand-in runtime that exits 0 is run by the blocking path.
    #[test]
    fn the_environment_scrub_is_applied_to_every_program_started() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn mark(command: &mut std::process::Command) {
            CALLS.fetch_add(1, Ordering::SeqCst);
            command.env("JS_SCRUBBED", "yes");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut command = Command::new("printenv");
        command
            .arg("JS_SCRUBBED")
            .stdout(std::process::Stdio::piped());
        let (status, _, out, _) = runtime
            .block_on(KillOnDrop(Crun::new(PathBuf::from("printenv"), mark, None)).execute(command))
            .expect("printenv");
        assert!(status.success());
        assert_eq!(out, "yes\n", "the child did not see the scrub");
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        let crun = Crun::new(PathBuf::from("true"), mark, None);
        let lease = crate::unit_dir("crun-scrub");
        crun.delete_blocking(&lease, "c").expect("true exits 0");
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        crun.run_helper_blocking(&lease.join("state"), "h", &lease.join("bundle"))
            .expect("true exits 0");
        assert_eq!(CALLS.load(Ordering::SeqCst), 3);
        // The default changes nothing.
        let mut plain = std::process::Command::new("true");
        no_scrub(&mut plain);
        assert_eq!(plain.get_envs().count(), 0);
    }

    /// Catches the privilege drop applied to a daemon that is not root, or without a job
    /// user (`setresuid` would fail, or change nothing wanted), and the drop's environment:
    /// only `PATH` (the id tools' directories) and `HOME=/`, never the daemon's own.
    #[test]
    fn the_job_drop_is_made_only_for_a_root_daemon_with_a_job_user() {
        let path = std::ffi::OsStr::new("/id/tools");
        assert!(JobDrop::new(None, Some(path), 0).is_none());
        assert!(JobDrop::new(Some((900, 900)), Some(path), 1000).is_none());
        let drop = JobDrop::new(Some((900, 901)), Some(path), 0).expect("root with a job user");
        assert_eq!((drop.uid, drop.gid), (900, 901));

        let mut command = std::process::Command::new("true");
        command
            .env("XDG_RUNTIME_DIR", "/run/user/0")
            .env("TOKEN", "x");
        fn mark(command: &mut std::process::Command) {
            command.env("MARKED", "yes");
        }
        let launch = Launch {
            scrub: mark,
            job: Some(&drop),
        };
        launch.apply(&mut command);
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            [
                (
                    std::ffi::OsStr::new("HOME"),
                    Some(std::ffi::OsStr::new("/"))
                ),
                (std::ffi::OsStr::new("PATH"), Some(path)),
            ],
            "the caller's scrub runs first and env_clear leaves exactly the allowlist"
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    }

    /// Catches a root daemon allowed a runtime that is a copy (AppArmor refuses the copy its
    /// user namespace): a daemon that drops runs only the distribution's `crun` by its path
    /// (a relative name or a copy elsewhere is another path), a test double is
    /// allowed only when said so, and a daemon that does not drop runs whatever it is given.
    #[test]
    fn a_dropping_daemon_is_allowed_the_distribution_crun_and_nothing_else() {
        let allowed =
            |path: &str, drops, stand_in| runtime_allowed(Path::new(path), drops, stand_in);
        assert!(allowed("/usr/bin/crun", true, false));
        assert!(!allowed("/opt/crun/crun", true, false));
        assert!(!allowed("/usr/local/bin/crun", true, false));
        assert!(!allowed("crun", true, false));
        assert!(allowed("/opt/crun/crun", true, true));
        assert!(allowed("/opt/crun/crun", false, false));
        assert_eq!(DISTRO_CRUN, "/usr/bin/crun");
    }

    /// Catches the drop's order and its stop at the first failure, with the real calls as an
    /// ordinary user (setting one's own ids is allowed; another gid and `setgroups` are not
    /// without privilege, so the failures are the kernel's): a failing `groups` ends it
    /// before any id call (and no id changed), a gid that is not ours ends it before the uid,
    /// and our own ids pass. Run as root, the "not ours" calls succeed, so each result is
    /// compared with whether this process is root.
    #[test]
    fn the_drop_stops_at_the_first_failing_call() {
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        let root = uid == 0;
        let boom = || Err(std::io::Error::other("groups"));
        let failed = drop_ids(uid, gid + 1, boom).expect_err("groups first");
        assert_eq!(failed.to_string(), "groups");
        assert_eq!(rustix::process::getgid().as_raw(), gid);
        assert!(drop_ids(uid, gid, || Ok(())).is_ok());
        let other_gid = drop_ids(uid, gid + 1, || Ok(()));
        assert_eq!(other_gid.is_ok(), root);
        // The real `setgroups`, and the closure given to the child, as an ordinary user.
        assert_eq!(clear_groups().is_ok(), root);
        let job = JobDrop::new(Some((uid, gid)), None, 0).expect("root daemon");
        assert_eq!(job.in_child()().is_ok(), root);
    }

    /// Catches a failed `crun delete` taken for a success while the container's state is
    /// still there (nothing was deleted, and the state is the lease's to keep until the
    /// clean can remove it), one that fails when the state is already gone (the `crun run`
    /// that owned it removed it first), and a success that is reported as a failure. A
    /// stand-in runtime that exits 1 (or 0) is the `delete`.
    #[test]
    fn a_failed_delete_is_an_error_unless_the_state_is_gone() {
        let lease = crate::unit_dir("crun-delete");
        let fails = Crun::new(PathBuf::from("false"), no_scrub, None);
        let succeeds = Crun::new(PathBuf::from("true"), no_scrub, None);
        succeeds
            .delete_waiting(&lease, "c", Duration::ZERO)
            .expect("deleted");
        // Gone already: the failure lost the race to the run that owns the state.
        fails
            .delete_waiting(&lease, "c", Duration::ZERO)
            .expect("nothing left to delete");
        std::fs::create_dir_all(state_dir(&lease).join("c")).expect("state");
        let why = fails
            .delete_waiting(&lease, "c", Duration::ZERO)
            .expect_err("the state stays");
        assert!(why.starts_with("crun delete: "), "{why}");
    }

    /// A pause that counts its calls in `calls`.
    fn counting(calls: &Cell<u32>) -> impl Fn() + '_ {
        move || calls.set(calls.get() + 1)
    }

    /// Catches a wait for a path that ends at the wrong time: an absent path is gone at
    /// once, and a present one is not gone when the limit is zero.
    #[test]
    fn the_wait_for_a_path_to_go_ends_at_once_or_at_the_limit() {
        let dir = crate::unit_dir("crun-gone");
        let path = dir.join("state");
        assert!(wait_gone(&path, Duration::ZERO));
        std::fs::create_dir(&path).expect("mkdir");
        assert!(!wait_gone(&path, Duration::ZERO));
    }

    /// Catches a wait that does not look again after a pause: the path goes during the
    /// first pause, and the wait returns true after exactly that one pause.
    #[test]
    fn the_wait_for_a_path_sees_it_go_after_a_pause() {
        let dir = crate::unit_dir("crun-gone-after-pause");
        let path = dir.join("state");
        std::fs::create_dir(&path).expect("mkdir");
        let pauses = Cell::new(0);
        let gone = wait_gone_by(&path, Duration::from_secs(60), &Instant::now, &|| {
            pauses.set(pauses.get() + 1);
            std::fs::remove_dir(&path).expect("removed");
        });
        assert!(gone);
        assert_eq!(pauses.get(), 1);
    }

    /// Catches a wait that never gives up or gives up early: with a clock that moves one
    /// second per reading and a 3 s limit, a path that stays is given up on after exactly
    /// two pauses.
    #[test]
    fn the_wait_for_a_path_gives_up_at_the_limit_by_the_clock() {
        let dir = crate::unit_dir("crun-gone-limit");
        let path = dir.join("state");
        std::fs::create_dir(&path).expect("mkdir");
        let start = Instant::now();
        let readings = Cell::new(0u32);
        let clock = || {
            let at = readings.get();
            readings.set(at + 1);
            start + Duration::from_secs(u64::from(at))
        };
        let pauses = Cell::new(0);
        let gone = wait_gone_by(&path, Duration::from_secs(3), &clock, &counting(&pauses));
        assert!(!gone);
        assert_eq!(pauses.get(), 2);
    }

    /// Catches a wait that looks at the clock or pauses for a path that is already gone.
    #[test]
    fn the_wait_for_an_absent_path_does_not_pause() {
        let dir = crate::unit_dir("crun-gone-absent");
        let pauses = Cell::new(0);
        let gone = wait_gone_by(
            &dir.join("none"),
            Duration::from_secs(60),
            &Instant::now,
            &counting(&pauses),
        );
        assert!(gone);
        assert_eq!(pauses.get(), 0);
    }

    /// The real pause returns (it sleeps a few milliseconds).
    #[test]
    fn the_real_pause_returns() {
        nap();
    }

    /// The `Io` hook `crun run` is given has nothing to close once the child has the
    /// pipes (the crate calls it for `create` and `exec`, not `run`).
    #[tokio::test]
    async fn the_pipe_io_has_nothing_to_close_after_the_start() {
        let null = || std::fs::File::open("/dev/null").expect("/dev/null");
        PipeIo {
            stdin: null(),
            stdout: null().into(),
            stderr: null().into(),
        }
        .close_after_start()
        .await;
    }
}
