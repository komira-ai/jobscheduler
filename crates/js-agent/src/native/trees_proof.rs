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

use super::{NativeConfig, NativeFlags, NativeRuntime, NativeSettings, PfFlags, grant, proof};
use crate::lifecycle::{
    Body, JobFs, ProviderAnswer, ProviderCall, Providers, RunSpec, Runtime, Status,
};
use crate::mac_supervisor::SystemLauncher;
use crate::native_fs::MountRoot;
use crate::provider::ProviderClient;

/// The verb.
pub const VERB: &str = "__native-trees-proof";

/// The pause between two polls.
const PAUSE: Duration = Duration::from_millis(100);

/// The most asks of one provider call, a [`PAUSE`] apart (two minutes in production).
const ASKS: u32 = 1200;

/// The two runs, by id.
const RUNS: [&str; 2] = ["trees-proof-a", "trees-proof-b"];

/// The target of each run's tree.
const TARGET: &str = "/work/.kbf-tree";

/// The exit statuses.
pub mod status {
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

/// What opens the mount providers of the verb, for the agent id and the provider (its name,
/// socket and uid) its flags gave.
pub type ProvidersOpener<'a> =
    &'a dyn Fn(&str, &[(String, PathBuf, u32)]) -> std::io::Result<Box<dyn Providers>>;

/// What the verb runs over: the runtime and the providers it opens, and the pause between
/// two asks of a provider call not answered yet and between two polls of a job.
pub struct Over<'a> {
    /// Makes the runtime from the configuration the flags gave.
    pub runtime: proof::Opener<'a>,
    /// Makes the providers' client.
    pub providers: ProvidersOpener<'a>,
    /// Waits a moment.
    pub pause: &'a dyn Fn(),
}

/// Runs the verb on `args` (the program name first, the verb second), over the production
/// runtime and the real providers' client.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let providers = |agent: &str, sockets: &[(String, PathBuf, u32)]| {
        ProviderClient::new(agent, sockets).map(|client| Box::new(client) as Box<dyn Providers>)
    };
    let over = Over {
        runtime: &open_production,
        providers: &providers,
        pause: &|| std::thread::sleep(PAUSE),
    };
    run_with(args, out, err, &over)
}

/// [`run`] over `over`: how a test runs the verb over a pool, a launcher and providers of its
/// own. The flags, the calls, the report and the exit status are the verb's.
#[doc(hidden)]
pub fn run_with(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    over: &Over<'_>,
) -> ExitCode {
    match parse(args.get(2..).unwrap_or_default()) {
        Ok(parsed) => ExitCode::from(prove(&parsed, over, out, err)),
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            ExitCode::from(status::USAGE)
        }
    }
}

/// The production runtime over `config`: the real supervisor (this program) and the pool of
/// `--accounts` on this Mac.
fn open_production(config: NativeConfig) -> Result<NativeRuntime, String> {
    let launcher = SystemLauncher::current().map_err(|why| format!("own path: {why}"))?;
    NativeRuntime::open(Arc::new(launcher), config, 1).map_err(|why| why.0)
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

/// Opens the runtime and the providers, has both trees made, runs both jobs and releases the
/// trees; the exit status.
fn prove(args: &Args, over: &Over<'_>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let given = match given(args) {
        Ok(given) => given,
        Err(why) => {
            let _ = writeln!(err, "js-agent: {VERB}: {why}");
            return status::USAGE;
        }
    };
    let mut runtime = match open(args, &given, over.runtime, out) {
        Ok(runtime) => runtime,
        Err(why) => {
            let _ = writeln!(out, "open: {why}");
            return status::REFUSED;
        }
    };
    let (name, socket) = given.provider;
    let sockets = [(name.clone(), socket.clone(), args.provider_uid)];
    let mut providers = match (over.providers)(&args.agent_id, &sockets) {
        Ok(providers) => providers,
        Err(why) => {
            let _ = writeln!(out, "provider client: {why}");
            return status::REFUSED;
        }
    };
    let mut calls = Calls {
        providers: providers.as_mut(),
        name,
        pause: over.pause,
    };
    let mut trees = Vec::new();
    for run_id in RUNS {
        match tree_of(&mut calls, run_id, given.reference) {
            Ok(dir) => {
                let _ = writeln!(out, "tree {run_id}: {dir}");
                trees.push(PathBuf::from(dir));
            }
            Err(why) => {
                let _ = writeln!(out, "tree {run_id}: {why}");
                release_all(&mut calls, out);
                return status::REFUSED;
            }
        }
    }
    let mut ok = true;
    for (at, run_id) in RUNS.iter().enumerate() {
        let other = &trees[1 - at];
        let job = (args.command.as_slice(), over.pause);
        match run_one(&mut runtime, &given, job, (run_id, &trees[at], other), out) {
            Ok(passed) => ok &= passed,
            Err(why) => {
                let _ = writeln!(out, "run {run_id}: {why}");
                ok = false;
            }
        }
    }
    ok &= release_all(&mut calls, out);
    if ok { status::OK } else { status::FAILED }
}

/// The runtime of the flags made by `opener`, with the trees root as its one mount root (or,
/// with `--without-tree-rules`, a root that is neither denied nor the trees root).
fn open(
    args: &Args,
    given: &Given<'_>,
    opener: proof::Opener<'_>,
    out: &mut dyn Write,
) -> Result<NativeRuntime, String> {
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
    opener(config)
}

/// The provider `name` of the verb and the pause between two asks of a call.
struct Calls<'a> {
    providers: &'a mut dyn Providers,
    name: &'a str,
    pause: &'a dyn Fn(),
}

impl Calls<'_> {
    /// Asks `call` until it is answered, at most [`ASKS`] times with a pause between two.
    fn ask(&mut self, call: &ProviderCall) -> ProviderAnswer {
        for _ in 1..ASKS {
            let answer = self.providers.ask(self.name, call);
            if answer != ProviderAnswer::Pending {
                return answer;
            }
            (self.pause)();
        }
        match self.providers.ask(self.name, call) {
            ProviderAnswer::Pending => ProviderAnswer::Unavailable("no answer".to_owned()),
            answer => answer,
        }
    }
}

/// The directory the provider answered for the tree `reference` of `run_id`.
fn tree_of(calls: &mut Calls<'_>, run_id: &str, reference: &str) -> Result<String, String> {
    let prepare = ProviderCall::Prepare {
        run_id: run_id.to_owned(),
        index: 0,
        reference: reference.to_owned(),
        mode: MountMode::ReadOnly,
    };
    match calls.ask(&prepare) {
        ProviderAnswer::Done(_) => {}
        other => return Err(format!("prepare: {other:?}")),
    }
    let mount = ProviderCall::Mount {
        run_id: run_id.to_owned(),
        index: 0,
    };
    match calls.ask(&mount) {
        ProviderAnswer::Done(Some(dir)) => Ok(dir),
        other => Err(format!("mount: {other:?}")),
    }
}

/// Releases both runs' sources and sweeps this agent's placements, then asks the provider to
/// mount each again; whether every release and the sweep were answered done and no source is
/// mounted any more.
fn release_all(calls: &mut Calls<'_>, out: &mut dyn Write) -> bool {
    let mut done = true;
    let releases = RUNS
        .iter()
        .map(|run_id| ProviderCall::Release {
            run_id: (*run_id).to_owned(),
            index: 0,
        })
        .chain([ProviderCall::Sweep { keep: Vec::new() }]);
    for call in releases {
        let answer = calls.ask(&call);
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
        let answer = calls.ask(&mount);
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

/// Runs `command` as `run_id` with its tree `own` at [`TARGET`], given the other run's tree
/// and the trees root as its last two arguments, pausing with `pause` between two polls;
/// whether it exited 0 and its removal retired its account.
fn run_one(
    runtime: &mut NativeRuntime,
    given: &Given<'_>,
    (command, pause): (&[String], &dyn Fn()),
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
    let (program, rest) = command.split_first().ok_or("no program")?;
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
            Ok(Status::Running) => pause(),
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
