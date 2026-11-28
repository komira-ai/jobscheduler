//! The pf check of a native agent whose cap is `on`: it refuses to start, and refuses each
//! job that asks for the network, unless the anchor of `js-mac-provision` is enforced right
//! then. The check is a fake that answers as the real one does and counts its calls; the
//! runtime and the lifecycle are the real ones over the real pool on a fake host and the fake
//! launcher.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_agent::lifecycle::{Event, Runtime};
use js_agent::native::{
    Ceiling, Enforcer, NativeFlags, NativeRuntime, NativeSettings, PfFlags, ProvisionError,
};
use js_agent_wire::{JobSpec, NetworkLevel};

use super::lifecycle::{Native, job};
use super::{FakeAccounts, FakeLauncher, Rig, TERM, config, open_pool, pool_settings};
use crate::lifecycle::fake::{FakeRuntime, Log, ManualClock};
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

/// A check that answers `verdict` and counts how often it was asked.
#[derive(Debug)]
pub struct Fake {
    verdict: Mutex<Verdict>,
    calls: AtomicUsize,
}

impl Fake {
    pub fn new(verdict: Verdict) -> Arc<Self> {
        Arc::new(Self {
            verdict: Mutex::new(verdict),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn answer(&self, verdict: Verdict) {
        *self.verdict.lock().unwrap() = verdict;
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Enforcer for Fake {
    fn enforced(&self) -> Result<(), ProvisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match *self.verdict.lock().unwrap() {
            Verdict::Enforced => Ok(()),
            Verdict::PfDisabled => Err(ProvisionError::PfDisabled),
            Verdict::NotReferenced => {
                Err(ProvisionError::NotReferenced("com.apple/pool".to_owned()))
            }
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
    h.life.start_spec(&spec)
}

/// Asserts that a start that asks for the network is refused as `egress-unchecked`, with the
/// reason on the first line of the text, for each way the anchor can fail to be enforced
/// (pf off, the main ruleset not reaching the anchor, the loaded rules drifted, `pfctl`
/// exiting non-zero, `pfctl` not starting), and that nothing is made: no account is lent and
/// no supervisor started, the scratch directory is not made, the booking is intact and the result
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
        h.made_no_run();
        let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
        assert!(!scratch.join("r1").exists(), "{verdict:?}");
        assert_eq!(h.life.free().cpu_millis, 4000, "{verdict:?}");
        let result = &crate::lifecycle::results(&h.life)[0];
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

/// Asserts that a runtime whose cap is `on` and that was given no check does not open (as an
/// agent opens it, and over a pool it is given), naming the flag, and starts no supervisor;
/// and that it opens once it has the check. It catches a configuration that reaches the
/// network with nothing to limit it.
#[test]
fn a_runtime_whose_cap_is_on_does_not_open_without_a_check() {
    let dir = Scratch::new("native-pf-open");
    let settings = pool_settings(&dir, 1);
    let accounts = FakeAccounts::new();
    let pool = open_pool(&dir, &settings, &accounts);
    let launcher = FakeLauncher::new();
    let mut cfg = config(&dir, &settings);
    cfg.network = Ceiling::On;
    let opened = NativeRuntime::open(launcher.clone(), cfg.clone(), TERM);
    let Err(why) = opened else {
        panic!("no check, no runtime");
    };
    let why = why.to_string();
    assert!(why.contains("--native-network"), "{why}");
    let over = NativeRuntime::over(Some(pool.clone()), launcher.clone(), cfg.clone(), TERM);
    assert!(over.is_err());
    assert!(launcher.launched().is_empty(), "no supervisor was started");
    cfg.egress = Some(Fake::new(Verdict::Enforced));
    let over = NativeRuntime::over(Some(pool), launcher, cfg, TERM);
    assert!(over.is_ok());
}

/// The settings of a native agent with the cap `network`, from the flags the command line
/// gives.
fn settings(network: &str) -> NativeSettings {
    let flags = NativeFlags {
        accounts: Some(Path::new("/srv/js-accounts")),
        account_range: Some("600-609"),
        account_ready: None,
        homes: None,
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
    NativeSettings::from_flags(&flags, &[], Path::new("/srv/journal"))
        .unwrap()
        .unwrap()
}

/// Asserts that a native runtime whose cap is `on` asks the check once at the agent's start
/// (`Runtime::check_egress`, which the worker calls before it serves) and fails when it
/// fails, naming `--native-network` and the reason; that it passes when the check passes;
/// that a cap below `on` asks nothing; and that the flags of a cap of `on` make a
/// configuration that has the check and those of the lower caps one that has none. It
/// catches an agent that registers for network jobs with no limit in force.
#[test]
fn the_start_check_fails_while_the_anchor_is_not_enforced() {
    let cases = [
        ("on", Verdict::PfDisabled, 1, false),
        ("on", Verdict::Enforced, 1, true),
        ("loopback", Verdict::PfDisabled, 0, true),
        ("off", Verdict::PfDisabled, 0, true),
    ];
    for (n, (network, verdict, calls, passes)) in cases.into_iter().enumerate() {
        let check = Fake::new(verdict);
        let from_flags = settings(network);
        assert_eq!(
            from_flags.config().egress.is_some(),
            network == "on",
            "case {n}"
        );
        let ceiling = from_flags.config().network;
        let rig = Rig::with(&format!("native-pf-start{n}"), |config| {
            config.network = ceiling;
            config.egress = Some(check.clone());
        });
        let started = rig.rt.check_egress();
        assert_eq!(check.calls(), calls, "case {n}");
        match (started, passes) {
            (Ok(()), true) => {}
            (Err(why), false) => {
                let why = why.to_string();
                assert!(why.contains("--native-network"), "{why}");
                assert!(why.contains("pf is not enabled"), "{why}");
            }
            (other, _) => panic!("case {n}: {other:?}"),
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
    let settings = pool_settings(&dir, 1);
    let accounts = FakeAccounts::new();
    let pool = open_pool(&dir, &settings, &accounts);
    let launcher = FakeLauncher::new();
    let check = Fake::new(Verdict::PfDisabled);
    let asks = job_at("r1", NetworkLevel::On).native.unwrap();
    let mut cfg = config(&dir, &settings);
    cfg.egress = Some(check.clone());
    let below =
        NativeRuntime::over(Some(pool.clone()), launcher.clone(), cfg.clone(), TERM).unwrap();
    assert!(below.check_egress().is_ok());
    assert!(below.check_native_egress(&asks).is_ok());
    assert_eq!(check.calls(), 0);
    cfg.network = Ceiling::On;
    let on = NativeRuntime::over(Some(pool), launcher, cfg, TERM).unwrap();
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
    h.made_no_run();
    let scratch = fs::canonicalize(h.dir.path("scratch")).unwrap();
    assert!(!scratch.join("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert_eq!(h.start(job_at("r2", NetworkLevel::Off)), ["started r2"]);

    let dir = Scratch::new("native-pf-nocheck-rt");
    let settings = pool_settings(&dir, 1);
    let accounts = FakeAccounts::new();
    let pool = open_pool(&dir, &settings, &accounts);
    let mut cfg = config(&dir, &settings);
    cfg.network = Ceiling::On;
    let runtime = NativeRuntime::open_ungated(Some(pool), FakeLauncher::new(), cfg, TERM);
    assert!(runtime.unwrap().check_egress().is_err());
}
