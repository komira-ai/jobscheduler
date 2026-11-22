//! The pf check of a native agent whose cap is `on`: it refuses to start, and refuses each
//! job that asks for the network, unless the anchor of `js-mac-provision` is enforced right
//! then. The check is a fake that answers as the real one does and counts its calls; the
//! runtime, the lifecycle and the worker are the real ones over the fake helper.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use js_agent::lifecycle::{Event, Runtime};
use js_agent::native::{
    Applied, Ceiling, Enforcer, NativeFlags, NativeRuntime, NativeSettings, PfFlags,
    ProvisionError, Ticks,
};
use js_agent::{Backend, Worker};
use js_agent_wire::{Command, JobSpec, NetworkLevel};

use super::fake_helper::FakeHelper;
use super::lifecycle::{Native, job};
use super::{TERM, config};
use crate::jobs::fake::FakeMachine;
use crate::jobs::{STEP_NEVER, plan};
use crate::lifecycle::fake::{FakeRuntime, Log, ManualClock};
use crate::lifecycle::reply;
use crate::support::Scratch;

/// How a pf anchor can fail to be enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Enforced,
    PfDisabled,
    NotReferenced,
    Drift,
    PfctlExited,
    PfctlMissing,
}

/// A check that answers `verdict`, counts how often it was asked, and keeps an ordered log of
/// its calls (`"apply"`, `"enforced"`). Its `apply` answers from a script (`InForce` when the
/// script is empty); its clock has `ticks` ticks, then stops.
#[derive(Debug)]
pub struct Fake {
    verdict: Mutex<Verdict>,
    calls: AtomicUsize,
    applies: Mutex<VecDeque<Result<Applied, Verdict>>>,
    log: Mutex<Vec<&'static str>>,
    ticks: AtomicUsize,
}

/// The clock `Fake::ticks` gives: `left` ticks, then a stop.
struct Counted {
    left: usize,
}

impl Ticks for Counted {
    fn wait(&mut self, _stop: &Receiver<()>) -> bool {
        let tick = self.left > 0;
        self.left = self.left.saturating_sub(1);
        tick
    }
}

impl Fake {
    pub fn new(verdict: Verdict) -> Arc<Self> {
        Arc::new(Self {
            verdict: Mutex::new(verdict),
            calls: AtomicUsize::new(0),
            applies: Mutex::new(VecDeque::new()),
            log: Mutex::new(Vec::new()),
            ticks: AtomicUsize::new(0),
        })
    }

    pub fn answer(&self, verdict: Verdict) {
        *self.verdict.lock().unwrap() = verdict;
    }

    /// The answers of the next applies, in order.
    pub fn script_applies(&self, answers: Vec<Result<Applied, Verdict>>) {
        *self.applies.lock().unwrap() = answers.into();
    }

    /// How many ticks the keeper's clock gives.
    pub fn give_ticks(&self, n: usize) {
        self.ticks.store(n, Ordering::SeqCst);
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The calls of `apply` and `enforced`, in the order they were made.
    pub fn log(&self) -> Vec<&'static str> {
        self.log.lock().unwrap().clone()
    }

    /// How many times `apply` was called.
    pub fn applies(&self) -> usize {
        self.log().iter().filter(|call| **call == "apply").count()
    }
}

fn fault(verdict: Verdict) -> Result<(), ProvisionError> {
    match verdict {
        Verdict::Enforced => Ok(()),
        Verdict::PfDisabled => Err(ProvisionError::PfDisabled),
        Verdict::NotReferenced => Err(ProvisionError::NotReferenced("com.apple/pool".to_owned())),
        Verdict::Drift => Err(ProvisionError::Drift {
            expected: "block".to_owned(),
            loaded: String::new(),
        }),
        Verdict::PfctlExited => Err(ProvisionError::Failed {
            args: "-s info".to_owned(),
            code: "1".to_owned(),
            stderr: "Permission denied".to_owned(),
        }),
        Verdict::PfctlMissing => Err(ProvisionError::Run {
            args: "-s info".to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, "no pfctl"),
        }),
    }
}

impl Enforcer for Fake {
    fn enforced(&self) -> Result<(), ProvisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push("enforced");
        fault(*self.verdict.lock().unwrap())
    }

    fn apply(&self) -> Result<Applied, ProvisionError> {
        self.log.lock().unwrap().push("apply");
        match self.applies.lock().unwrap().pop_front() {
            None => Ok(Applied::InForce),
            Some(Ok(applied)) => Ok(applied),
            Some(Err(verdict)) => fault(verdict).map(|()| Applied::InForce),
        }
    }

    fn ticks(&self) -> Box<dyn Ticks> {
        Box::new(Counted {
            left: self.ticks.load(Ordering::SeqCst),
        })
    }
}

/// A native lifecycle with the network ceiling `net`, checked by `check`.
pub(super) fn gated(name: &str, net: Ceiling, check: &Arc<Fake>) -> Native {
    let check: Arc<dyn Enforcer> = check.clone();
    Native::built(name, None, move |config| {
        config.network = net;
        config.egress = Some(check);
    })
}

/// A start of `run_id` with the network at `level`.
pub(super) fn job_at(run_id: &str, level: NetworkLevel) -> JobSpec {
    let mut spec = job(run_id, "/bin/echo");
    spec.native.as_mut().unwrap().network = level;
    spec
}

/// The events of one start.
fn start(h: &mut Native, spec: JobSpec) -> Vec<Event> {
    h.life.apply(&reply(vec![Command::Start(Box::new(spec))]))
}

/// Asserts that a start that asks for the network is refused as `egress-unchecked`, with the
/// reason on the first line of the text, for each way the anchor can fail to be enforced
/// (pf off, the main ruleset not reaching the anchor, the loaded rules drifted, `pfctl`
/// exiting non-zero, `pfctl` not starting), and that nothing is made: the helper hears only
/// of its boot reset, the scratch directory is not made, the booking is intact and the result
/// is a start that failed. It catches a Mac job that reaches the network with no limit in
/// force, and a refusal that still costs a lease user.
#[test]
fn a_network_job_is_refused_for_each_way_the_anchor_is_not_enforced() {
    let table = [
        (Verdict::PfDisabled, "pf is not enabled"),
        (Verdict::NotReferenced, "main ruleset"),
        (Verdict::Drift, "differ from the rendered"),
        (Verdict::PfctlExited, "exited with 1"),
        (Verdict::PfctlMissing, "could not run pfctl"),
    ];
    for (n, (verdict, said)) in table.into_iter().enumerate() {
        let check = Fake::new(verdict);
        let mut h = gated(&format!("pf-refuse{n}"), Ceiling::On, &check);
        let events = start(&mut h, job_at("r1", NetworkLevel::On));
        let refused: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                Event::Refused { code, why, .. } => Some((*code, why.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(refused.len(), 1, "{verdict:?}: {events:?}");
        assert_eq!(refused[0].0, "egress-unchecked", "{verdict:?}");
        assert!(refused[0].1.contains(said), "{verdict:?}: {}", refused[0].1);
        assert!(!refused[0].1.contains('\n'), "{verdict:?}: one line");
        assert_eq!(check.calls(), 1, "{verdict:?}");
        assert_eq!(h.fake.verbs(), ["boot-reset"], "{verdict:?}");
        let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
        assert!(!scratch.join("r1").exists(), "{verdict:?}");
        assert_eq!(h.life.free().cpu_millis, 4000, "{verdict:?}");
        let result = &h.life.results()[0];
        assert_eq!(result.detail.as_deref(), Some("egress-unchecked"));
    }
}

/// Asserts that a job that asks for no network, or loopback only, starts although the anchor
/// is not enforced, and that the check is not asked for it. It catches an agent that stops
/// serving the jobs the limit never concerned, and a check paid for by every job.
#[test]
fn jobs_without_the_network_are_not_checked() {
    let check = Fake::new(Verdict::PfDisabled);
    let mut h = gated("pf-off", Ceiling::On, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::Off)), ["started r1"]);
    assert_eq!(
        h.start(job_at("r2", NetworkLevel::Loopback)),
        ["started r2"]
    );
    assert_eq!(check.calls(), 0);
}

/// Asserts that the check runs before every job that asks for the network and its answer is
/// never kept: a job passes, a second passes, the main ruleset is then reloaded without the
/// anchor and the third is refused, the anchor comes back and the fourth starts; four jobs,
/// four calls. It catches a check remembered from the agent's start or from the last job,
/// which leaves the window after a reload open until the next tick of the daemon.
#[test]
fn the_check_runs_before_every_network_job_and_is_not_kept() {
    let check = Fake::new(Verdict::Enforced);
    let mut h = gated("pf-every", Ceiling::On, &check);
    assert_eq!(h.start(job_at("r1", NetworkLevel::On)), ["started r1"]);
    assert_eq!(h.start(job_at("r2", NetworkLevel::On)), ["started r2"]);
    assert_eq!(check.calls(), 2);
    check.answer(Verdict::NotReferenced);
    assert_eq!(
        h.start(job_at("r3", NetworkLevel::On)),
        ["refused r3 egress-unchecked"]
    );
    assert_eq!(check.calls(), 3);
    check.answer(Verdict::Enforced);
    assert_eq!(h.start(job_at("r4", NetworkLevel::On)), ["started r4"]);
    assert_eq!(check.calls(), 4);
}

/// Asserts that an agent whose cap is below `on` never asks the check, whatever it was
/// given: a loopback job starts with the anchor not enforced and the check is not called. It
/// catches a gate that stops agents that were never allowed the network.
#[test]
fn an_agent_whose_cap_is_not_on_never_asks() {
    let check = Fake::new(Verdict::PfDisabled);
    let mut h = gated("pf-cap", Ceiling::Loopback, &check);
    assert_eq!(
        h.start(job_at("r1", NetworkLevel::Loopback)),
        ["started r1"]
    );
    assert_eq!(check.calls(), 0);
}

/// Asserts that a runtime whose cap is `on` and that was given no check does not open, naming
/// the flag, and sends the helper nothing. It catches a configuration that reaches the
/// network with nothing to limit it.
#[test]
fn a_runtime_whose_cap_is_on_does_not_open_without_a_check() {
    let dir = Scratch::new("native-pf-open");
    let fake = FakeHelper::start(&dir);
    let mut cfg = config(&fake);
    cfg.network = Ceiling::On;
    let Err(why) = NativeRuntime::open(fake.client(), cfg.clone(), TERM) else {
        panic!("no check, no runtime");
    };
    let why = why.to_string();
    assert!(why.contains("--native-network"), "{why}");
    assert!(fake.verbs().is_empty(), "the helper was not asked");
    cfg.egress = Some(Fake::new(Verdict::Enforced));
    assert!(NativeRuntime::open(fake.client(), cfg, TERM).is_ok());
}

/// The settings of a native agent over `fake` with the cap `network`, checked by `check`.
fn settings(fake: &FakeHelper, network: &str, check: &Arc<Fake>) -> NativeSettings {
    let flags = NativeFlags {
        socket: Some(fake.socket()),
        homes: Some(fake.homes()),
        network: Some(network),
        tool_roots: &[],
        deny_read: &[],
        max_file_mib: None,
        max_processes: None,
        pf: PfFlags {
            uid_start: (network == "on").then_some(600),
            uid_count: (network == "on").then_some(10),
            ..PfFlags::default()
        },
    };
    let settings = NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap();
    settings.with_enforcer(check.clone())
}

/// Asserts that a worker over a native backend whose cap is `on` asks the check once at start
/// and does not start when it fails, naming `--native-network` and the reason; that it starts
/// when the check passes; and that a cap below `on` asks nothing. It catches an agent that
/// registers for network jobs with no limit in force.
#[test]
fn the_worker_does_not_start_while_the_anchor_is_not_enforced() {
    let dir = Scratch::new("native-pf-start");
    let fake = FakeHelper::start(&dir);
    let machine = FakeMachine::new();
    let cases = [
        ("on", Verdict::PfDisabled, 1, false),
        ("on", Verdict::Enforced, 1, true),
        ("loopback", Verdict::PfDisabled, 0, true),
        ("off", Verdict::PfDisabled, 0, true),
    ];
    for (n, (network, verdict, calls, starts)) in cases.into_iter().enumerate() {
        let check = Fake::new(verdict);
        let roots = Scratch::new(&format!("native-pf-start{n}"));
        let mut plan = plan(&roots, &machine, STEP_NEVER);
        let settings = settings(&fake, network, &check);
        plan.backend = Backend::Native(settings);
        let started = Worker::start(plan);
        assert_eq!(check.calls(), calls, "case {n}");
        match (started, starts) {
            (Ok(_), true) => {}
            (Err(why), false) => {
                assert!(why.starts_with("--native-network: "), "{why}");
                assert!(why.contains("pf is not enabled"), "{why}");
            }
            (other, _) => panic!("case {n}: {:?}", other.err()),
        }
    }
}

/// Asserts that a runtime whose cap is below `on` passes both of its checks without asking,
/// even for a job that asks for the network and with a check that would fail; and that with
/// a cap of `on` the same two checks ask. It catches a check that is made where the limit is
/// not the agent's to keep.
#[test]
fn the_runtime_asks_only_when_its_cap_is_on() {
    let dir = Scratch::new("native-pf-cap");
    let fake = FakeHelper::start(&dir);
    let check = Fake::new(Verdict::PfDisabled);
    let asks = job_at("r1", NetworkLevel::On).native.unwrap();
    let mut cfg = config(&fake);
    cfg.egress = Some(check.clone());
    let below = NativeRuntime::open(fake.client(), cfg.clone(), TERM).unwrap();
    assert!(below.check_egress().is_ok());
    assert!(below.check_native_egress(&asks).is_ok());
    assert_eq!(check.calls(), 0);
    cfg.network = Ceiling::On;
    let on = NativeRuntime::open(fake.client(), cfg, TERM).unwrap();
    assert!(on.check_egress().is_err());
    assert!(on.check_native_egress(&asks).is_err());
    assert_eq!(check.calls(), 2);
}

/// Asserts that a runtime with nothing to check refuses a job that asks for the network and
/// passes one that asks for none. It catches a default that lets a runtime that never learned
/// of the check run a job with the network on.
#[test]
fn a_runtime_with_nothing_to_check_refuses_a_network_job() {
    let runtime = FakeRuntime::new(&Log::default(), &ManualClock::default());
    let asks = job_at("r1", NetworkLevel::On).native.unwrap();
    assert!(runtime.check_native_egress(&asks).is_err());
    let none = job_at("r2", NetworkLevel::Off).native.unwrap();
    assert!(runtime.check_native_egress(&none).is_ok());
}

/// Asserts that the gate refuses by itself when it is missing its check: a runtime with a cap
/// of `on` that `open` would have refused (opened here without that refusal) refuses the
/// start-up check and a job that asks for the network as `egress-unchecked`, creating
/// nothing, and still starts a job that asks for none. It catches a gate that is safe only
/// because `open` stands in front of it.
#[test]
fn a_gate_with_no_check_refuses_a_network_job_by_itself() {
    let mut h = Native::ungated("pf-nocheck", |config| config.network = Ceiling::On);
    assert_eq!(
        h.start(job_at("r1", NetworkLevel::On)),
        ["refused r1 egress-unchecked"]
    );
    assert_eq!(h.fake.verbs(), ["boot-reset"]);
    let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
    assert!(!scratch.join("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert_eq!(h.start(job_at("r2", NetworkLevel::Off)), ["started r2"]);

    let dir = Scratch::new("native-pf-nocheck-rt");
    let fake = FakeHelper::start(&dir);
    let mut cfg = config(&fake);
    cfg.network = Ceiling::On;
    let runtime = NativeRuntime::open_ungated(fake.client(), cfg, TERM).unwrap();
    assert!(runtime.check_egress().is_err());
}

/// Asserts that a worker over a native backend whose cap is `on` loads the anchor before it
/// checks it (the log is `apply`, `enforced`), and that a cap below `on` does neither. It
/// catches an agent that checks an anchor it has not loaded, which stops at every start
/// until something else loads it, and one that loads an anchor it does not keep.
#[test]
fn the_agent_loads_the_anchor_before_it_checks_it() {
    let dir = Scratch::new("native-pf-load");
    let fake = FakeHelper::start(&dir);
    let machine = FakeMachine::new();
    let cases = [
        ("on", vec!["apply", "enforced"]),
        ("loopback", vec![]),
        ("off", vec![]),
    ];
    for (n, (network, log)) in cases.into_iter().enumerate() {
        let check = Fake::new(Verdict::Enforced);
        let roots = Scratch::new(&format!("native-pf-load{n}"));
        let mut plan = plan(&roots, &machine, STEP_NEVER);
        plan.backend = Backend::Native(settings(&fake, network, &check));
        let started = Worker::start(plan);
        assert!(started.is_ok(), "{network}");
        drop(started);
        assert_eq!(check.log(), log, "{network}");
    }
}

/// Asserts that a first load the system refuses (the main ruleset does not reach the anchor
/// yet) is made once more on the next tick, and the worker then starts: the log is `apply`,
/// `apply`, `enforced`; and that with no tick the refused load is not repeated and the
/// check decides (here it fails, so the worker does not start). It catches an agent that
/// stops at every boot that is faster than the system's own pf start, and one that retries
/// without waiting.
#[test]
fn a_first_refusal_is_retried_on_the_next_tick() {
    let dir = Scratch::new("native-pf-retry");
    let fake = FakeHelper::start(&dir);
    let machine = FakeMachine::new();
    let cases = [
        (
            1,
            Verdict::Enforced,
            vec!["apply", "apply", "enforced"],
            true,
        ),
        (0, Verdict::NotReferenced, vec!["apply", "enforced"], false),
    ];
    for (n, (ticks, verdict, log, starts)) in cases.into_iter().enumerate() {
        let check = Fake::new(verdict);
        check.script_applies(vec![Err(Verdict::NotReferenced), Ok(Applied::Installed)]);
        check.give_ticks(ticks);
        let roots = Scratch::new(&format!("native-pf-retry{n}"));
        let mut plan = plan(&roots, &machine, STEP_NEVER);
        plan.backend = Backend::Native(settings(&fake, "on", &check));
        let started = Worker::start(plan);
        assert_eq!(started.is_ok(), starts, "case {n}");
        drop(started);
        assert_eq!(check.log(), log, "case {n}");
    }
}

/// Asserts that, once the agent's start check ran, the anchor is loaded again at every tick
/// until the runtime is dropped (the loads are one for the start plus one for each tick), and
/// that a tick whose load fails is followed by another load. It catches a keeper that loads
/// once, stops at its first failure, or outlives the runtime.
#[test]
fn the_anchor_is_loaded_again_at_every_tick_until_the_runtime_ends() {
    let dir = Scratch::new("native-pf-keep");
    let fake = FakeHelper::start(&dir);
    let check = Fake::new(Verdict::Enforced);
    check.script_applies(vec![
        Ok(Applied::InForce),
        Err(Verdict::PfctlExited),
        Ok(Applied::Installed),
    ]);
    check.give_ticks(3);
    let mut cfg = config(&fake);
    cfg.network = Ceiling::On;
    cfg.egress = Some(check.clone());
    let runtime = NativeRuntime::open(fake.client(), cfg, TERM).unwrap();
    assert!(runtime.check_egress().is_ok());
    drop(runtime);
    assert_eq!(check.applies(), 4);
    assert_eq!(check.calls(), 1);
}

/// Asserts that the checks of a network job never load the anchor: with the anchor not
/// enforced, jobs refused at admission leave the loads at the start's count, on a runtime
/// and on a lifecycle. It catches an admission that repairs the limit it is to enforce,
/// which would let a job in on the strength of its own load.
#[test]
fn a_network_job_never_loads_the_anchor() {
    let dir = Scratch::new("native-pf-admit");
    let fake = FakeHelper::start(&dir);
    let check = Fake::new(Verdict::Enforced);
    let asks = job_at("r1", NetworkLevel::On).native.unwrap();
    let mut cfg = config(&fake);
    cfg.network = Ceiling::On;
    cfg.egress = Some(check.clone());
    let runtime = NativeRuntime::open(fake.client(), cfg, TERM).unwrap();
    assert!(runtime.check_egress().is_ok());
    check.answer(Verdict::NotReferenced);
    assert!(runtime.check_native_egress(&asks).is_err());
    assert!(runtime.check_native_egress(&asks).is_err());
    drop(runtime);
    assert_eq!(check.applies(), 1);

    let refused = Fake::new(Verdict::NotReferenced);
    let mut h = gated("pf-admit", Ceiling::On, &refused);
    assert_eq!(
        h.start(job_at("r2", NetworkLevel::On)),
        ["refused r2 egress-unchecked"]
    );
    assert_eq!(refused.applies(), 0);
}
