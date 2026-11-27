//! Mount providers that live in memory: they answer each call as a test scripts it, and
//! write every call they are sent in the shared [`Log`], in order with the fake runtime's
//! own lines (`create`, `resolve`, `discard`), so a test can say what came before what.
//!
//! The fake keeps the contract of [`Providers::ask`]: a call is sent (and logged) the first
//! time it is asked, a `Pending` answer keeps it out (asking it again sends nothing), and any
//! other answer is handed over once, after which asking it sends the call again.
//!
//! A test may have the providers keep the sources they took, as a real one does in memory
//! ([`FakeProviders::keep_sources`]), and then restart them ([`FakeProviders::restart`]):
//! a `Mount` of a source taken by no `Prepare` since is then answered as tierstore answers it.
//!
//! The fake does not tell the attempts of a run apart: a call is scripted, and logged, by its
//! kind, run and index, as a provider that does not read attempts sees it. The attempt of
//! every call sent is kept beside the log ([`FakeProviders::attempts`]).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use js_agent::lifecycle::{Config, Lifecycle, MountRoots, ProviderAnswer, ProviderCall, Providers};
use js_agent_wire::{
    HeldState, JobMount, JobStartSpec, Layer, MountMode, MountSource, Propagation,
};

use super::fake::{FakeRuntime, Log, ManualClock};
use super::per_job::{job_start, listing, start};
use super::{Harness, config_in, hook};
use crate::support::Scratch;

/// What tells one call apart for one provider: the provider, then the call's key without its
/// attempt.
type Key = (String, u8, String, u32);

/// What a call is answered.
#[derive(Default)]
struct Script {
    /// The answers to give, one for each ask, first.
    then: VecDeque<ProviderAnswer>,
    /// The answer once those are given; the call's default when none.
    always: Option<ProviderAnswer>,
    /// The call is out: its last answer was `Pending`.
    out: bool,
}

struct State {
    names: Vec<String>,
    scripts: BTreeMap<Key, Script>,
    /// Where the agent's provider records are: a `Prepare` notes whether its run's is there.
    records: Option<PathBuf>,
    /// The sources the providers hold, by provider, run and index, when the test has them kept
    /// ([`FakeProviders::keep_sources`]): a `Prepare` that succeeds adds its source.
    held: Option<BTreeSet<(String, String, u32)>>,
    /// Each call sent, as its log line, with its attempt (a sweep: each kept run's).
    attempts: Vec<(String, Vec<u64>)>,
}

/// The providers of a test: the names the agent was given, and what each call is answered.
#[derive(Clone)]
pub struct FakeProviders {
    state: Rc<RefCell<State>>,
    log: Log,
}

/// `Prepare` of the source `index` of `run_id`.
pub fn prepare(run_id: &str, index: u32, reference: &str, mode: MountMode) -> ProviderCall {
    ProviderCall::Prepare {
        run_id: run_id.to_owned(),
        attempt: 0,
        index,
        reference: reference.to_owned(),
        mode,
    }
}

/// `Mount` of the source `index` of `run_id`.
pub fn mount(run_id: &str, index: u32) -> ProviderCall {
    ProviderCall::Mount {
        run_id: run_id.to_owned(),
        attempt: 0,
        index,
    }
}

/// `Release` of the source `index` of `run_id`.
pub fn release(run_id: &str, index: u32) -> ProviderCall {
    ProviderCall::Release {
        run_id: run_id.to_owned(),
        attempt: 0,
        index,
    }
}

/// `Sweep`.
pub fn sweep() -> ProviderCall {
    ProviderCall::Sweep { keep: Vec::new() }
}

/// The key of `call` for `provider`: the call's own without the attempt, so a sweep is one
/// key whatever it keeps.
fn key_of(provider: &str, call: &ProviderCall) -> Key {
    let (kind, run, _, index) = call.key();
    (provider.to_owned(), kind, run.to_owned(), index)
}

/// The attempts `call` names: its own, or each kept run's of a sweep.
fn attempts_of(call: &ProviderCall) -> Vec<u64> {
    match call {
        ProviderCall::Prepare { attempt, .. }
        | ProviderCall::Mount { attempt, .. }
        | ProviderCall::Release { attempt, .. } => vec![*attempt],
        ProviderCall::Sweep { keep } => keep.iter().map(|(_, attempt)| *attempt).collect(),
    }
}

/// What a call is answered when nothing is scripted: it succeeds, but for `Mount`, which has
/// no directory to give.
fn default_answer(call: &ProviderCall) -> ProviderAnswer {
    match call {
        ProviderCall::Mount { .. } => ProviderAnswer::Unavailable("nothing is scripted".to_owned()),
        _ => ProviderAnswer::Done(None),
    }
}

impl FakeProviders {
    /// Providers named `names`, writing their calls in `log`.
    pub fn new(names: &[&str], log: &Log) -> Self {
        let state = State {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            scripts: BTreeMap::new(),
            records: None,
            held: None,
            attempts: Vec::new(),
        };
        Self {
            state: Rc::new(RefCell::new(state)),
            log: Rc::clone(log),
        }
    }

    /// A `Prepare` the providers are sent notes in the log whether the record of its run is
    /// in `dir` (the agent's `providers` directory) at that moment.
    pub fn watch_records(&self, dir: PathBuf) {
        self.state.borrow_mut().records = Some(dir);
    }

    /// The providers keep the sources they take from now on, as a provider does in memory: a
    /// `Prepare` that succeeds holds its source, and a `Mount` of a source not held is answered
    /// [`not_prepared`], whatever is scripted for it, which stays for a later ask.
    pub fn keep_sources(&self) {
        self.state.borrow_mut().held = Some(BTreeSet::new());
    }

    /// The providers restart, as a release of the provider restarts it: every source they
    /// held is lost (with [`FakeProviders::keep_sources`]).
    pub fn restart(&self) {
        if let Some(held) = self.state.borrow_mut().held.as_mut() {
            held.clear();
        }
    }

    /// The next ask of `call` of `provider` is answered `answer` (after those scripted
    /// before).
    pub fn then(&self, provider: &str, call: &ProviderCall, answer: ProviderAnswer) {
        let mut state = self.state.borrow_mut();
        let script = state.scripts.entry(key_of(provider, call)).or_default();
        script.then.push_back(answer);
    }

    /// `call` of `provider` is answered `answer` every time, once what was scripted with
    /// [`FakeProviders::then`] is given.
    pub fn always(&self, provider: &str, call: &ProviderCall, answer: ProviderAnswer) {
        let mut state = self.state.borrow_mut();
        let script = state.scripts.entry(key_of(provider, call)).or_default();
        script.always = Some(answer);
    }

    /// `Mount` of the source `index` of `run_id` answers the directory `path`.
    pub fn mounts_at(&self, provider: &str, run_id: &str, index: u32, path: &str) {
        let answer = ProviderAnswer::Done(Some(path.to_owned()));
        self.always(provider, &mount(run_id, index), answer);
    }

    /// The calls sent whose log line begins `provider <kind> `, in order.
    pub fn sent(&self, kind: &str) -> Vec<String> {
        let prefix = format!("provider {kind} ");
        let lines = self.log.borrow();
        lines
            .iter()
            .filter(|line| line.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Each call sent whose log line begins `provider <kind> `, in order, with the attempts it
    /// named: its own, or a sweep's of each kept run.
    pub fn attempts(&self, kind: &str) -> Vec<(String, Vec<u64>)> {
        let prefix = format!("provider {kind} ");
        let state = self.state.borrow();
        let sent = state.attempts.iter();
        sent.filter(|(line, _)| line.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// What the log says of a call that was sent.
    fn line(&self, provider: &str, call: &ProviderCall) -> String {
        match call {
            ProviderCall::Prepare {
                run_id,
                index,
                reference,
                mode,
                ..
            } => {
                let note = match &self.state.borrow().records {
                    Some(dir) => match dir.join(format!("{run_id}.json")).is_file() {
                        true => " record=yes",
                        false => " record=no",
                    },
                    None => "",
                };
                format!("provider prepare {provider} {run_id}#{index} {reference} {mode:?}{note}")
            }
            ProviderCall::Mount { run_id, index, .. } => {
                format!("provider mount {provider} {run_id}#{index}")
            }
            ProviderCall::Release { run_id, index, .. } => {
                format!("provider release {provider} {run_id}#{index}")
            }
            ProviderCall::Sweep { keep } => {
                let runs: Vec<&str> = keep.iter().map(|(run_id, _)| run_id.as_str()).collect();
                format!("provider sweep {provider} keep={runs:?}")
            }
        }
    }
}

impl Providers for FakeProviders {
    fn has(&self, name: &str) -> bool {
        self.state.borrow().names.iter().any(|have| have == name)
    }

    fn names(&self) -> Vec<String> {
        self.state.borrow().names.clone()
    }

    fn ask(&mut self, provider: &str, call: &ProviderCall) -> ProviderAnswer {
        if !self.has(provider) {
            return ProviderAnswer::Unavailable(format!("no provider {provider}"));
        }
        let sent = !self
            .state
            .borrow()
            .scripts
            .get(&key_of(provider, call))
            .is_some_and(|script| script.out);
        if sent {
            let line = self.line(provider, call);
            self.log.borrow_mut().push(line.clone());
            let attempts = attempts_of(call);
            self.state.borrow_mut().attempts.push((line, attempts));
        }
        let mut state = self.state.borrow_mut();
        let (_, _, run, index) = key_of(provider, call);
        let source = (provider.to_owned(), run, index);
        let lost = matches!(call, ProviderCall::Mount { .. })
            && state
                .held
                .as_ref()
                .is_some_and(|held| !held.contains(&source));
        let script = state.scripts.entry(key_of(provider, call)).or_default();
        let answer = match lost {
            true => not_prepared(),
            false => script
                .then
                .pop_front()
                .or_else(|| script.always.clone())
                .unwrap_or_else(|| default_answer(call)),
        };
        script.out = answer == ProviderAnswer::Pending;
        let prepared = matches!(call, ProviderCall::Prepare { .. })
            && matches!(answer, ProviderAnswer::Done(_));
        if let Some(held) = state.held.as_mut().filter(|_| prepared) {
            held.insert(source);
        }
        answer
    }
}

/// What tierstore answers a `Mount` of a source it does not hold (never prepared, or lost
/// when it restarted): `UNAVAILABLE`, which the agent's client hands over as unavailable.
pub fn not_prepared() -> ProviderAnswer {
    ProviderAnswer::Unavailable("the source is not prepared here".to_owned())
}

/// The image of the job starts of these tests.
pub const IMAGE: &str = "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The image of an image layer.
pub const LAYER: &str = "registry.example/layer:1@sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";

/// How long a start of these tests may wait on its providers, from its admission.
pub const WAIT_MS: u64 = 30_000;

/// A harness whose agent has the roots `data` (read-write) and `ro` (read-only), whose
/// runtime gives layers, and which was given no provider yet.
pub fn bare(name: &str) -> Harness {
    let dir = Scratch::new(&format!("life-{name}"));
    let mut config: Config = config_in(&dir, false);
    fs::create_dir_all(dir.path("ro")).unwrap();
    config.mounts = MountRoots::with_read_only(
        &[("data".to_owned(), dir.path("data"))],
        &[("ro".to_owned(), dir.path("ro"))],
    )
    .unwrap();
    let (clock, log) = (ManualClock::default(), Log::default());
    let fake = FakeRuntime::new(&log, &clock);
    fake.give_layers();
    fake.keep_lease_dirs_in(&dir.path("scratch"));
    let checks = hook(&log, &fake, &dir.path("scratch"), &[]);
    let (life, _) = Lifecycle::adopt(
        config.clone(),
        Box::new(fake.clone()),
        checks,
        Box::new(clock.clone()),
    )
    .unwrap();
    Harness {
        life,
        fake,
        clock,
        log,
        config,
        dir,
    }
}

/// [`bare`], with the provider `tier` given to the lifecycle (`script` first says how it
/// answers).
pub fn rig(name: &str, script: impl FnOnce(&FakeProviders)) -> (Harness, FakeProviders) {
    let mut h = bare(name);
    let providers = FakeProviders::new(&["tier"], &h.log);
    script(&providers);
    h.life
        .attach_providers(Box::new(providers.clone()), WAIT_MS);
    (h, providers)
}

/// Where in the root `root` (`data` or `ro`) of the harness the tree `rel` is.
pub fn tree(h: &Harness, root: &str, rel: &str) -> String {
    format!("{}/{rel}", h.dir.path(root).display())
}

/// A start of `job_id` of a tenth of a core, so that many fit the harness's four.
fn small(job_id: &str) -> JobStartSpec {
    let mut small = start(job_id);
    small.booked.cpu_millis = 100;
    small
}

/// A source `reference` of the provider `tier`, as a mount at `target`.
pub fn provided_mount(reference: &str, target: &str, mode: MountMode) -> JobMount {
    JobMount {
        source: MountSource::Provider {
            provider: "tier".to_owned(),
            reference: reference.to_owned(),
        },
        target: target.to_owned(),
        mode,
        sub_path: None,
        no_exec: false,
        propagation: Propagation::Private,
    }
}

/// A start of `job_id` whose one layer is `k1` of the provider `tier`: its source 0.
pub fn with_layer(job_id: &str) -> JobStartSpec {
    JobStartSpec {
        layers: vec![Layer::Provider {
            provider: "tier".to_owned(),
            reference: "k1".to_owned(),
        }],
        ..small(job_id)
    }
}

/// A start of `job_id` whose layer is `k1` of the provider `tier` (source 0) and which mounts
/// `k2` of it read-only at `/opt/sdk` (source 1).
pub fn with_layer_and_mount(job_id: &str) -> JobStartSpec {
    JobStartSpec {
        mounts: vec![provided_mount("k2", "/opt/sdk", MountMode::ReadOnly)],
        ..with_layer(job_id)
    }
}

/// A start of `job_id` whose one mount, at `/opt/sdk` in `mode`, is `k2` of the provider
/// `tier`: its source 0.
pub fn with_mount(job_id: &str, mode: MountMode) -> JobStartSpec {
    JobStartSpec {
        mounts: vec![provided_mount("k2", "/opt/sdk", mode)],
        ..small(job_id)
    }
}

/// Starts `job_id` as [`with_layer`] has it, the provider answering the directory `path`
/// for its source, and steps until the job runs (a bounded number of times).
pub fn run_one(h: &mut Harness, providers: &FakeProviders, job_id: &str) {
    let path = tree(h, "ro", &format!("{job_id}-0"));
    providers.mounts_at("tier", job_id, 0, &path);
    h.apply(vec![job_start(with_layer(job_id))]);
    for _ in 0..4 {
        h.life.step();
    }
    assert_eq!(listing(h, job_id).state, HeldState::Running);
}
