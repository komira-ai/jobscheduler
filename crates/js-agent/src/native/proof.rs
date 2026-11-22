//! `js-agent __native-proof`: a development verb that runs one native job on a Mac through the
//! production runtime, to prove it with the signed agent that holds Full Disk Access. It is
//! removed once the end-to-end proof through the job service runs a native job.
//!
//! It checks its flags as the agent checks its native flags ([`NativeSettings::from_flags`]),
//! asks for Full Disk Access as the agent does at start, opens [`NativeRuntime::open`] over
//! the real supervisor ([`SystemLauncher`]) and runs one start through `create_and_start`,
//! `poll` (with a `stop` after `--stop-after` seconds, with `--grace`, when asked) and
//! `remove`. It prints the start's steps, how the job ended, the tails of its output and
//! error, and the outcome of the removal with the retirement's proof. Real seconds are fine
//! here: it is not the agent.
//!
//! ```text
//! js-agent __native-proof --accounts <dir> --range <first>-<last> --state <dir>
//!     [--ready <n>] [--network off|loopback|on|open] [--deny-read <path>]...
//!     [--input <host dir>=<dest>]... [--output <path>]... [--cwd <dir>]
//!     [--stop-after <s>] [--grace <s>]
//!     [--mount-root <dir> --put-root <dir> --put-user <user> --put-mount <dir>=<target>]
//!     -- <program> [<arg>...]
//! ```

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use js_agent_wire::{
    Isolation, JobMount, MountMode, MountSource, NativeSpec, NetworkLevel, OutputKind, OutputSpec,
    Propagation,
};

use super::{
    MountRoot, NativeConfig, NativeFlags, NativeRuntime, NativeSettings, PfFlags, PutFlags, grant,
};
use crate::lifecycle::{Body, FsPrepared, JobFs, ResolvedMount, RunSpec, Runtime, Status};
use crate::mac_supervisor::SystemLauncher;

/// The verb.
pub const VERB: &str = "__native-proof";

/// The pause between two polls.
const PAUSE: Duration = Duration::from_millis(100);

/// The exit statuses.
pub mod status {
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
    /// The put ingest (`--put-root`, `--put-user`), the read-write mount root that holds it
    /// (`--mount-root`) and the run's put mount (`--put-mount <host dir>=<target>`).
    put: PutFlags,
    mount_root: Option<PathBuf>,
    put_mount: Option<(String, String)>,
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
            "--put-root" => args.put.native_put_root = Some(PathBuf::from(value()?)),
            "--put-user" => args.put.native_put_user = Some(value()?),
            "--mount-root" => args.mount_root = Some(PathBuf::from(value()?)),
            "--put-mount" => {
                let given = value()?;
                let (host, target) = given
                    .split_once('=')
                    .ok_or(format!("--put-mount {given}: not <host dir>=<target>"))?;
                args.put_mount = Some((host.to_owned(), target.to_owned()));
            }
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

/// What opens the runtime of the verb from the configuration its flags made.
pub type Opener<'a> = &'a dyn Fn(NativeConfig) -> Result<NativeRuntime, String>;

/// Runs the verb on `args` (the program name first, the verb second), over the production
/// runtime: [`NativeRuntime::open`] with the real supervisor.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    run_with(args, out, err, &open_production)
}

/// [`run`] with `open` making the runtime from the configuration the flags gave: how a test
/// runs the verb over a pool of its own. The flags, the steps, the report and the exit status
/// are the verb's.
#[doc(hidden)]
pub fn run_with(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    open: Opener<'_>,
) -> ExitCode {
    match parse(args.get(2..).unwrap_or_default()) {
        Ok(parsed) => ExitCode::from(prove(&parsed, open, out, err)),
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            ExitCode::from(status::USAGE)
        }
    }
}

/// The production runtime over `config`: the real supervisor (this program) and the pool of
/// `--accounts` on this Mac.
fn open_production(config: NativeConfig) -> Result<NativeRuntime, String> {
    let launcher =
        SystemLauncher::current().map_err(|why| format!("the agent's own path: {why}"))?;
    NativeRuntime::open(Arc::new(launcher), config, 1).map_err(|why| format!("open: {}", why.0))
}

/// Opens the runtime with `open` and runs the job of `args`; the exit status.
fn prove(args: &Args, open: Opener<'_>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
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
    let roots: Vec<MountRoot> = args
        .mount_root
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .map(|path| MountRoot {
            path,
            read_only: false,
            trees: false,
        })
        .collect();
    let settings = match settings.with_mount_roots(roots.clone()) {
        Ok(settings) => settings,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return status::USAGE;
        }
    };
    let put = match super::put_of(&args.put, &roots, &node_of_name) {
        Ok(put) => put,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return status::USAGE;
        }
    };
    let _ = writeln!(out, "put: {put:?}");
    let held = grant::ask(grant::list(Path::new(grant::PRIVACY_DIR)));
    let _ = writeln!(out, "full disk access: {}", held.as_str());
    let config = settings.with_full_disk_access(held).config().clone();
    let mut runtime = match open(config) {
        Ok(runtime) => runtime.with_put(put),
        Err(why) => {
            let _ = writeln!(out, "{why}");
            return status::REFUSED;
        }
    };
    if let Some((host, target)) = &args.put_mount {
        let fs = JobFs {
            layers: Vec::new(),
            mounts: vec![JobMount {
                source: MountSource::HostPath { path: host.clone() },
                target: target.clone(),
                mode: MountMode::ReadWrite,
                sub_path: None,
                no_exec: true,
                propagation: Propagation::Private,
            }],
        };
        let checked = runtime.check_job_fs(&fs, &[]);
        let resolved = runtime.resolve_fs("native-proof", "", &fs, 0);
        let _ = writeln!(out, "put mount: checked {checked:?}, resolved {resolved:?}");
        if !matches!(resolved, FsPrepared::Ready) {
            return status::REFUSED;
        }
    }
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
    // What the lifecycle would keep as the finding `outputs-committed`, and the pack as the
    // node finds it, read at once (the node may take it any moment after).
    match runtime.outputs_report(&id) {
        Some(report) => {
            let _ = writeln!(out, "finding outputs-committed {}", report.note());
            if let Some(put) = &args.put_mount {
                let pack = Path::new(&put.0).join("pack");
                match fs::symlink_metadata(&pack) {
                    Ok(meta) => {
                        use std::os::unix::fs::MetadataExt as _;
                        let _ = writeln!(
                            out,
                            "pack at once: uid {} gid {} mode {:o} bytes {}",
                            meta.uid(),
                            meta.gid(),
                            meta.mode() & 0o7777,
                            meta.len()
                        );
                    }
                    Err(why) => {
                        let _ = writeln!(out, "pack at once: {why} (the node took it)");
                    }
                }
            }
        }
        None => {
            let _ = writeln!(out, "finding outputs-committed: none");
        }
    }
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

/// The uid and group of the user `name` in the directory service.
fn node_of_name(name: &str) -> std::io::Result<Option<crate::mac_put::Node>> {
    use js_mac_session::helper::Host as _;
    let account = js_mac_session::macos::MacHost.account(name)?;
    Ok(account.map(|account| crate::mac_put::Node {
        uid: account.uid,
        gid: account.gid,
    }))
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
