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

/// The default [`EnvScrub`]: nothing is removed.
pub fn no_scrub(_: &mut std::process::Command) {}

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
struct KillOnDrop(EnvScrub);

#[async_trait]
impl Spawner for KillOnDrop {
    async fn execute(&self, mut cmd: Command) -> runc::Result<(ExitStatus, u32, String, String)> {
        cmd.kill_on_drop(true);
        (self.0)(cmd.as_std_mut());
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
    pub(crate) fn new(program: PathBuf, scrub: EnvScrub) -> Self {
        Self { program, scrub }
    }

    /// The client for the lease in `lease`: its own state directory and log.
    fn client(&self, lease: &Path) -> Result<Runc, String> {
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state_dir(lease))
            .log(log_path(lease))
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop(self.scrub)));
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
        opts.custom_spawner(Arc::new(KillOnDrop(self.scrub)));
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
        let mut command = std::process::Command::new(&self.program);
        (self.scrub)(&mut command);
        let output = command
            .arg("--root")
            .arg(state)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "crun {what}: {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("run {}: {e}", self.program.display())),
        }
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
            let mut run = Box::pin(KillOnDrop(super::no_scrub).execute(command));
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
            .block_on(KillOnDrop(mark).execute(command))
            .expect("printenv");
        assert!(status.success());
        assert_eq!(out, "yes\n", "the child did not see the scrub");
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        let crun = Crun::new(PathBuf::from("true"), mark);
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

    /// Catches a failed `crun delete` taken for a success while the container's state is
    /// still there (nothing was deleted, and the state is the lease's to keep until the
    /// clean can remove it), one that fails when the state is already gone (the `crun run`
    /// that owned it removed it first), and a success that is reported as a failure. A
    /// stand-in runtime that exits 1 (or 0) is the `delete`.
    #[test]
    fn a_failed_delete_is_an_error_unless_the_state_is_gone() {
        let lease = crate::unit_dir("crun-delete");
        let fails = Crun::new(PathBuf::from("false"), no_scrub);
        let succeeds = Crun::new(PathBuf::from("true"), no_scrub);
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
