//! `js-agent __native-proof`: a development verb that runs one native job on a Mac through the
//! production runtime, to prove it with the signed agent that holds Full Disk Access. It is
//! removed once the end-to-end proof through the job service runs a native job.
//!
//! It checks its flags as the agent checks its native flags ([`NativeSettings::from_flags`]),
//! asks for Full Disk Access as the agent does at start, opens [`NativeRuntime::open`] over
//! the real supervisor ([`SystemLauncher`]) and runs one start through `create_and_start`,
//! `poll` (with a `stop` after `--stop-after` seconds, with `--grace`, when asked) and
//! `remove`. It prints the start's steps, how the job ended, the tails of its output and
//! error, the outcome of the removal and each sweep of the run's processes (`sweep: <lease>
//! <tally as JSON>`); the retirement's proof is logged on the error
//! stream. Real seconds are fine here: it is not the agent.
//!
//! ```text
//! js-agent __native-proof --accounts <dir> --range <first>-<last> --state <dir>
//!     [--ready <n>] [--network off|loopback|on|open] [--deny-read <path>]...
//!     [--input <host dir>=<dest>]... [--output <path>]... [--cwd <dir>]
//!     [--stop-after <s>] [--grace <s>] -- <program> [<arg>...]
//! ```

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{Isolation, NativeSpec, NetworkLevel, OutputKind, OutputSpec};

use super::{NativeFlags, NativeRuntime, NativeSettings, PfFlags, grant};
use crate::lifecycle::{Body, ResolvedMount, RunSpec, Runtime, Status};
use crate::mac_supervisor::SystemLauncher;

/// The verb.
pub const VERB: &str = "__native-proof";

/// The pause between two polls.
const PAUSE: Duration = Duration::from_millis(100);

/// The exit statuses.
mod status {
    /// The job exited 0 and its removal retired its account.
    pub const OK: u8 = 0;
    /// The job failed, or the removal did.
    pub const FAILED: u8 = 1;
    /// The runtime could not be opened or the job not started.
    pub const REFUSED: u8 = 2;
    /// The command line is wrong.
    pub const USAGE: u8 = 64;
}

/// The command line, parsed.
#[derive(Debug, Default)]
struct Args {
    accounts: Option<PathBuf>,
    range: Option<String>,
    ready: Option<usize>,
    state: Option<PathBuf>,
    network: Option<String>,
    deny_read: Vec<PathBuf>,
    inputs: Vec<(PathBuf, String)>,
    outputs: Vec<String>,
    cwd: String,
    stop_after: Option<u64>,
    grace: u64,
    command: Vec<String>,
}

/// Parses the words after the verb.
fn parse(words: &[OsString]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut words = words.iter().map(|word| word.to_string_lossy().into_owned());
    while let Some(word) = words.next() {
        if word == "--" {
            args.command = words.by_ref().collect();
            break;
        }
        let mut value = || words.next().ok_or(format!("{word} needs a value"));
        match word.as_str() {
            "--accounts" => args.accounts = Some(PathBuf::from(value()?)),
            "--range" => args.range = Some(value()?),
            "--ready" => args.ready = Some(number(&word, &value()?)?),
            "--state" => args.state = Some(PathBuf::from(value()?)),
            "--network" => args.network = Some(value()?),
            "--deny-read" => args.deny_read.push(PathBuf::from(value()?)),
            "--input" => {
                let given = value()?;
                let (host, dest) = given
                    .split_once('=')
                    .ok_or(format!("--input {given}: not <host dir>=<dest>"))?;
                args.inputs.push((PathBuf::from(host), dest.to_owned()));
            }
            "--output" => args.outputs.push(value()?),
            "--cwd" => args.cwd = value()?,
            "--stop-after" => args.stop_after = Some(number(&word, &value()?)?),
            "--grace" => args.grace = number(&word, &value()?)?,
            other => return Err(format!("{other}: not a flag of {VERB}")),
        }
    }
    if args.command.is_empty() {
        return Err("no program after --".to_owned());
    }
    Ok(args)
}

/// `text` as a number, or an error naming `flag`.
fn number<T: std::str::FromStr>(flag: &str, text: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag} {text}: not a number"))
}

/// The network level of a job given as a ceiling's name.
fn level(name: &str) -> NetworkLevel {
    match name {
        "loopback" => NetworkLevel::Loopback,
        "on" => NetworkLevel::On,
        "open" => NetworkLevel::Open,
        _ => NetworkLevel::Off,
    }
}

/// Runs the verb on `args` (the program name first, the verb second).
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let parsed = match parse(args.get(2..).unwrap_or_default()) {
        Ok(parsed) => parsed,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return ExitCode::from(status::USAGE);
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    ExitCode::from(prove(&parsed, out, err))
}

/// Opens the runtime and runs the job of `args`; the exit status.
fn prove(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let Some(state) = &args.state else {
        let _ = writeln!(err, "js-agent: {VERB}: --state is required");
        return status::USAGE;
    };
    let flags = NativeFlags {
        accounts: args.accounts.as_deref(),
        account_range: args.range.as_deref(),
        account_ready: args.ready,
        homes: None,
        network: args.network.as_deref(),
        tool_roots: &[],
        deny_read: &args.deny_read,
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let journal = state.join("native-leases");
    let settings = match NativeSettings::from_flags(&flags, &[state.as_path()], &journal) {
        Ok(Some(settings)) => settings,
        Ok(None) => {
            let _ = writeln!(err, "js-agent: {VERB}: --accounts is required");
            return status::USAGE;
        }
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return status::USAGE;
        }
    };
    let held = grant::ask(grant::list(Path::new(grant::PRIVACY_DIR)));
    let _ = writeln!(out, "full disk access: {}", held.as_str());
    let config = settings.with_full_disk_access(held).config().clone();
    let launcher = match SystemLauncher::current() {
        Ok(launcher) => Arc::new(launcher),
        Err(why) => {
            let _ = writeln!(out, "the agent's own path: {why}");
            return status::REFUSED;
        }
    };
    let mut runtime = match NativeRuntime::open(launcher, config, 1) {
        Ok(runtime) => runtime,
        Err(why) => {
            let _ = writeln!(out, "open: {}", why.0);
            return status::REFUSED;
        }
    };
    let scratch = state.join("proof-run");
    if let Err(why) = fresh(&scratch) {
        let _ = writeln!(out, "scratch {}: {why}", scratch.display());
        return status::REFUSED;
    }
    let Some((program, rest)) = args.command.split_first() else {
        return status::USAGE;
    };
    let spec = spec_of(args, (program, rest), scratch);
    let id = match runtime.create_and_start(&spec) {
        Ok(id) => id,
        Err(why) => {
            let _ = writeln!(out, "start: {}", why.0);
            return status::REFUSED;
        }
    };
    for (step, at_ms) in runtime.start_steps(&id) {
        let _ = writeln!(out, "step: {step} at {at_ms}");
    }
    let mut polls = 0_u64;
    let ended = loop {
        match runtime.poll(&id) {
            Ok(Status::Running) => {}
            Ok(ended) => break ended,
            Err(why) => {
                let _ = writeln!(out, "poll: {}", why.0);
                break Status::Gone;
            }
        }
        polls += 1;
        if args.stop_after.is_some_and(|after| polls == after * 10) {
            let _ = writeln!(out, "stop: grace {} s", args.grace);
            let grace = Duration::from_secs(args.grace);
            if let Err(why) = runtime.stop(&id, grace) {
                let _ = writeln!(out, "stop: {}", why.0);
            }
        }
        std::thread::sleep(PAUSE);
    };
    let _ = writeln!(out, "ended: {ended:?}");
    if let Ok(Some(tail)) = runtime.log_tail(&id) {
        let _ = writeln!(out, "stdout:\n{}", tail.stdout);
        let _ = writeln!(out, "stderr:\n{}", tail.stderr);
    }
    let removed = runtime.remove(&id);
    match &removed {
        Ok(()) => {
            let _ = writeln!(out, "remove: ok");
            if let Some((name, retired)) = runtime.last_retired() {
                let _ = writeln!(out, "retired: {name} killed {:?}", retired.killed);
                let _ = writeln!(out, "retired.gone: {:?}", retired.gone);
            }
        }
        Err(why) => {
            let _ = writeln!(out, "remove: {}", why.0);
        }
    }
    for (lease, tally) in runtime.sweeps() {
        let tally = serde_json::to_string(tally).unwrap_or_else(|why| why.to_string());
        let _ = writeln!(out, "sweep: {lease} {tally}");
    }
    let exited_0 = matches!(ended, Status::Exited(exit) if exit.code == 0);
    match exited_0 && removed.is_ok() {
        true => status::OK,
        false => status::FAILED,
    }
}

/// The start of `args` running `program` with `rest`, its scratch directory `scratch`.
fn spec_of(args: &Args, (program, rest): (&String, &[String]), scratch: PathBuf) -> RunSpec {
    let network = level(args.network.as_deref().unwrap_or("off"));
    RunSpec {
        run_id: "native-proof".to_owned(),
        body: Body::Native(NativeSpec {
            program: program.clone(),
            args: rest.to_vec(),
            env: Default::default(),
            cwd: args.cwd.clone(),
            inputs: Vec::new(),
            outputs: args
                .outputs
                .iter()
                .map(|path| OutputSpec {
                    path: path.clone(),
                    kind: OutputKind::default(),
                })
                .collect(),
            network,
            ports: Vec::new(),
        }),
        env: Vec::new(),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: args
            .inputs
            .iter()
            .map(|(host, dest)| ResolvedMount {
                host: host.clone(),
                target: dest.clone(),
                ro: true,
            })
            .collect(),
        scratch,
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        fuse: false,
        labels: Default::default(),
        bundle: None,
        network: Default::default(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        log_sink: None,
        xcode: None,
    }
}

/// Makes `dir` empty and the agent's alone.
fn fresh(dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(why) if why.kind() != std::io::ErrorKind::NotFound => return Err(why),
        _ => {}
    }
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}
