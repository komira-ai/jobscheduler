//! The supervisor's steps ([`supervise`]) over the calls they make ([`SuperviseHost`]), and the
//! real host ([`SystemHost`]), whose job start is one of the two starts that keep the
//! responsibility (`crates/xtask/src/spawn_sites.rs`, `KEEPERS`).

use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Child, ExitStatus};

use js_mac_pool::seatbelt::SandboxInit;
use js_mac_session::disclaim::{Kept, Program};
use rustix::process::{Pid, Signal, kill_process_group};

use super::record::{Ended, ExitRecord, JobRusage, Ready, Records, Step, ended_of, status_of};
use super::{SuperviseSpec, VERB, status};

/// The calls of the supervisor's steps, one method a step, so a test can watch their order and
/// fail each in turn. [`SystemHost`] makes the real ones.
pub trait SuperviseHost {
    /// Opens the records in `dir` ([`Records::open`]), while the supervisor is still root.
    ///
    /// # Errors
    /// A record file could not be made.
    fn open_records(&self, dir: &Path) -> io::Result<Records>;

    /// Opens a new audit session for this process, with `auid` as its audit user; its id.
    ///
    /// # Errors
    /// The session could not be opened (only root may).
    fn audit_session(&self, auid: u32) -> io::Result<i32>;

    /// Whether this process runs as root: only then does it drop to the slot user.
    fn is_root(&self) -> bool;

    /// Drops this process to `uid`, with `gid` its only group, for good.
    ///
    /// # Errors
    /// A call failed, or root could be regained after it.
    fn drop_to(&self, uid: u32, gid: u32) -> io::Result<()>;

    /// Sets the file-size limit to `max_file_bytes` and the process limit to `max_processes`,
    /// each soft and hard.
    ///
    /// # Errors
    /// A limit could not be set.
    fn limits(&self, max_file_bytes: u64, max_processes: u32) -> io::Result<()>;

    /// Applies the Seatbelt profile `init` to this process and every process it starts.
    ///
    /// # Errors
    /// The profile was refused (the library's own message).
    fn apply_profile(&self, init: &SandboxInit) -> io::Result<()>;

    /// Starts the job's program with its arguments, in a process group of its own, with this
    /// process's environment, working directory and standard streams.
    ///
    /// # Errors
    /// The program could not be started.
    fn spawn(&self, spec: &SuperviseSpec) -> io::Result<Box<dyn JobChild>>;

    /// Writes the ready record ([`Records::write_ready`]).
    ///
    /// # Errors
    /// It could not be written whole.
    fn write_ready(&self, records: &mut Records, ready: &Ready) -> io::Result<()>;

    /// Writes the exit record ([`Records::write_exit`]).
    ///
    /// # Errors
    /// It could not be written whole.
    fn write_exit(&self, records: &mut Records, record: &ExitRecord) -> io::Result<()>;
}

/// How the job ended, and what it used ([`JobRusage`]), as its wait reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    /// Its wait status.
    pub status: ExitStatus,
    /// What it and the processes it waited for used.
    pub usage: JobRusage,
}

/// The job's process, as the supervisor holds it.
pub trait JobChild {
    /// Its pid, also the id of its process group.
    fn pid(&self) -> i32;

    /// Waits until it ends and reaps it: how it ended and what it used.
    ///
    /// # Errors
    /// It cannot be waited for.
    fn wait(&mut self) -> io::Result<Reaped>;

    /// Kills its process group (`SIGKILL`).
    ///
    /// # Errors
    /// The signal could not be sent.
    fn kill_group(&mut self) -> io::Result<()>;
}

/// Runs the start of `spec` over `host`, in this order: opens the records; with
/// `new_session`, opens an audit session; as root, drops to the slot user; sets the limits;
/// applies the profile; starts the job; writes the ready record; waits for the job; writes the
/// exit record. Returns the supervisor's exit status ([`status_of`]).
///
/// A step that fails ends the start there: the job is never started, the exit record says
/// [`Ended::SetupFailed`] with the step and why, one line `js-agent: __supervise-native:
/// <step>: <why>` goes to `err`, and the status is [`status::SETUP_FAILED`]. When the records
/// cannot be opened nothing can be written but that line. When the ready record cannot be
/// written the job, already started, is killed with its process group, and the exit record says
/// the records failed: the agent could not have learned the job's pid to stop it.
pub fn supervise(host: &dyn SuperviseHost, spec: &SuperviseSpec, err: &mut dyn Write) -> u8 {
    let mut records = match host.open_records(Path::new(&spec.record_dir)) {
        Ok(records) => records,
        Err(why) => {
            say(err, Step::Records, &why);
            return status::SETUP_FAILED;
        }
    };
    let record = match start(host, spec) {
        Ok((child, asid)) => run_job(host, &mut records, child, asid),
        Err((step, why)) => setup_failed(step, &why, None, None),
    };
    if let Ended::SetupFailed { step, why } = &record.ended {
        say(err, *step, why);
    }
    if let Err(why) = host.write_exit(&mut records, &record) {
        say(
            err,
            Step::Records,
            &format!("writing the exit record: {why}"),
        );
    }
    status_of(&record)
}

/// The job, started, and the audit session it runs in.
type Started = (Box<dyn JobChild>, Option<i32>);

/// The step that failed, and why.
type Failed = (Step, io::Error);

/// The steps up to the job's start: the job and the audit session it runs in, or the step that
/// failed and why.
fn start(host: &dyn SuperviseHost, spec: &SuperviseSpec) -> Result<Started, Failed> {
    let asid = if spec.new_session {
        Some(
            host.audit_session(spec.uid)
                .map_err(at(Step::AuditSession))?,
        )
    } else {
        None
    };
    if host.is_root() {
        host.drop_to(spec.uid, spec.gid).map_err(at(Step::Drop))?;
    }
    host.limits(spec.max_file_bytes, spec.max_processes)
        .map_err(at(Step::Limits))?;
    SandboxInit::new(&spec.profile, &spec.params)
        .map_err(io::Error::other)
        .and_then(|init| host.apply_profile(&init))
        .map_err(at(Step::Profile))?;
    let child = host.spawn(spec).map_err(at(Step::Spawn))?;
    Ok((child, asid))
}

/// The error of `step`, with the step.
fn at(step: Step) -> impl Fn(io::Error) -> Failed {
    move |why| (step, why)
}

/// Writes the ready record, waits for the job, and says how it ended.
fn run_job(
    host: &dyn SuperviseHost,
    records: &mut Records,
    mut child: Box<dyn JobChild>,
    asid: Option<i32>,
) -> ExitRecord {
    let job_pid = child.pid();
    if let Err(why) = host.write_ready(records, &Ready { job_pid, asid }) {
        // The job, killed: the signal could not fail for a child not yet waited for.
        let _ = child.kill_group();
        let _ = child.wait();
        let why = format!("writing the ready record: {why}");
        return setup_failed(Step::Records, &why, Some(job_pid), asid);
    }
    match child.wait() {
        Ok(reaped) => ExitRecord {
            ended: ended_of(reaped.status),
            job_pid: Some(job_pid),
            asid,
            usage: Some(reaped.usage),
        },
        Err(why) => {
            let _ = child.kill_group();
            let why = format!("waiting for the job: {why}");
            setup_failed(Step::Spawn, &why, Some(job_pid), asid)
        }
    }
}

/// The exit record of a start that failed at `step`.
fn setup_failed(
    step: Step,
    why: &dyn std::fmt::Display,
    job_pid: Option<i32>,
    asid: Option<i32>,
) -> ExitRecord {
    ExitRecord {
        ended: Ended::SetupFailed {
            step,
            why: why.to_string(),
        },
        job_pid,
        asid,
        usage: None,
    }
}

/// Writes the line that says `step` failed, and why.
fn say(err: &mut dyn Write, step: Step, why: &dyn std::fmt::Display) {
    let _ = writeln!(err, "js-agent: {VERB}: {step}: {why}");
}

/// The real calls: the records, the job's start and its wait here; the audit session, the
/// drop, the limits and the profile in `super::macos`, which only macOS has (elsewhere each
/// fails with [`io::ErrorKind::Unsupported`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemHost;

impl SuperviseHost for SystemHost {
    fn open_records(&self, dir: &Path) -> io::Result<Records> {
        Records::open(dir)
    }

    fn audit_session(&self, auid: u32) -> io::Result<i32> {
        platform::audit_session(auid)
    }

    fn is_root(&self) -> bool {
        rustix::process::geteuid().is_root()
    }

    fn drop_to(&self, uid: u32, gid: u32) -> io::Result<()> {
        platform::drop_to(uid, gid)
    }

    fn limits(&self, max_file_bytes: u64, max_processes: u32) -> io::Result<()> {
        platform::limits(max_file_bytes, max_processes)
    }

    fn apply_profile(&self, init: &SandboxInit) -> io::Result<()> {
        platform::apply_profile(init)
    }

    /// Starts the program keeping the responsibility ([`Kept::Job`]), so the job answers to the
    /// privacy controls for the agent. Nothing of the environment, the working directory or
    /// the streams is set: the job gets this process's own.
    fn spawn(&self, spec: &SuperviseSpec) -> io::Result<Box<dyn JobChild>> {
        let child = Program::new(&spec.program, &spec.args)
            .keep_responsibility(Kept::Job)
            .process_group(0)
            .spawn()?;
        Ok(Box::new(SystemJob(child)))
    }

    fn write_ready(&self, records: &mut Records, ready: &Ready) -> io::Result<()> {
        records.write_ready(ready)
    }

    fn write_exit(&self, records: &mut Records, record: &ExitRecord) -> io::Result<()> {
        records.write_exit(record)
    }
}

/// The job, a child of this process.
struct SystemJob(Child);

impl JobChild for SystemJob {
    fn pid(&self) -> i32 {
        Pid::from_child(&self.0).as_raw_nonzero().get()
    }

    /// `wait4` of the job, which reaps it and says what it used. This process installs no
    /// signal handler, so no signal interrupts the wait. The [`Child`] is not waited for
    /// again: it was reaped here, and dropping it neither waits nor kills.
    fn wait(&mut self) -> io::Result<Reaped> {
        let mut status: libc::c_int = 0;
        // SAFETY: `rusage` is a C struct of integers, for which all zeroes is a valid value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals of the types `wait4` writes, and it keeps
        // neither after it returns.
        let reaped = unsafe { libc::wait4(self.pid(), &raw mut status, 0, &raw mut usage) };
        if reaped < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Reaped {
            status: ExitStatus::from_raw(status),
            usage: rusage_of(&usage),
        })
    }

    fn kill_group(&mut self) -> io::Result<()> {
        Ok(kill_process_group(Pid::from_child(&self.0), Signal::KILL)?)
    }
}

/// The bytes of one unit of `ru_maxrss`: bytes on macOS, KiB on Linux.
#[cfg(target_os = "macos")]
const MAXRSS_UNIT: u64 = 1;
/// The bytes of one unit of `ru_maxrss`: bytes on macOS, KiB on Linux.
#[cfg(not(target_os = "macos"))]
const MAXRSS_UNIT: u64 = 1024;

/// What `usage`, as `wait4` filled it, says the job used: its user and system time together,
/// and its largest resident set in bytes. A negative field (which the kernel never reports)
/// counts as 0.
fn rusage_of(usage: &libc::rusage) -> JobRusage {
    let micros = |time: libc::timeval| {
        let seconds = u64::try_from(time.tv_sec).unwrap_or(0);
        let micros = u64::try_from(time.tv_usec).unwrap_or(0);
        seconds.saturating_mul(1_000_000).saturating_add(micros)
    };
    JobRusage {
        cpu_usec: micros(usage.ru_utime).saturating_add(micros(usage.ru_stime)),
        max_rss_bytes: u64::try_from(usage.ru_maxrss)
            .unwrap_or(0)
            .saturating_mul(MAXRSS_UNIT),
    }
}

#[cfg(target_os = "macos")]
use super::macos as platform;

/// Off macOS: no audit session, drop, limit or profile, so a start fails at its first step that
/// needs one.
#[cfg(not(target_os = "macos"))]
mod platform {
    use std::io;

    use js_mac_pool::seatbelt::SandboxInit;

    /// What each call answers.
    fn macos_only() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "macOS only")
    }

    /// Fails: macOS only.
    pub(super) fn audit_session(_auid: u32) -> io::Result<i32> {
        Err(macos_only())
    }

    /// Fails: macOS only.
    pub(super) fn drop_to(_uid: u32, _gid: u32) -> io::Result<()> {
        Err(macos_only())
    }

    /// Fails: macOS only.
    pub(super) fn limits(_max_file_bytes: u64, _max_processes: u32) -> io::Result<()> {
        Err(macos_only())
    }

    /// Fails: macOS only.
    pub(super) fn apply_profile(_init: &SandboxInit) -> io::Result<()> {
        Err(macos_only())
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
pub(crate) mod tests;
