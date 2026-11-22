//! `js-agent __native-trees-proof`: a development verb that proves a job's trees on a Mac with
//! the signed agent: two runs, A and B, each the tree of one tree list from the mount
//! provider, each started through the production runtime as its own slot account under the
//! real profile, with the other run's tree mounted meanwhile. It is removed once the
//! end-to-end proof through the job service runs a native job with a tree.
//!
//! For each run it asks the provider to prepare and mount the tree (`Prepare`, `Mount`, as the
//! lifecycle does for a start), then hands the runtime the answered directory as the
//! read-only, `no_exec` mount at `/work/.kbf-tree` that the build service sends, so the
//! runtime checks, resolves, links and profiles it as it does a lifecycle's start. The job is
//! `<program> <args>... <other run's tree> <trees root>`. After both runs it releases both
//! sources, sweeps the agent id's placements and asks for each source's directory again, which
//! the provider must no longer answer. Each step is printed; real seconds are fine
//! here: it is not the agent.
//!
//! `--without-tree-rules` builds the runs' profiles as an agent without a trees root would:
//! the trees root is neither a read deny nor the run's trees, and a tree is granted as any
//! mount. It is the control that shows what the trees root's rules hold back.
//!
//! ```text
//! js-agent __native-trees-proof --accounts <dir> --range <first>-<last> --state <dir>
//!     --provider <name>=<socket> --provider-uid <uid> --trees-root <path> --ref <tree list>
//!     [--agent-id <id>] [--without-tree-rules] -- <program> [<arg>...]
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
    Isolation, JobMount, MountMode, MountSource, NativeSpec, NetworkLevel, Propagation,
};

use super::{NativeFlags, NativeRuntime, NativeSettings, PfFlags, grant};
use crate::lifecycle::{
    Body, JobFs, ProviderAnswer, ProviderCall, Providers as _, RunSpec, Runtime, Status,
};
use crate::mac_supervisor::SystemLauncher;
use crate::native_fs::MountRoot;
use crate::provider::ProviderClient;

/// The verb.
pub const VERB: &str = "__native-trees-proof";

/// The pause between two polls.
const PAUSE: Duration = Duration::from_millis(100);

/// The longest wait for one provider call's answer.
const CALL_GUARD: Duration = Duration::from_secs(120);

/// The two runs, by id.
const RUNS: [&str; 2] = ["trees-proof-a", "trees-proof-b"];

/// The target of each run's tree.
const TARGET: &str = "/work/.kbf-tree";

/// The exit statuses.
mod status {
    /// Both jobs exited 0, both removals retired their accounts, both sources were released.
    pub const OK: u8 = 0;
    /// A job, a removal or a release failed.
    pub const FAILED: u8 = 1;
    /// The runtime could not be opened, a tree not had or a job not started.
    pub const REFUSED: u8 = 2;
    /// The command line is wrong.
    pub const USAGE: u8 = 64;
}

/// The command line, parsed.
#[derive(Debug, Default)]
struct Args {
    accounts: Option<PathBuf>,
    range: Option<String>,
    state: Option<PathBuf>,
    provider: Option<(String, PathBuf)>,
    provider_uid: u32,
    trees_root: Option<PathBuf>,
    reference: Option<String>,
    agent_id: String,
    without_tree_rules: bool,
    command: Vec<String>,
}

/// Parses the words after the verb.
fn parse(words: &[OsString]) -> Result<Args, String> {
    let mut args = Args {
        agent_id: "trees-proof".to_owned(),
        ..Args::default()
    };
    let mut words = words.iter().map(|word| word.to_string_lossy().into_owned());
    while let Some(word) = words.next() {
        if word == "--" {
            args.command = words.by_ref().collect();
            break;
        }
        if word == "--without-tree-rules" {
            args.without_tree_rules = true;
            continue;
        }
        let mut value = || words.next().ok_or(format!("{word} needs a value"));
        match word.as_str() {
            "--accounts" => args.accounts = Some(PathBuf::from(value()?)),
            "--range" => args.range = Some(value()?),
            "--state" => args.state = Some(PathBuf::from(value()?)),
            "--provider" => {
                let given = value()?;
                let (name, socket) = given
                    .split_once('=')
                    .ok_or(format!("--provider {given}: not <name>=<socket>"))?;
                args.provider = Some((name.to_owned(), PathBuf::from(socket)));
            }
            "--provider-uid" => {
                let given = value()?;
                args.provider_uid = given
                    .parse()
                    .map_err(|_| format!("--provider-uid {given}: not a uid"))?;
            }
            "--trees-root" => args.trees_root = Some(PathBuf::from(value()?)),
            "--ref" => args.reference = Some(value()?),
            "--agent-id" => args.agent_id = value()?,
            other => return Err(format!("{other}: not a flag of {VERB}")),
        }
    }
    if args.command.is_empty() {
        return Err("no program after --".to_owned());
    }
    Ok(args)
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

/// What [`prove`] needs that the command line must give.
struct Given<'a> {
    state: &'a Path,
    provider: &'a (String, PathBuf),
    root: PathBuf,
    reference: &'a str,
}

/// The required parts of `args`, the trees root canonical.
fn given(args: &Args) -> Result<Given<'_>, String> {
    let need = |flag: &str| format!("{flag} is required");
    let state = args.state.as_deref().ok_or_else(|| need("--state"))?;
    let provider = args.provider.as_ref().ok_or_else(|| need("--provider"))?;
    let root = args
        .trees_root
        .as_deref()
        .ok_or_else(|| need("--trees-root"))?;
    let root = fs::canonicalize(root).map_err(|why| format!("--trees-root: {why}"))?;
    let reference = args.reference.as_deref().ok_or_else(|| need("--ref"))?;
    Ok(Given {
        state,
        provider,
        root,
        reference,
    })
}

/// Opens the runtime, has both trees made, runs both jobs and releases the trees; the exit
/// status.
fn prove(args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let given = match given(args) {
        Ok(given) => given,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return status::USAGE;
        }
    };
    let mut runtime = match open(args, &given, out) {
        Ok(runtime) => runtime,
        Err(why) => {
            let _ = writeln!(out, "open: {why}");
            return status::REFUSED;
        }
    };
    let (name, socket) = given.provider;
    let sockets = [(name.clone(), socket.clone(), args.provider_uid)];
    let mut client = match ProviderClient::new(&args.agent_id, &sockets) {
        Ok(client) => client,
        Err(why) => {
            let _ = writeln!(out, "provider client: {why}");
            return status::REFUSED;
        }
    };
    let mut trees = Vec::new();
    for run_id in RUNS {
        match tree_of(&mut client, name, run_id, given.reference) {
            Ok(dir) => {
                let _ = writeln!(out, "tree {run_id}: {dir}");
                trees.push(PathBuf::from(dir));
            }
            Err(why) => {
                let _ = writeln!(out, "tree {run_id}: {why}");
                release_all(&mut client, name, out);
                return status::REFUSED;
            }
        }
    }
    let mut ok = true;
    for (at, run_id) in RUNS.iter().enumerate() {
        let other = &trees[1 - at];
        match run_one(&mut runtime, args, &given, (run_id, &trees[at], other), out) {
            Ok(passed) => ok &= passed,
            Err(why) => {
                let _ = writeln!(out, "run {run_id}: {why}");
                ok = false;
            }
        }
    }
    ok &= release_all(&mut client, name, out);
    match ok {
        true => status::OK,
        false => status::FAILED,
    }
}

/// The runtime of the flags, with the trees root as its one mount root (or, with
/// `--without-tree-rules`, a root that is neither denied nor the trees root).
fn open(args: &Args, given: &Given<'_>, out: &mut dyn Write) -> Result<NativeRuntime, String> {
    let flags = NativeFlags {
        accounts: args.accounts.as_deref(),
        account_range: args.range.as_deref(),
        account_ready: None,
        homes: None,
        network: None,
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags::default(),
    };
    let journal = given.state.join("native-leases");
    let settings = NativeSettings::from_flags(&flags, &[given.state], &journal)?
        .ok_or("--accounts is required")?;
    let root = MountRoot {
        path: given.root.clone(),
        read_only: true,
        trees: !args.without_tree_rules,
    };
    let held = grant::ask(grant::list(Path::new(grant::PRIVACY_DIR)));
    let _ = writeln!(out, "full disk access: {}", held.as_str());
    let mut config = settings
        .with_mount_roots(vec![root])?
        .with_full_disk_access(held)
        .config()
        .clone();
    if args.without_tree_rules {
        let _ = writeln!(out, "profile: without the trees root's rules");
        config
            .read_denies
            .retain(|denied| Path::new(denied.as_str()) != given.root);
    }
    let launcher = SystemLauncher::current().map_err(|why| format!("own path: {why}"))?;
    NativeRuntime::open(Arc::new(launcher), config, 1).map_err(|why| why.0)
}

/// Asks `call` of the provider `name` and waits for its answer.
fn ask(client: &mut ProviderClient, name: &str, call: &ProviderCall) -> ProviderAnswer {
    let first = client.ask(name, call);
    if first != ProviderAnswer::Pending {
        return first;
    }
    let _ = client.wait_answered(name, call, CALL_GUARD);
    client.ask(name, call)
}

/// The directory the provider `name` answered for the tree `reference` of `run_id`.
fn tree_of(
    client: &mut ProviderClient,
    name: &str,
    run_id: &str,
    reference: &str,
) -> Result<String, String> {
    let prepare = ProviderCall::Prepare {
        run_id: run_id.to_owned(),
        index: 0,
        reference: reference.to_owned(),
        mode: MountMode::ReadOnly,
    };
    match ask(client, name, &prepare) {
        ProviderAnswer::Done(_) => {}
        other => return Err(format!("prepare: {other:?}")),
    }
    let mount = ProviderCall::Mount {
        run_id: run_id.to_owned(),
        index: 0,
    };
    match ask(client, name, &mount) {
        ProviderAnswer::Done(Some(dir)) => Ok(dir),
        other => Err(format!("mount: {other:?}")),
    }
}

/// Releases both runs' sources and sweeps this agent's placements, then asks the provider to
/// mount each again; whether every release and the sweep were answered done and no source is
/// mounted any more.
fn release_all(client: &mut ProviderClient, name: &str, out: &mut dyn Write) -> bool {
    let mut done = true;
    let calls = RUNS
        .iter()
        .map(|run_id| ProviderCall::Release {
            run_id: (*run_id).to_owned(),
            index: 0,
        })
        .chain([ProviderCall::Sweep { keep: Vec::new() }]);
    for call in calls {
        let answer = ask(client, name, &call);
        let _ = writeln!(out, "{call:?}: {answer:?}");
        done &= matches!(answer, ProviderAnswer::Done(_));
    }
    // The provider's own word that it holds no placement: a released source is mounted no
    // more (an NFS client may show its directory for a while after).
    for run_id in RUNS {
        let mount = ProviderCall::Mount {
            run_id: run_id.to_owned(),
            index: 0,
        };
        let answer = ask(client, name, &mount);
        let held = matches!(answer, ProviderAnswer::Done(Some(_)));
        let _ = writeln!(out, "after release, mount {run_id}: {answer:?}");
        let _ = writeln!(
            out,
            "placement {run_id}: {}",
            if held { "held" } else { "released" }
        );
        done &= !held;
    }
    done
}

/// Runs the job of `args` as `run_id` with its tree `own` at [`TARGET`], given the other run's
/// tree and the trees root as its last two arguments; whether it exited 0 and its removal
/// retired its account.
fn run_one(
    runtime: &mut NativeRuntime,
    args: &Args,
    given: &Given<'_>,
    (run_id, own, other): (&str, &Path, &Path),
    out: &mut dyn Write,
) -> Result<bool, String> {
    let fs_of = JobFs {
        layers: Vec::new(),
        mounts: vec![JobMount {
            source: MountSource::HostPath {
                path: own.display().to_string(),
            },
            target: TARGET.to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: true,
            propagation: Propagation::Private,
        }],
    };
    runtime
        .check_job_fs(&fs_of, &[])
        .map_err(|why| format!("admission: {} {}", why.code, why.why))?;
    let resolved = runtime.resolve_fs(run_id, "", &fs_of, 0);
    let _ = writeln!(out, "run {run_id}: lookup {resolved:?}");
    let scratch = given.state.join(run_id);
    fresh(&scratch).map_err(|why| format!("scratch {}: {why}", scratch.display()))?;
    let (program, rest) = args.command.split_first().ok_or("no program")?;
    let mut job_args = rest.to_vec();
    job_args.extend([
        other.display().to_string(),
        given.root.display().to_string(),
    ]);
    let spec = spec_of(run_id, program, job_args, scratch);
    let id = runtime
        .create_and_start(&spec)
        .map_err(|why| format!("start: {}", why.0))?;
    let ended = loop {
        match runtime.poll(&id) {
            Ok(Status::Running) => std::thread::sleep(PAUSE),
            Ok(ended) => break ended,
            Err(why) => {
                let _ = writeln!(out, "run {run_id}: poll: {}", why.0);
                break Status::Gone;
            }
        }
    };
    let _ = writeln!(out, "run {run_id}: ended {ended:?}");
    if let Ok(Some(tail)) = runtime.log_tail(&id) {
        let _ = writeln!(out, "run {run_id}: stdout:\n{}", tail.stdout);
        let _ = writeln!(out, "run {run_id}: stderr:\n{}", tail.stderr);
    }
    let removed = runtime.remove(&id);
    let _ = writeln!(
        out,
        "run {run_id}: remove {}",
        removed
            .as_ref()
            .map_or_else(|why| why.0.clone(), |()| "ok".to_owned())
    );
    let exited_0 = matches!(ended, Status::Exited(exit) if exit.code == 0);
    Ok(exited_0 && removed.is_ok())
}

/// The start of `program` with `args` as `run_id`, its scratch directory `scratch`.
fn spec_of(run_id: &str, program: &str, args: Vec<String>, scratch: PathBuf) -> RunSpec {
    RunSpec {
        run_id: run_id.to_owned(),
        body: Body::Native(NativeSpec {
            program: program.to_owned(),
            args,
            env: Default::default(),
            cwd: String::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            network: NetworkLevel::Off,
            ports: Vec::new(),
        }),
        env: Vec::new(),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
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
