//! A container run by a supervisor process of its own, so that it outlives the agent.
//!
//! [`Oci::start`] runs `crun run` on a thread of the agent: when the agent ends, so do the
//! pipes the container writes to and the thread that waits for it. [`Oci::start_supervised`]
//! runs the same machinery (the init's report file, the capture of the output, `crun run`)
//! in a process of its own instead: the daemon's binary in the supervise mode
//! (`<daemon> <modes.supervise> <crun> <lease dir> <name> <file cap> <tail bytes> <drain
//! grace ms>`, [`run`]), in a process group of its own, started with the job drop and the
//! environment scrub every `crun` gets, with `SIGHUP`, `SIGINT` and `SIGTERM` blocked. It
//! is not a child the agent has to keep: an agent that ends leaves it and the container
//! running, and a later agent finds it again ([`Oci::adopt_supervised`]).
//!
//! **The handshake:** the supervisor makes the init's report file and the capture files,
//! then writes a ready word to its standard output. [`Oci::start_supervised`] returns once
//! it has read the word; a supervisor that exits without it, says something else, or is
//! silent for [`READY_WAIT`], is killed and reaped, and the start fails.
//!
//! **The exit record:** when `crun run` has returned and the drain of the output is over,
//! the supervisor writes `exit.json` in the lease directory whole (a temporary file,
//! synced, renamed, the directory synced), and exits. The record holds how the run ended,
//! what each stream wrote (the tail, the byte count, whether the file lacks bytes) and the
//! wall clock time it was written at. Its form is version 1, JSON:
//!
//! ```text
//! {"version":1,"end":{"exited":<i32>}|{"failed":"<text>"},"finished_ms":<u64>,
//!  "stdout":{"tail":"<hex>","total":<u64>,"file_cut":<bool>},"stderr":{...}}
//! ```
//!
//! A reader ignores fields it does not know and refuses another version, so later forms
//! add fields. The record is read through [`read_exit_record`], which opens it without
//! following a link and reads at most 16.1 MiB of a regular file. A stream's tail
//! of more than 4 MiB is recorded as its last 4 MiB.
//!
//! **The handle:** [`Supervised`] reads the record. A run is over when the record is there;
//! a supervisor that is gone ([`SupervisorId::alive`]) and wrote none ended without one,
//! which is a failure of the run. It signals nothing by itself: [`Supervised::force`] ends
//! the supervisor's process group (unless the supervisor has written its record, and so only
//! exits), and the container is ended, as for every run, by the kill of the lease's cgroup
//! and [`Oci::remove`].
//!
//! The parts every supervisor shares (the exit record, the handshake, the supervisor's
//! process identity, the handle and the signal mask) are the crate `js-supervise`, and the
//! items of it this module has always offered are re-exported here. What is here is the crun
//! arm: the start of the supervisor and the supervise mode that runs `crun run`.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub use js_supervise::{
    ExitRecord, READY_WAIT, SUPERVISED_POLL, Supervised, SupervisorId, end_supervisor,
    exit_record_path, read_exit_record, recorded_supervisor, supervisor_path,
};
use js_supervise::{READY, note_supervisor, write_record};

use super::crun::{Crun, RunEnd, no_scrub};
use super::logs::{Capture, Launch, LogLimits};
use super::{Oci, init, logs};
use crate::container::ContainerSpec;

/// Starts `command` as a supervisor (stdin and stderr null, stdout a pipe, a process
/// group of its own) and returns once it has written `ready` to its stdout. A child
/// that does not say it within `limit` is killed (its process group) and reaped. A
/// thread waits for the child, so it never stays a zombie ([`Supervised::handshake`]).
///
/// # Errors
/// The command could not be run, its answer is not `ready`, or it gave none within
/// `limit`.
fn spawn(
    command: Command,
    lease: PathBuf,
    ready: &[u8],
    limit: Duration,
) -> Result<Supervised, String> {
    let (child, from_child) = start(command).map_err(|e| e.to_string())?;
    Supervised::handshake(child, from_child, lease, ready, limit).map_err(|e| e.to_string())
}

/// Starts `command` with stdin and stderr null, stdout the write end of a new pipe, in a
/// process group of its own: the child and the read end of the pipe.
fn start(mut command: Command) -> std::io::Result<(Child, std::io::PipeReader)> {
    let (from_child, to_child) = std::io::pipe()?;
    command
        .stdin(Stdio::null())
        .stdout(to_child)
        .stderr(Stdio::null())
        .process_group(0);
    let spawned = command.spawn();
    // The command holds the write end of the pipe: closed, the child's exit ends the
    // read.
    drop(command);
    Ok((spawned?, from_child))
}

impl Oci {
    /// Starts the container of `spec` under a supervisor process (`<daemon> <modes.supervise>
    /// <crun> <lease dir> <name> <file cap> <tail bytes> <drain grace ms>`, the file cap being
    /// `file_cap`, or the runtime's [`LogLimits::file_cap`] when it is `None`, and the tail and
    /// the grace the runtime's), run with the job
    /// drop and the environment scrub every `crun` gets, stdin null, stdout a pipe used once
    /// for the ready word, stderr null, in a process group of its own. Returns once the
    /// supervisor wrote its ready word (it has made the init report and the capture files); a
    /// supervisor that does not answer is killed and reaped, and the call fails. A thread
    /// waits for the child so it never stays a zombie. The supervisor's pid and start time are
    /// then written to the lease directory ([`recorded_supervisor`]). The bundle
    /// ([`Oci::create`]) must be written.
    ///
    /// # Errors
    /// An exit record is already in the lease directory, or the supervisor could not be
    /// started or did not answer (the lease directory has the files of an earlier start, say).
    pub fn start_supervised(
        &self,
        spec: &ContainerSpec,
        file_cap: Option<u64>,
    ) -> Result<Supervised, String> {
        let record = exit_record_path(&spec.dir);
        if record.symlink_metadata().is_ok() {
            return Err(format!(
                "{}: an exit record is already there",
                record.display()
            ));
        }
        let limits = self.run_logs(file_cap);
        let mut command = Command::new(&self.daemon);
        command
            .arg(&self.modes.supervise)
            .arg(self.crun.program())
            .arg(&spec.dir)
            .arg(&spec.name)
            .arg(limits.file_cap.to_string())
            .arg(limits.tail.to_string())
            .arg(limits.drain_grace.as_millis().to_string());
        // As the job user, or as the daemon for a host container (`Oci::crun_of`).
        self.crun_of(spec).prepare(&mut command);
        let supervised = spawn(command, spec.dir.clone(), READY, READY_WAIT)?;
        note_supervisor(&spec.dir, supervised.id());
        Ok(supervised)
    }

    /// The handle of a container an earlier process started under the supervisor `id`, whose
    /// lease directory is `lease`. Starts nothing.
    #[must_use]
    pub fn adopt_supervised(&self, lease: &Path, id: SupervisorId) -> Supervised {
        Supervised::adopt(lease, id)
    }
}

/// Makes the runtime the supervisor drives `crun run` on.
type RuntimeMaker = fn() -> std::io::Result<tokio::runtime::Runtime>;

/// One thread, with the drivers `crun`'s child needs.
fn one_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// What the supervisor was told: `<crun> <lease dir> <name> <file cap> <tail bytes> <drain
/// grace ms>`.
struct Request {
    program: PathBuf,
    lease: PathBuf,
    name: String,
    limits: LogLimits,
}

impl Request {
    fn parse(args: &[OsString]) -> Option<Self> {
        let [program, lease, name, file_cap, tail, grace] = args else {
            return None;
        };
        let number = |arg: &OsString| arg.to_str()?.parse::<u64>().ok();
        Some(Self {
            program: PathBuf::from(program),
            lease: PathBuf::from(lease),
            name: name.to_str()?.to_owned(),
            limits: LogLimits {
                file_cap: number(file_cap)?,
                tail: usize::try_from(number(tail)?).ok()?,
                drain_grace: Duration::from_millis(number(grace)?),
            },
        })
    }
}

/// The supervise mode, its arguments after the mode word (`modes.supervise`) being `<crun>
/// <lease dir> <name> <file cap> <tail bytes> <drain grace ms>`: blocks `SIGHUP`, `SIGINT`
/// and `SIGTERM` (blocked, not ignored, so that the `crun` it starts has them as any
/// process does), makes the init's report file and the capture files, tells its parent it
/// is ready on standard output, runs `crun run` over the lease's bundle as [`Oci::start`]
/// does, and writes the exit record. Returns 0 once the record is written, 1 when a file
/// could not be made, the ready word not written or the record not written, and 2 for
/// arguments it cannot read.
#[must_use]
pub fn run(args: &[OsString]) -> u8 {
    // Before anything makes a thread, so that every thread has the mask.
    js_supervise::block_stop_signals();
    run_with(args, &mut std::io::stdout().lock(), one_thread_runtime)
}

/// [`run`] telling `ready` it is ready, on the runtime `runtime` makes (the place it can
/// fail, a test's to cause), with the signals as they are.
fn run_with(args: &[OsString], ready: &mut dyn std::io::Write, runtime: RuntimeMaker) -> u8 {
    let Some(request) = Request::parse(args) else {
        eprintln!("supervise-run: usage: <crun> <lease> <name> <cap> <tail> <grace>, not {args:?}");
        return 2;
    };
    let (report, capture) = match prepare(&request) {
        Ok(made) => made,
        Err(why) => {
            eprintln!("supervise-run: {why}");
            return 1;
        }
    };
    if let Err(e) = ready.write_all(READY).and_then(|()| ready.flush()) {
        eprintln!("supervise-run: the ready word: {e}");
        return 1;
    }
    let end = crun_run(&request, report, &capture, runtime);
    let record = ExitRecord {
        end,
        tail: capture.tail(),
        finished_ms: wall_ms(),
    };
    match write_record(&request.lease, &record) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("supervise-run: the exit record: {e}");
            1
        }
    }
}

/// The init's report file and the capture files of the lease.
fn prepare(request: &Request) -> Result<(std::fs::File, Arc<Capture>), String> {
    let report = init::create_report(&request.lease)?;
    let capture = Arc::new(Capture::create(&request.lease, request.limits)?);
    Ok((report, capture))
}

/// Runs `crun run` over the lease's bundle on the runtime `runtime` makes, draining the
/// output into `capture`, as [`Oci::start`] does. The process already runs as the job user
/// with the scrubbed environment, so `crun` is started as it is.
fn crun_run(
    request: &Request,
    report: std::fs::File,
    capture: &Arc<Capture>,
    runtime: RuntimeMaker,
) -> RunEnd {
    let crun = Crun::new(request.program.clone(), no_scrub, None);
    let (lease, name) = (request.lease.clone(), request.name.clone());
    let bundle = lease.join("bundle");
    let launch: Launch = Box::new(move |stdout, stderr| {
        Box::pin(async move {
            crun.run(&lease, &name, &bundle, report, stdout, stderr)
                .await
        })
    });
    match runtime() {
        Ok(runtime) => runtime.block_on(logs::supervise(capture.clone(), launch)),
        Err(e) => RunEnd::Failed(format!("the runtime of the supervisor: {e}")),
    }
}

/// The wall clock, in milliseconds since the Unix epoch.
fn wall_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "supervise_tests.rs"]
mod tests;
