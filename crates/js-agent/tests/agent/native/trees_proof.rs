//! The development verb `js-agent __native-trees-proof` through its command line: its flags,
//! the provider calls it makes, the two runs it starts, what it prints and its exit status,
//! over a pool of a fake host, the fake launcher and a fake mount provider given through its
//! seam (`trees_proof::run_with`). The production runtime and provider are the real
//! supervisor and tierstore's socket, which the Mac proof runs.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use js_agent::lifecycle::{ProviderAnswer, ProviderCall, Providers};
use js_agent::native::trees_proof::{self, Over, VERB, status};
use js_agent::native::{NativeConfig, NativeRuntime};

use super::{FakeAccounts, FakeLauncher, Scratch, TERM, open_pool, param, pool_settings};

/// The runs of the verb, in order.
const RUNS: [&str; 2] = ["trees-proof-a", "trees-proof-b"];

/// What the fake provider holds and was asked.
#[derive(Default)]
struct State {
    /// The trees root it makes the runs' directories below.
    root: PathBuf,
    /// The runs whose source is prepared.
    prepared: BTreeSet<String>,
    /// The calls answered, in order.
    answered: Vec<ProviderCall>,
    /// The call asked last and answered `Pending`: the next ask of the same call answers.
    pending: Option<ProviderCall>,
    /// Refuse every `Prepare`.
    refuse_prepare: bool,
    /// Keep every source after a release or a sweep, as a provider that does not release.
    keeps: bool,
    /// What the verb opened its providers' client with: the agent id and the providers.
    opened: Vec<(String, Vec<Provider>)>,
}

/// A provider as the verb names it to its client: name, socket and uid.
type Provider = (String, PathBuf, u32);

/// A mount provider that answers each call the second time it is asked (the first answer is
/// `Pending`, as a socket call's is), and makes a run's directory as tierstore names it:
/// `<root>/mount-provider/<hex agent>/<hex js.<run>>/0`.
#[derive(Clone)]
struct FakeProvider(Arc<Mutex<State>>);

impl FakeProvider {
    fn new(root: &Path) -> Self {
        Self(Arc::new(Mutex::new(State {
            root: root.to_path_buf(),
            ..State::default()
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The directory of `run`'s source, made, with a file naming the run.
    fn dir_of(root: &Path, run: &str) -> PathBuf {
        let dir = root
            .join("mount-provider/aa")
            .join(hex(&format!("js.{run}")))
            .join("0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.txt"), format!("{run}\n")).unwrap();
        dir
    }
}

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

impl Providers for FakeProvider {
    fn has(&self, name: &str) -> bool {
        name == "tierstore"
    }

    fn names(&self) -> Vec<String> {
        vec!["tierstore".to_owned()]
    }

    fn ask(&mut self, _provider: &str, call: &ProviderCall) -> ProviderAnswer {
        let mut state = self.state();
        if state.pending.as_ref() != Some(call) {
            state.pending = Some(call.clone());
            return ProviderAnswer::Pending;
        }
        state.pending = None;
        state.answered.push(call.clone());
        match call {
            ProviderCall::Prepare { .. } if state.refuse_prepare => {
                ProviderAnswer::Refused("no tree list of that key".to_owned())
            }
            ProviderCall::Prepare { run_id, .. } => {
                state.prepared.insert(run_id.clone());
                ProviderAnswer::Done(None)
            }
            ProviderCall::Mount { run_id, .. } if state.prepared.contains(run_id) => {
                let dir = Self::dir_of(&state.root, run_id);
                ProviderAnswer::Done(Some(dir.display().to_string()))
            }
            ProviderCall::Mount { .. } => {
                ProviderAnswer::Unavailable("the source is not prepared here".to_owned())
            }
            ProviderCall::Release { run_id, .. } => {
                if !state.keeps {
                    state.prepared.remove(run_id);
                }
                ProviderAnswer::Done(None)
            }
            ProviderCall::Sweep { .. } => {
                if !state.keeps {
                    state.prepared.clear();
                }
                ProviderAnswer::Done(None)
            }
        }
    }
}

/// A directory with the pool's layout and a trees root, the fake provider and the launcher.
struct Proof {
    dir: Scratch,
    root: PathBuf,
    provider: FakeProvider,
    launcher: Arc<FakeLauncher>,
}

impl Proof {
    fn new(name: &str) -> Self {
        let dir = Scratch::new(&format!("native-trees-proof-{name}"));
        fs::create_dir_all(dir.path("state")).unwrap();
        fs::create_dir_all(dir.path("trees")).unwrap();
        let root = fs::canonicalize(dir.path("trees")).unwrap();
        let provider = FakeProvider::new(&root);
        Self {
            dir,
            root,
            provider,
            launcher: FakeLauncher::new(),
        }
    }

    /// Runs the verb with the pool and provider flags, then `rest`; the exit status, and what
    /// it printed on its output and its error stream.
    fn run(&self, rest: &[&str]) -> (ExitCode, String, String) {
        let mut words = vec![
            "--accounts".to_owned(),
            self.dir.path("accounts").display().to_string(),
            "--range".to_owned(),
            "600-609".to_owned(),
            "--state".to_owned(),
            self.dir.path("state").display().to_string(),
            "--provider".to_owned(),
            "tierstore=/nonexistent/provider.sock".to_owned(),
            "--trees-root".to_owned(),
            self.root.display().to_string(),
            "--ref".to_owned(),
            "cas/l/00".to_owned(),
        ];
        words.extend(rest.iter().map(|word| (*word).to_owned()));
        self.run_words(&words)
    }

    /// Runs the verb with exactly `words` after the verb.
    fn run_words(&self, words: &[String]) -> (ExitCode, String, String) {
        let settings = pool_settings(&self.dir, 1);
        let host = FakeAccounts::new();
        let pool = open_pool(&self.dir, &settings, &host);
        let launcher = self.launcher.clone();
        let runtime = move |mut config: NativeConfig| -> Result<NativeRuntime, String> {
            config.homes = settings.homes.clone();
            NativeRuntime::over(Some(pool.clone()), launcher.clone(), config, TERM)
                .map_err(|why| why.0)
        };
        let provider = self.provider.clone();
        let providers = move |agent: &str, sockets: &[(String, PathBuf, u32)]| {
            provider
                .state()
                .opened
                .push((agent.to_owned(), sockets.to_vec()));
            Ok(Box::new(provider.clone()) as Box<dyn Providers>)
        };
        let over = Over {
            runtime: &runtime,
            providers: &providers,
            pause: &std::thread::yield_now,
        };
        let args: Vec<OsString> = ["js-agent", VERB]
            .iter()
            .map(OsString::from)
            .chain(words.iter().map(OsString::from))
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = trees_proof::run_with(&args, &mut out, &mut err, &over);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// The directory the provider answers for `run`.
    fn dir_of(&self, run: &str) -> PathBuf {
        FakeProvider::dir_of(&self.root, run)
    }

    /// The calls the provider answered, as kind and run.
    fn answered(&self) -> Vec<String> {
        self.provider
            .state()
            .answered
            .iter()
            .map(|call| match call {
                ProviderCall::Prepare { run_id, .. } => format!("prepare {run_id}"),
                ProviderCall::Mount { run_id, .. } => format!("mount {run_id}"),
                ProviderCall::Release { run_id, .. } => format!("release {run_id}"),
                ProviderCall::Sweep { keep } => format!("sweep {keep:?}"),
            })
            .collect()
    }
}

/// The job of every run here: it prints its own tree's file read through its link, and the
/// two arguments the verb adds.
const JOB: [&str; 4] = [
    "--",
    "/bin/sh",
    "-c",
    "cat .kbf-tree/data.txt; echo \"other=$1 root=$2\"",
];

/// Asserts that the verb has each run's tree prepared and mounted, starts each run with its
/// own tree linked at `.kbf-tree` and the other run's tree and the trees root as its last
/// arguments, profiles each with its own tree only (`MOUNT` the root, `MT_0` its tree, no
/// `MT_1`), then releases both, sweeps and asks again, prints each placement released and exits
/// 0; the provider is asked in that order, each call answered after a pause. Catches a verb
/// that runs a job without its tree, hands a run the other's tree, never releases a
/// placement, or reports success without the provider's word.
#[test]
fn both_runs_read_their_own_tree_and_both_placements_are_released() {
    let proof = Proof::new("ok");
    let (code, out, err) = proof.run(&JOB);
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    let (a, b) = (proof.dir_of(RUNS[0]), proof.dir_of(RUNS[1]));
    let root = proof.root.display().to_string();
    for (run, own, other) in [(RUNS[0], &a, &b), (RUNS[1], &b, &a)] {
        assert!(
            out.contains(&format!("tree {run}: {}\n", own.display())),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "run {run}: stdout:\n{run}\nother={} root={root}\n",
                other.display()
            )),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "run {run}: ended Exited(Exit {{ code: 0, oom_killed: false }})\n"
            )),
            "{out}"
        );
        assert!(out.contains(&format!("run {run}: remove ok\n")), "{out}");
        assert!(
            out.contains(&format!("placement {run}: released\n")),
            "{out}"
        );
    }
    let launched = proof.launcher.launched();
    assert_eq!(launched.len(), 2);
    for (launch, own) in launched.iter().zip([&a, &b]) {
        assert_eq!(param(launch, "MOUNT"), Some(root.as_str()));
        assert_eq!(param(launch, "MT_0"), own.to_str());
        assert_eq!(param(launch, "MT_1"), None);
    }
    assert_eq!(
        proof.answered(),
        [
            "prepare trees-proof-a",
            "mount trees-proof-a",
            "prepare trees-proof-b",
            "mount trees-proof-b",
            "release trees-proof-a",
            "release trees-proof-b",
            "sweep []",
            "mount trees-proof-a",
            "mount trees-proof-b",
        ]
    );
}

/// Asserts that a provider that refuses the tree ends the verb with status 2 before any run
/// starts, naming the run and the refusal, and still releases and sweeps. Catches a verb that
/// starts a job with no tree, or leaves a placement after a failed start.
#[test]
fn a_tree_the_provider_refuses_starts_no_run_and_is_released() {
    let proof = Proof::new("refused");
    proof.provider.state().refuse_prepare = true;
    let (code, out, err) = proof.run(&JOB);
    assert_eq!(code, ExitCode::from(status::REFUSED), "{out}{err}");
    assert!(
        out.contains("tree trees-proof-a: prepare: Refused(\"no tree list of that key\")\n"),
        "{out}"
    );
    assert!(proof.launcher.launched().is_empty());
    assert_eq!(
        proof.answered()[1..4],
        ["release trees-proof-a", "release trees-proof-b", "sweep []"]
    );
}

/// Asserts that a provider that still answers a run's directory after its release and the
/// sweep makes the verb exit 1 with that placement printed held, and that a job that fails
/// makes it exit 1 too. Catches a verb that reports a placement released on the release's own
/// answer alone, or a failed job as a pass.
#[test]
fn a_placement_still_held_or_a_failed_job_fails_the_verb() {
    let held = Proof::new("held");
    held.provider.state().keeps = true;
    let (code, out, err) = held.run(&JOB);
    assert_eq!(code, ExitCode::from(status::FAILED), "{out}{err}");
    assert!(out.contains("placement trees-proof-a: held\n"), "{out}");

    let failing = Proof::new("failing");
    let (code, out, err) = failing.run(&["--", "/bin/sh", "-c", "exit 3"]);
    assert_eq!(code, ExitCode::from(status::FAILED), "{out}{err}");
    assert!(
        out.contains("run trees-proof-a: ended Exited(Exit { code: 3, oom_killed: false })\n"),
        "{out}"
    );
    assert!(out.contains("placement trees-proof-b: released\n"), "{out}");
}

/// Asserts that `--without-tree-rules` profiles each run as an agent without a trees root:
/// no `MOUNT` and no `MT_` parameter, the run's tree granted as a mount (`GR_`), and says so.
/// Catches a control that still carries the trees root's rules, so the proof's red run would
/// prove nothing.
#[test]
fn without_the_tree_rules_a_tree_is_granted_as_any_mount() {
    let proof = Proof::new("control");
    let mut words = vec!["--without-tree-rules"];
    words.extend(JOB);
    let (code, out, err) = proof.run(&words);
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    assert!(
        out.contains("profile: without the trees root's rules\n"),
        "{out}"
    );
    let launched = proof.launcher.launched();
    assert_eq!(launched.len(), 2);
    for (launch, own) in launched.iter().zip([RUNS[0], RUNS[1]]) {
        assert_eq!(param(launch, "MOUNT"), None);
        assert_eq!(param(launch, "MT_0"), None);
        let own = proof.dir_of(own);
        assert!(
            launch
                .params
                .iter()
                .any(|(name, value)| name.starts_with("GR_") && Path::new(value) == own),
            "{:?}",
            launch.params
        );
    }
}

/// Asserts that a wrong command line exits 64 with the reason on the error stream and
/// nothing asked of the provider, by exact equality: no program, an unknown flag, a flag with
/// no value, a provider or uid that does not parse, and each required flag left out. Catches
/// a verb that runs with a part of its configuration missing.
#[test]
fn a_wrong_command_line_exits_64_naming_what_is_wrong() {
    let proof = Proof::new("usage");
    let words = |list: &[&str]| {
        list.iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let state = proof.dir.path("state").display().to_string();
    let root = proof.root.display().to_string();
    let cases: Vec<(Vec<String>, &str)> = vec![
        (words(&["--state", &state]), "no program after --"),
        (
            words(&["--bogus", "--", "/usr/bin/true"]),
            "--bogus: not a flag of __native-trees-proof",
        ),
        (words(&["--state"]), "--state needs a value"),
        (
            words(&["--provider", "tierstore", "--", "/usr/bin/true"]),
            "--provider tierstore: not <name>=<socket>",
        ),
        (
            words(&["--provider-uid", "x", "--", "/usr/bin/true"]),
            "--provider-uid x: not a uid",
        ),
        (words(&["--", "/usr/bin/true"]), "--state is required"),
        (
            words(&["--state", &state, "--", "/usr/bin/true"]),
            "--provider is required",
        ),
        (
            words(&[
                "--state",
                &state,
                "--provider",
                "t=/s",
                "--",
                "/usr/bin/true",
            ]),
            "--trees-root is required",
        ),
        (
            words(&[
                "--state",
                &state,
                "--provider",
                "t=/s",
                "--trees-root",
                &root,
                "--",
                "/usr/bin/true",
            ]),
            "--ref is required",
        ),
    ];
    for (given, why) in cases {
        let (code, out, err) = proof.run_words(&given);
        assert_eq!(code, ExitCode::from(status::USAGE), "{given:?}: {out}{err}");
        assert_eq!(err, format!("js-agent: {VERB}: {why}\n"), "{given:?}");
    }
    assert!(proof.answered().is_empty());
}

/// Asserts that the providers' client is opened once with the agent id the placements are
/// made for, `trees-proof` unless `--agent-id` names another, and with the provider of
/// `--provider` and the uid of `--provider-uid` (0 unless given). Catches a verb whose
/// placements are made under an agent id other than the one it releases and sweeps, or
/// that checks the provider's socket against the wrong user.
#[test]
fn the_providers_client_gets_the_agent_id_and_the_provider_given() {
    let socket = PathBuf::from("/nonexistent/provider.sock");
    let by_default = Proof::new("agent-default");
    let (code, out, err) = by_default.run(&JOB);
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    assert_eq!(
        by_default.provider.state().opened,
        [(
            "trees-proof".to_owned(),
            vec![("tierstore".to_owned(), socket.clone(), 0)]
        )]
    );

    let named = Proof::new("agent-named");
    let mut words = vec!["--agent-id", "mac-proof-7", "--provider-uid", "263"];
    words.extend(JOB);
    let (code, out, err) = named.run(&words);
    assert_eq!(code, ExitCode::from(status::OK), "{out}{err}");
    assert_eq!(
        named.provider.state().opened,
        [(
            "mac-proof-7".to_owned(),
            vec![("tierstore".to_owned(), socket, 263)]
        )]
    );
}
