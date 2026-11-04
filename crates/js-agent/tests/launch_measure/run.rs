//! The measurement (Linux, root): the agent's own `CrunRuntime` started on a busybox job,
//! once with egress mode none and once with an allow-list, sample by sample.
//!
//! Each variant has a runtime of its own (`runtime_host::RuntimeHost`), opened as the agent
//! opens it, with each lease's directory beside its run's: mode none starts no egress and
//! gives the runtime no port, so its job has loopback only; the allow-list starts the host's
//! `Egress` once (its firewall loaded, as the agent does at start) with one endpoint,
//! `192.0.2.10:9300`, and its runtime wires every container through it (the veth, the /30,
//! the set member, the resolver file, and the init's `--wait-route` gate that holds the job
//! until the route is there). The endpoint is configured and nothing listens there: the job
//! opens no connection, so the interval covers the wiring, not a connect.
//!
//! One sample, in the lifecycle's order: the run's scratch directory and its empty lease
//! directory are made (`Runtime::lease_dir`); the per-start firewall check
//! (`Runtime::check_egress`) runs; `CrunRuntime::stage` makes the lease (its directories, the
//! cgroup, the bundle; the image is in the store before the first sample, so no pull or
//! unpack happens). Then a FIFO `up` is made, mode `0666`, in the run's scratch directory,
//! which the runtime binds at `/scratch` (a bind shares the host's inode, so the job and this
//! program open one pipe; `/work` is an overlayfs, where a FIFO does not). This program opens
//! it for reading and writing: the open never waits, and since this program holds a writing
//! end, a read waits for a byte instead of returning the end of file before the job opens it.
//! A thread blocks in that read. The start stamp is taken and `CrunRuntime::start_staged` is
//! called (`crun run`, and for the allow-list the pid and the wiring); the job,
//! `sh -c 'echo up > /scratch/up'`, writes, and the end stamp is taken by the reading thread
//! when its blocking read returns the first byte: one wake-up, no poll interval inside the
//! interval. Both stamps are `std::time::Instant` (monotonic). Then, outside the interval, the
//! run is removed (its wiring released) with its directories, before the next sample starts.
//! `create_and_start` is `stage` then `start_staged`, so the interval is the second half of
//! the runtime's start entry.
//!
//! A sample whose check, stage or start returns an error, or whose job does not write within
//! [`REPORT_LIMIT`] (or ends without writing), is failed (the limit is a guard, never a
//! duration in the table): counted, listed with its error, and
//! in no statistic (`table`). A fault of the harness itself (a directory, the FIFO, a removal)
//! ends the program with an error, since every later sample would stand on it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use js_agent::crun::start_egress;
use js_agent::egress::{Config, Egress, Flags as EgressFlags};
use js_agent::lifecycle::{RunHandle, RunSpec, Runtime as _, Status};
use js_contain::oci::helper::HelperModes;
use js_mac_session::disclaim::Program;
use rustix::fs::{CWD, Mode, mkfifoat};

use crate::flags::{self, Flags};
use crate::out;
use crate::runtime_host::{RuntimeHost, Setup};
use crate::table::{self, Facts, Method, Sample, Samples, Variant};

/// The job: one write to the FIFO, its first instruction.
// /scratch, not /work: /work is an overlayfs, where a FIFO does not share the host's pipe.
const SCRIPT: &str = "echo up > /scratch/up";

/// The FIFO's name in the run's scratch directory, which the job sees at `/scratch`.
const FIFO: &str = "up";

/// The one endpoint of the allow-list: the allowed endpoint of the far host the egress tests
/// build. The job opens no connection, so nothing needs to listen there.
const ALLOWED: &str = "192.0.2.10:9300";

/// How long a started job has to write to its FIFO before its sample is failed.
const REPORT_LIMIT: Duration = Duration::from_secs(60);

/// How often, while a job has not written, the runtime is asked whether it has ended.
const ASK_EVERY: Duration = Duration::from_millis(100);

/// How long a job that has ended has for its write to reach the reading thread.
const AFTER_END: Duration = Duration::from_secs(1);

/// The program: the helper modes first (a helper container runs this binary), then the
/// flags, the check for root, the measurement and its result.
pub fn main() -> ExitCode {
    if let Some(code) = js_contain::modes::dispatch(&HelperModes::default()) {
        return ExitCode::from(code);
    }
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let Ok(args) = args else {
        eprintln!("js-launch-measure: an argument is not UTF-8");
        return ExitCode::from(2);
    };
    let flags = match flags::parse(&args) {
        Ok(flags) => flags,
        Err(why) => {
            eprintln!("js-launch-measure: {why}");
            return ExitCode::from(2);
        }
    };
    if !rustix::process::geteuid().is_root() {
        eprintln!(
            "js-launch-measure: run it as root: the crun runtime and the egress it \
             measures are the root agent's"
        );
        return ExitCode::from(2);
    }
    let measured = measure(&flags);
    let text = match &measured {
        Ok(text) => {
            print!("{text}");
            text.clone()
        }
        Err(why) => {
            let text = format!("js-launch-measure: {why}\n");
            eprint!("{text}");
            text
        }
    };
    if let Some(path) = &flags.summary_file
        && let Err(why) = out::append(path, &text)
    {
        eprintln!("js-launch-measure: append to {}: {why}", path.display());
        return ExitCode::FAILURE;
    }
    match measured {
        Ok(_) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// What the program reads of the machine.
fn facts(flags: &Flags) -> Result<Facts, String> {
    let release = Path::new("/proc/sys/kernel/osrelease");
    let kernel =
        fs::read_to_string(release).map_err(|why| format!("read {}: {why}", release.display()))?;
    let version = Program::new(&flags.crun, ["--version"])
        .output()
        .map_err(|why| format!("{} --version: {why}", flags.crun.display()))?;
    let crun = String::from_utf8_lossy(&version.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let cpus = std::thread::available_parallelism()
        .map_err(|why| format!("the CPUs this process may use: {why}"))?;
    Ok(Facts {
        image: flags.image_name.clone(),
        kernel: kernel.trim().to_owned(),
        crun,
        cpus: cpus.get(),
    })
}

/// `dir` made, empty and searchable by every user.
fn fresh_dir(dir: &Path) -> Result<(), String> {
    let made = fs::create_dir_all(dir)
        .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o755)));
    made.map_err(|why| format!("make {}: {why}", dir.display()))?;
    let mut entries = fs::read_dir(dir).map_err(|why| format!("list {}: {why}", dir.display()))?;
    match entries.next() {
        None => Ok(()),
        Some(_) => Err(format!("{} is not empty", dir.display())),
    }
}

/// The samples of both variants, and the result.
fn measure(flags: &Flags) -> Result<String, String> {
    let facts = facts(flags)?;
    fresh_dir(&flags.work_dir)?;
    let daemon =
        std::env::current_exe().map_err(|why| format!("this program's own path: {why}"))?;
    let setup = Setup {
        crun: &flags.crun,
        busybox: &flags.busybox,
        job_user: flags.job_user,
        daemon: &daemon,
        leases_beside: true,
    };
    let allow = [ALLOWED.to_owned()];
    let allow_list = Config::from_flags(&EgressFlags {
        mode: Some("allow-list"),
        allow: &allow,
        ..EgressFlags::default()
    })
    .map_err(|why| format!("the allow-list: {why}"))?;
    // As the agent starts its egress: once, before any run. Mode none starts nothing.
    let none_egress = start_egress(&Config::none(), Egress::start)
        .map_err(|why| format!("egress mode none: {why}"))?;
    let wired_egress = start_egress(&allow_list, Egress::start)
        .map_err(|why| format!("start the allow-list's egress: {why}"))?;
    let open = |variant: Variant, egress: Option<&Egress>| -> Result<RuntimeHost, String> {
        let name = variant.name();
        let work = flags.work_dir.join(name);
        fresh_dir(&work)?;
        RuntimeHost::open(name, &format!("launch-{name}"), &work, &setup, egress)
            .map_err(|why| format!("variant {name}: {why}"))
    };
    // Dropped in the reverse order: the second goes home to the first's cgroup, the first to
    // the cgroup this process started in.
    let mut none = open(Variant::None, none_egress.as_ref())?;
    let mut wired = open(Variant::Wired, wired_egress.as_ref())?;
    let mut taken = [Variant::None, Variant::Wired].map(|variant| Samples {
        variant,
        samples: Vec::new(),
    });
    for index in 0..flags.warmup.saturating_add(flags.runs) {
        for (host, samples) in [&mut none, &mut wired].into_iter().zip(taken.iter_mut()) {
            let name = samples.variant.name();
            let sample = take_sample(host, &format!("{name}-{index}"))?;
            if index >= flags.warmup {
                samples.samples.push(sample);
            } else if let Some(why) = &sample.failed {
                eprintln!("js-launch-measure: warm-up sample {name}-{index}: {why}");
            }
        }
    }
    drop(wired);
    drop(none);
    let method = Method {
        runs: flags.runs,
        warmup: flags.warmup,
    };
    table::report(&taken, method, &facts).map_err(|why| {
        let errors: Vec<String> = taken
            .iter()
            .flat_map(|variant| {
                let name = variant.variant.name();
                variant
                    .samples
                    .iter()
                    .enumerate()
                    .filter_map(move |(at, sample)| {
                        let why = sample.failed.as_ref()?;
                        Some(format!("\n- {name} sample {}: {why}", at + 1))
                    })
            })
            .collect();
        format!("{why}{}", errors.concat())
    })
}

/// The thread that reads the FIFO, blocked from before the start, and the stamp it takes when
/// its read returns.
struct Reader {
    pipe: Arc<File>,
    read: Receiver<(Instant, std::io::Result<usize>)>,
    thread: Option<JoinHandle<()>>,
}

impl Reader {
    /// Starts the thread on `pipe`.
    fn start(pipe: File) -> Self {
        let pipe = Arc::new(pipe);
        let (sent, read) = mpsc::channel();
        let reading = Arc::clone(&pipe);
        let thread = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            let got = (&*reading).read(&mut byte);
            let _ = sent.send((Instant::now(), got));
        });
        Self {
            pipe,
            read,
            thread: Some(thread),
        }
    }
}

impl Drop for Reader {
    /// Ends the thread: a read still waiting gets a byte of this program's own.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            if !thread.is_finished() {
                let _ = (&*self.pipe).write_all(b"x");
            }
            let _ = thread.join();
        }
    }
}

/// A failed sample of `micros`, with the error `why`.
fn failed(micros: u64, why: String) -> Sample {
    Sample {
        micros,
        failed: Some(why),
    }
}

/// One sample of the run `run_id` on `host`; see the module docs.
fn take_sample(host: &mut RuntimeHost, run_id: &str) -> Result<Sample, String> {
    host.enter()?;
    let spec = host.spec(run_id, SCRIPT)?;
    // As the lifecycle does: the lease's directory beside the run's, made empty before the
    // create.
    let lease = host
        .runtime
        .lease_dir(run_id)
        .ok_or_else(|| format!("the runtime names no lease directory for {run_id}"))?;
    fs::create_dir(&lease).map_err(|why| format!("make {}: {why}", lease.display()))?;
    let sample = match host.runtime.check_egress() {
        Err(why) => failed(0, format!("check the egress: {why}")),
        Ok(()) => staged_then_timed(host, &spec)?,
    };
    for dir in [&spec.scratch, &lease] {
        match js_contain::remove::remove_tree(dir) {
            Err(why) if why.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("remove {}: {why}", dir.display()));
            }
            _ => {}
        }
    }
    Ok(sample)
}

/// `CrunRuntime::stage`, the timed start of the staged run, and its removal, outside the
/// interval.
fn staged_then_timed(host: &mut RuntimeHost, spec: &RunSpec) -> Result<Sample, String> {
    let id = match host.runtime.stage(spec) {
        Ok(id) => id,
        Err(why) => return Ok(failed(0, format!("stage: {why}"))),
    };
    let sample = timed(host, &id, &spec.scratch);
    host.runtime
        .remove(&id)
        .map_err(|why| format!("remove the run {}: {why}", spec.run_id))?;
    sample
}

/// The interval itself: the FIFO made in the run's scratch directory `scratch` and held open
/// (outside it), the start stamp, `CrunRuntime::start_staged`, and the reading thread's stamp
/// of the job's first byte.
fn timed(host: &mut RuntimeHost, id: &RunHandle, scratch: &Path) -> Result<Sample, String> {
    let fifo = scratch.join(FIFO);
    let harness =
        |what: &str, why: &dyn std::fmt::Display| format!("{what} {}: {why}", fifo.display());
    mkfifoat(CWD, &fifo, Mode::from_raw_mode(0o666)).map_err(|why| harness("make", &why))?;
    // The job's user is none of this program's: the FIFO is written by others.
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o666))
        .map_err(|why| harness("set the mode of", &why))?;
    let pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .map_err(|why| harness("open", &why))?;
    let reader = Reader::start(pipe);
    let started = Instant::now();
    if let Err(why) = host.runtime.start_staged(id) {
        return Ok(failed(micros(started.elapsed()), format!("start: {why}")));
    }
    Ok(match reported_at(host, id, &reader, started) {
        Ok(at) => Sample {
            micros: micros(at.saturating_duration_since(started)),
            failed: None,
        },
        Err(why) => failed(micros(started.elapsed()), why),
    })
}

/// When the reading thread read the job's first byte; the error when the job ended without
/// writing, did not write within [`REPORT_LIMIT`], or the read failed.
fn reported_at(
    host: &mut RuntimeHost,
    id: &RunHandle,
    reader: &Reader,
    started: Instant,
) -> Result<Instant, String> {
    let read = |got: (Instant, std::io::Result<usize>)| match got {
        (at, Ok(1)) => Ok(at),
        (_, Ok(n)) => Err(format!("the FIFO read gave {n} bytes")),
        (_, Err(why)) => Err(format!("read the FIFO: {why}")),
    };
    loop {
        match reader.read.recv_timeout(ASK_EVERY) {
            Ok(got) => return read(got),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the reading thread ended without a reading".to_owned());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Ok(Status::Exited(exit)) = host.runtime.poll(id) {
            if let Ok(got) = reader.read.recv_timeout(AFTER_END) {
                return read(got);
            }
            let tail = host.runtime.log_tail(id).ok().flatten().unwrap_or_default();
            return Err(format!(
                "the job ended with code {} without writing to the FIFO; stderr: {:?}",
                exit.code, tail.stderr
            ));
        }
        if started.elapsed() >= REPORT_LIMIT {
            return Err(format!(
                "the job did not write to the FIFO within {REPORT_LIMIT:?}"
            ));
        }
    }
}

/// `elapsed` in whole microseconds.
fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}
