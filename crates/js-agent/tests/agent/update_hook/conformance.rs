//! `js-agent update-hook` against the conformance harness of the host updater's hook contract
//! (`update_hook::conformance`, kbf's `update-hook` crate at the revision the workspace pins).
//!
//! The harness starts `js-update-hook-fake` (the hook's own steps over the fake ports,
//! tests/bin/update_hook_fake.rs) the way the updater starts a hook: a process of its own, an
//! empty environment but what [`AgentHost::env`] names, one params file for the update. It
//! runs every step twice, kills the hook at every kill point and runs it again, drives the
//! updater's next-step table through a failure, a deferral and a halt at each step, and holds
//! the record lock while every step runs; it refuses a hook that breaks the contract by name.
//!
//! The checks run one per test rather than through `check`: its last pass is a first install
//! with the same fake service, and the world of an update (an agent that runs, a host the
//! lease server knows) is not the world of a first install. [`AgentHost::first_install`] is.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use js_agent::update_hook::{NoGate, Report as AgentReport, swap_link};
use js_testkit::dir::TestDir;
use update_hook::conformance::{
    Allowances, Conformance, FakeService, Fault, HookCommand, LapsingCordon, Sandbox, Violation,
};
use update_hook::{Params, Scalar, Step};

use super::fakes::harness::{
    self, CONTROL_VAR, DEFECT_VAR, Defect, EXIT_GATE_BROKEN, HOST, OWNED_CORDON, PARAMS,
    ParamValue, STATE_DIR_VAR, UNIT, WORLD_VAR,
};
use super::fakes::watchdog::WINDOW_VAR;
use super::fakes::{World, relative_to_link};

pub(super) const BIN: &str = env!("CARGO_BIN_EXE_js-update-hook-fake");

/// The version the harness's params name as the previous one (`update_hook::conformance`): the
/// agent that runs when the update begins says hello with it.
pub(super) const PREVIOUS_VERSION: &str = "1.0.0";

/// How far ahead the operator's cordon of [`AgentHost::operator_cordoned`] lapses, from the fake
/// clock's start: a week, as an operator who renewed it would leave it. Longer than the hook's
/// lease (so the hook's own cordon call leaves its expiry as it is) and than every wait of an
/// update (so it is in force when the update is rolled back).
const OPERATOR_CORDON_MS: u64 = 7 * 24 * 3600 * 1000;

/// The js-agent host the harness acts on: the agent of the previous release runs on the unit,
/// the release link points at it, the lease server knows the host and holds no lease, and the
/// state root is clean. On a first install there is no agent, no link and no host.
#[derive(Clone, Copy)]
pub(super) struct AgentHost {
    pub(super) first_install: bool,
    defect: Option<Defect>,
    /// An operator has cordoned the host before the update.
    operator_cordon: bool,
    /// The watchdog window of the binary (`UPDATE_HOOK_FAKE_WATCH_MS`); its default when `None`.
    window: Option<&'static str>,
    /// Readings of the host during `prepare` that still show a job draining
    /// ([`harness::DRAIN_LOOKS`]).
    drain_looks: u64,
}

impl AgentHost {
    pub(super) fn update() -> AgentHost {
        AgentHost {
            first_install: false,
            defect: None,
            operator_cordon: false,
            window: None,
            drain_looks: 0,
        }
    }

    pub(super) fn first_install() -> AgentHost {
        AgentHost {
            first_install: true,
            ..AgentHost::update()
        }
    }

    pub(super) fn with_defect(defect: Defect) -> AgentHost {
        AgentHost {
            defect: Some(defect),
            ..AgentHost::update()
        }
    }

    /// The watchdog's windows end at the step's announcement that it is stuck, never at a time.
    pub(super) fn on_stall(self) -> AgentHost {
        AgentHost {
            window: Some(super::fakes::watchdog::ON_STALL),
            ..self
        }
    }

    /// An update on a host an operator cordoned before it: the cordon is the operator's, and
    /// the hook neither renews, records nor lifts it.
    fn operator_cordoned() -> AgentHost {
        AgentHost {
            operator_cordon: true,
            ..AgentHost::update()
        }
    }

    /// An update on a host whose last job ends while `prepare` waits: `prepare` polls through
    /// more than half of its cordon's lease, and renews the cordon it placed.
    pub(super) fn draining() -> AgentHost {
        AgentHost {
            drain_looks: 20,
            ..AgentHost::update()
        }
    }

    /// The operator's cordon, as [`FakeService::init`] left it.
    fn operator_file(sandbox: &Sandbox) -> PathBuf {
        Self::control(sandbox).join("operator-cordon.json")
    }

    /// The fakes' own files (the clock, the releases, the faults): beside the sandbox, never
    /// in it, because the harness requires every step to leave the sandbox outside `world/` and
    /// `record/` as it was, and the fake clock moves.
    pub(super) fn control(sandbox: &Sandbox) -> PathBuf {
        sandbox.root().with_extension("fake")
    }

    pub(super) fn world(sandbox: &Sandbox) -> World {
        World::new(&sandbox.world(), &Self::control(sandbox), Box::new(NoGate))
    }
}

impl FakeService for AgentHost {
    fn init(&mut self, sandbox: &Sandbox) -> io::Result<()> {
        let world = Self::world(sandbox);
        world.state_root();
        // The directory of the record's view is always there; only its file comes and goes.
        fs::create_dir_all(
            sandbox
                .world()
                .join(Path::new(OWNED_CORDON).parent().unwrap()),
        )?;
        if !self.first_install {
            world.seed_unit(UNIT, "active", "running");
            let link = sandbox.world().join("link").join("current");
            swap_link(&link, &relative_to_link(&link, &sandbox.previous()))?;
            world.seed_host(HOST, PREVIOUS_VERSION);
            if self.operator_cordon {
                world.alter_host(|host| {
                    host.cordon = Some(js_agent::update_hook::CordonView {
                        expires_at_ms: super::fakes::START_MS + OPERATOR_CORDON_MS,
                        expires_in_ms: None,
                    });
                });
                fs::copy(
                    sandbox.world().join("cordon.json"),
                    Self::operator_file(sandbox),
                )?;
            }
        }
        if self.drain_looks > 0 {
            fs::create_dir_all(Self::control(sandbox))?;
            fs::write(
                Self::control(sandbox).join(harness::DRAIN_LOOKS),
                self.drain_looks.to_string(),
            )?;
        }
        Ok(())
    }

    fn env(&self, sandbox: &Sandbox) -> Vec<(OsString, OsString)> {
        let mut env = vec![
            (WORLD_VAR.into(), sandbox.world().into_os_string()),
            (CONTROL_VAR.into(), Self::control(sandbox).into_os_string()),
            (STATE_DIR_VAR.into(), sandbox.record().into_os_string()),
        ];
        if let Some(defect) = self.defect {
            env.push((DEFECT_VAR.into(), defect.word().into()));
        }
        if let Some(window) = self.window {
            env.push((WINDOW_VAR.into(), window.into()));
        }
        env
    }

    fn params(&self) -> Params {
        let mut params = Params::default();
        for (key, value) in PARAMS {
            let value = match value {
                ParamValue::Integer(n) => Scalar::Integer(n),
                ParamValue::Text(text) => Scalar::Text(text.to_owned()),
            };
            params.insert(key, value);
        }
        params
    }

    fn inject(&mut self, sandbox: &Sandbox, step: Step, fault: Fault) -> io::Result<bool> {
        let fault = match fault {
            Fault::Fail => harness::Fault::Fail,
            Fault::Defer => harness::Fault::Defer,
            Fault::Halt => harness::Fault::Halt,
        };
        let step = agent_step(step);
        let reachable = if self.first_install {
            fault.reachable_on_first_install(step)
        } else {
            fault.reachable(step)
        };
        // The operator's cordon is not the hook's, so its `cleanup` asks the lease server
        // nothing, and a server that does not answer cannot fail it.
        let operators = self.operator_cordon
            && step == js_agent::update_hook::Step::Cleanup
            && fault == harness::Fault::Fail;
        if !reachable || operators {
            return Ok(false);
        }
        harness::arrange(&Self::control(sandbox), step, fault);
        Ok(true)
    }

    /// The hook's record, and a cordon on the host that is not the operator's. The cordon is
    /// read from the server's cordon file itself: a file that cannot be read, or that holds no
    /// cordon, is an error of the check, never "no cordon".
    fn residue(&self, sandbox: &Sandbox) -> io::Result<Vec<String>> {
        residue_of(sandbox, self.operator_cordon)
    }
}

/// [`AgentHost::residue`].
pub(super) fn residue_of(sandbox: &Sandbox, operator_cordon: bool) -> io::Result<Vec<String>> {
    let mut left = Vec::new();
    match fs::symlink_metadata(sandbox.record().join("record.json")) {
        Ok(_) => left.push("the hook record".to_owned()),
        Err(why) if why.kind() == io::ErrorKind::NotFound => {}
        Err(why) => return Err(why),
    }
    let bytes = match fs::read(sandbox.world().join("cordon.json")) {
        Ok(bytes) => bytes,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(left),
        Err(why) => return Err(why),
    };
    serde_json::from_slice::<js_agent::update_hook::CordonView>(&bytes)
        .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why))?;
    let operators = operator_cordon
        .then(|| fs::read(AgentHost::operator_file(sandbox)))
        .transpose()?;
    if operators.as_deref() != Some(bytes.as_slice()) {
        left.push("the cordon".to_owned());
    }
    Ok(left)
}

/// The hook's step of the harness's.
pub(super) fn agent_step(step: Step) -> js_agent::update_hook::Step {
    use js_agent::update_hook::Step as Agent;
    match step {
        Step::Prepare => Agent::Prepare,
        Step::Cutover => Agent::Cutover,
        Step::Verify => Agent::Verify,
        Step::Rollback => Agent::Rollback,
        Step::Cleanup => Agent::Cleanup,
    }
}

/// What the hook declares the harness should accept, each a safe behaviour of its design:
///
/// - a `rollback` run again after one that stopped the agent and deleted the record finds no
///   record and says `reversible: true` (rollback.rs: it knows nothing; the updater ignores the
///   flag on a rollback that succeeded);
/// - a cordon placed by a `prepare` killed before the record confirmed it is taken for one that
///   may be an operator's and left to lapse (prepare.rs, `place`): the cordon is the fake
///   server's `cordon.json`, and the record holds it as its own while [`OWNED_CORDON`] is there;
/// - on a first install, a `rollback` after `cutover` fails `no_previous_release` rather than
///   stop an agent that may hold a job (rollback.rs, item 3).
pub(super) fn allowances() -> Allowances {
    Allowances {
        rollback_rerun_may_say_reversible: true,
        lapsing_cordon: Some(LapsingCordon {
            paths: vec![PathBuf::from("cordon.json")],
            ownership: vec![PathBuf::from(OWNED_CORDON)],
        }),
        first_install_rollback_may_fail: true,
    }
}

fn hook() -> HookCommand {
    HookCommand::new(Path::new(BIN))
}

/// Runs `checks` on the hook with `fake`, in a directory of the test's.
fn run<T>(
    name: &str,
    mut fake: AgentHost,
    checks: impl FnOnce(&mut Conformance<'_>) -> Result<T, Violation>,
) -> Result<T, Violation> {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let hook = hook();
    let first_install = fake.first_install;
    let harness = Conformance::new(&hook, &mut fake, tmp.path()).with_allowances(allowances());
    // The params of a first install have no previous release or version.
    let mut harness = if first_install {
        harness.with_first_install()
    } else {
        harness
    };
    checks(&mut harness)
}

#[test]
fn each_step_run_twice_leaves_the_world_and_the_record_as_the_first_run_did() {
    run("hook-conf-twice", AgentHost::update(), |harness| {
        harness.idempotence()
    })
    .unwrap();
}

#[test]
fn a_first_install_run_twice_at_each_step_is_a_no_op_the_second_time() {
    run("hook-conf-first", AgentHost::first_install(), |harness| {
        harness.idempotence()
    })
    .unwrap();
}

#[test]
fn killed_at_every_kill_point_and_run_again_each_step_reaches_the_uninterrupted_end() {
    let points = run("hook-conf-resume", AgentHost::update(), |harness| {
        harness.crash_resume()
    })
    .unwrap();
    assert!(points > 0, "the steps reached no kill point");
}

#[test]
fn the_updater_table_through_a_failure_a_deferral_and_a_halt_at_each_step_ends_as_injected() {
    let (ran, skipped) = run("hook-conf-scenarios", AgentHost::update(), |harness| {
        harness.scenarios()
    })
    .unwrap();
    assert_eq!(
        ran,
        [
            "none",
            "prepare=Fail",
            "prepare=Defer",
            "prepare=Halt",
            "cutover=Fail",
            "cutover=Defer",
            "cutover=Halt",
            "verify=Fail",
            "verify=Halt",
            "cleanup=Fail",
            "cutover=Fail,rollback=Fail",
            "verify=Fail,rollback=Fail",
        ]
    );
    // What the contract gives these steps no way to end so.
    assert_eq!(
        skipped,
        [
            "verify=Defer",
            "cleanup=Defer",
            "cleanup=Halt",
            "cutover=Fail,rollback=Defer",
        ]
    );
}

#[test]
fn a_params_file_with_an_updater_key_the_hook_does_not_know_is_no_reason_to_refuse() {
    run("hook-conf-probe", AgentHost::update(), |harness| {
        harness.tolerates_unknown_updater_keys()
    })
    .unwrap();
}

#[test]
fn with_the_record_directory_locked_every_step_refuses_and_changes_nothing() {
    run("hook-conf-locked", AgentHost::update(), |harness| {
        harness.exclusive_record()
    })
    .unwrap();
}

#[test]
fn a_step_that_writes_no_last_line_is_refused_by_name() {
    let refused = run(
        "hook-conf-no-line",
        AgentHost::with_defect(Defect::NoLastLine),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    assert!(
        matches!(
            refused,
            Violation::Line {
                step: Step::Prepare,
                ..
            }
        ),
        "{refused}"
    );
}

#[test]
fn a_cutover_that_starts_the_agent_again_on_every_run_is_refused_as_not_idempotent() {
    let refused = run(
        "hook-conf-restarts",
        AgentHost::with_defect(Defect::CutoverRestarts),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    assert!(
        matches!(
            refused,
            Violation::NotIdempotent {
                step: Step::Cutover,
                ..
            }
        ),
        "{refused}"
    );
}

#[test]
fn a_step_that_never_returns_is_stopped_by_the_hang_guard_and_refused() {
    // The clock stands still and no hello comes: `verify` would look for ever. The guard ends
    // it once its waits pass the declared bound, and the harness refuses a step that died.
    let refused = run(
        "hook-conf-hang",
        AgentHost::with_defect(Defect::Hang),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    assert!(
        matches!(refused, Violation::DiedBySignal { step: Step::Verify }),
        "{refused}"
    );
}

/// The kill-point protocol the binary speaks is the harness's: the same variables and parked
/// line, and the same count file (a point the binary counts is one the harness's own gate
/// reads).
#[test]
fn the_fake_binary_speaks_the_harness_kill_point_protocol() {
    use update_hook::conformance::gate;
    assert_eq!(harness::GATE_DIR_VAR, gate::GATE_DIR_VAR);
    assert_eq!(harness::KILL_AT_VAR, gate::KILL_AT_VAR);
    assert_eq!(harness::PARKED, gate::PARKED);
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-gate");
    let dir = Some(tmp.path().as_os_str().to_owned());
    let ours = harness::HarnessGate::new(dir.clone(), None);
    assert_eq!(ours.count().unwrap(), Some(1));
    let theirs = gate::Gate::parse(dir.clone(), None).unwrap();
    assert_eq!(theirs.count(), 2);
    assert_eq!(ours.count().unwrap(), Some(3));
    // Outside the harness a point counts nothing; inside it, a count that cannot be kept is an
    // error, never a point left out.
    assert_eq!(harness::HarnessGate::new(None, None).count().unwrap(), None);
    let unwritable = harness::HarnessGate::new(Some(tmp.path().join("absent").into()), None);
    assert!(unwritable.count().is_err());
    // A count that worked once and then cannot be kept is an error at the next point.
    let fresh = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-gate-break");
    let mut breaking = harness::HarnessGate::new(Some(fresh.path().as_os_str().to_owned()), None);
    breaking.break_after_first = true;
    assert_eq!(breaking.count().unwrap(), Some(1));
    assert!(breaking.count().is_err());
}

/// A kill point whose count fails after earlier points were counted ends the step with a status
/// outside the contract, and the harness refuses the run by name: a point is never left out of
/// the measurement and the replay in silence.
#[test]
fn a_kill_point_that_cannot_be_counted_fails_the_run_visibly() {
    let refused = run(
        "hook-conf-broken-count",
        AgentHost::with_defect(Defect::BrokenCount),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    assert!(
        matches!(
            refused,
            Violation::UnknownExit {
                step: Step::Prepare,
                code,
                ..
            } if code == i32::from(EXIT_GATE_BROKEN)
        ),
        "{refused}"
    );
}

/// A step that blocks without ever pausing (a call that never returns) is ended by the
/// watchdog thread: the step's whole process group is killed, and the harness reaps it and
/// refuses the stalled step, `verify`, as one that died of a signal. The watchdog's window ends
/// when the stuck call announces itself, so nothing here depends on how long any step takes.
#[test]
fn a_step_that_blocks_without_pausing_is_killed_by_the_watchdog_and_refused() {
    let host = AgentHost::with_defect(Defect::Stall).on_stall();
    let refused = run("hook-conf-stall", host, |harness| harness.idempotence()).unwrap_err();
    assert!(
        matches!(refused, Violation::DiedBySignal { step: Step::Verify }),
        "{refused}"
    );
}

/// With the watchdog's windows ending at a stuck call's announcement, a step that blocks
/// without announcing it is ended all the same, by the window's span: the fail-safe holds in
/// both modes. The span is short so the test does not wait long; the step never makes progress,
/// so it dies whatever the span, and the test asserts only that a step was killed and reaped,
/// not which or when.
#[test]
fn a_step_that_blocks_without_announcing_it_is_killed_by_the_fail_safe_window() {
    let host = AgentHost {
        window: Some("on-stall:300"),
        ..AgentHost::with_defect(Defect::SilentStall)
    };
    let refused = run("hook-conf-silent-stall", host, |harness| {
        harness.idempotence()
    })
    .unwrap_err();
    assert!(
        matches!(refused, Violation::DiedBySignal { .. }),
        "{refused}"
    );
}

/// A step that loops for ever reaching kill points and never pausing makes progress the
/// watchdog sees, and is ended all the same once it passes the bound on kill points; the harness
/// refuses it as one that died of a signal. Counted, not timed.
#[test]
fn a_step_that_reaches_kill_points_for_ever_is_killed_and_refused() {
    let refused = run(
        "hook-conf-point-loop",
        AgentHost::with_defect(Defect::PointLoop),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    assert!(
        matches!(refused, Violation::DiedBySignal { step: Step::Verify }),
        "{refused}"
    );
}

/// With the allowances declared, a hook that lifts an operator's cordon is refused: after a
/// deferred `prepare`, the rollback says it was reversible, and the world is not as it was before
/// the update (the operator's cordon is gone).
#[test]
fn a_rollback_that_lifts_an_operators_cordon_is_refused_with_the_allowances_declared() {
    let host = AgentHost {
        defect: Some(Defect::LiftsAnyCordon),
        ..AgentHost::operator_cordoned()
    };
    let refused = run("hook-conf-lifts-op", host, |harness| {
        harness.scenario(&[(Step::Prepare, Fault::Defer)])
    })
    .unwrap_err();
    match &refused {
        Violation::RollbackClaimsReversible { changes } => {
            assert_eq!(changes, &["removed cordon.json"]);
        }
        other => panic!("{other}"),
    }
}

/// With the allowances declared, a cordon the hook recorded as its own and then did not lift is
/// still refused: the allowance excuses only the crash window.
#[test]
fn a_cleanup_that_leaves_its_own_cordon_is_refused_with_the_allowances_declared() {
    let refused = run(
        "hook-conf-forgets",
        AgentHost::with_defect(Defect::ForgetsCordon),
        |harness| harness.idempotence(),
    )
    .unwrap_err();
    match &refused {
        Violation::ResidueLeft {
            step: Step::Cleanup,
            left,
        } => assert_eq!(left, &["the cordon"]),
        other => panic!("{other}"),
    }
}

/// On a host an operator cordoned before the update, every check passes with the allowances
/// declared, and the operator's cordon is never the hook's: it is not residue, and the hook
/// leaves its file as it found it. The cordon path is there before any step, so no kill point
/// opens the crash window: every crash on this host is compared strictly.
#[test]
fn on_a_host_an_operator_cordoned_the_checks_pass_and_the_cordon_is_left_as_it_was() {
    let host = AgentHost::operator_cordoned();
    run("hook-conf-op-twice", host, |harness| harness.idempotence()).unwrap();
    run("hook-conf-op-resume", host, |harness| {
        harness.crash_resume()
    })
    .unwrap();
    let (ran, _) = run("hook-conf-op-table", host, |harness| harness.scenarios()).unwrap();
    assert!(ran.len() > 1, "{ran:?}");
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-op-file");
    let sandbox = Sandbox::create(tmp.path().join("run")).unwrap();
    let mut fake = host;
    fake.init(&sandbox).unwrap();
    assert_eq!(fake.residue(&sandbox).unwrap(), Vec::<String>::new());
    fs::write(
        sandbox.world().join("cordon.json"),
        br#"{"expires_at_ms":1}"#,
    )
    .unwrap();
    assert_eq!(fake.residue(&sandbox).unwrap(), ["the cordon"]);
}

/// The residue check reads the cordon itself: a cordon file that does not parse, or that is
/// not a file, makes the check fail rather than read as no cordon; a record directory entry
/// that cannot be looked at is an error too.
#[test]
fn residue_that_cannot_be_read_fails_the_check_and_is_never_read_as_none() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-residue");
    let sandbox = Sandbox::create(tmp.path().join("run")).unwrap();
    let mut fake = AgentHost::update();
    fake.init(&sandbox).unwrap();
    assert_eq!(fake.residue(&sandbox).unwrap(), Vec::<String>::new());
    let cordon = sandbox.world().join("cordon.json");
    fs::write(&cordon, b"not json").unwrap();
    assert_eq!(
        fake.residue(&sandbox).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    fs::remove_file(&cordon).unwrap();
    fs::create_dir(&cordon).unwrap();
    assert!(fake.residue(&sandbox).is_err());
    fs::remove_dir(&cordon).unwrap();
    fs::write(sandbox.record().join("record.json"), b"{}").unwrap();
    assert_eq!(fake.residue(&sandbox).unwrap(), ["the hook record"]);
}

/// A taint halts `rollback` before the commit (after a failed `prepare`) and after it (after a
/// failed `cutover`): the updater's table stops with a halt either way.
#[test]
fn a_tainted_rollback_halts_before_and_after_the_commit() {
    use update_hook::Verdict;
    for (n, first) in [Step::Prepare, Step::Cutover].into_iter().enumerate() {
        let faults = [(first, Fault::Fail), (Step::Rollback, Fault::Halt)];
        let trace = run(
            &format!("hook-conf-rb-halt-{n}"),
            AgentHost::update(),
            |harness| harness.scenario(&faults),
        )
        .unwrap()
        .expect("the fake makes both");
        assert_eq!(trace.verdict, Verdict::Halt, "{:?}", trace.steps);
        assert_eq!(
            trace.steps.last(),
            Some(&(Step::Rollback, update_hook::Outcome::Halt))
        );
    }
}

/// Every last line the hook can write reads on the harness's strict reader with the same
/// fields, and every outcome exits with the harness's code for it.
#[test]
fn every_last_line_and_exit_code_of_the_hook_is_the_harness_contract() {
    use js_agent::update_hook::{Class, Outcome, Reason};
    for reason in Reason::ALL {
        for class in [Class::Release, Class::Environment] {
            for reversible in [true, false] {
                let mine = AgentReport {
                    class,
                    retry_after_secs: Some(300),
                    reversible,
                    ..AgentReport::failure(reason, "a detail")
                };
                let theirs = update_hook::Report::from_stdout(&format!("{}\n", mine.to_line()))
                    .unwrap_or_else(|why| panic!("{}: {why}", mine.to_line()));
                assert_eq!(theirs.reason.as_deref(), Some(reason.as_str()));
                assert_eq!(theirs.class.as_str(), class.as_str());
                assert_eq!(theirs.retry_after_secs, Some(300));
                assert_eq!(theirs.reversible, reversible);
                assert_eq!(theirs.detail.as_deref(), Some("a detail"));
            }
        }
    }
    let empty =
        update_hook::Report::from_stdout(&format!("{}\n", AgentReport::default().to_line()))
            .unwrap();
    let nothing = update_hook::Report::default();
    assert_eq!(
        (
            empty.class,
            empty.reason,
            empty.retry_after_secs,
            empty.detail
        ),
        (
            nothing.class,
            nothing.reason,
            nothing.retry_after_secs,
            nothing.detail
        )
    );
    assert!(empty.reversible && !empty.promotion_pending);
    for (mine, theirs) in [
        (Outcome::Success, update_hook::Outcome::Success),
        (Outcome::Failure, update_hook::Outcome::Failure),
        (Outcome::Deferred, update_hook::Outcome::Deferred),
        (Outcome::Halt, update_hook::Outcome::Halt),
    ] {
        assert_eq!(
            update_hook::Outcome::from_code(i32::from(mine.code())),
            Some(theirs)
        );
    }
}

/// A first install through the updater's table: success, and every fault the fake can make on a
/// first install that leads to no rollback after the link moved, or to one that is itself made
/// to fail. Each ends with the verdict the table gives it.
#[test]
fn a_first_install_through_the_updater_table_ends_as_each_fault_asks() {
    use update_hook::{Class, Phase, Verdict};
    let failed = |phase| Verdict::Failed {
        phase,
        class: Class::Release,
    };
    let cases: [(&[(Step, Fault)], Verdict); 8] = [
        (
            &[],
            Verdict::Succeeded {
                promotion_pending: false,
            },
        ),
        (&[(Step::Prepare, Fault::Fail)], failed(Phase::BeforeCommit)),
        (&[(Step::Prepare, Fault::Halt)], Verdict::Halt),
        (&[(Step::Cutover, Fault::Halt)], Verdict::Halt),
        (&[(Step::Verify, Fault::Halt)], Verdict::Halt),
        // The state directory cannot be looked at: `cleanup` fails after the commit, is retried,
        // and the update ends failed after the commit, with no rollback.
        (&[(Step::Cleanup, Fault::Fail)], failed(Phase::AfterCommit)),
        (
            &[(Step::Cutover, Fault::Fail), (Step::Rollback, Fault::Fail)],
            failed(Phase::AfterCommit),
        ),
        (
            &[(Step::Verify, Fault::Fail), (Step::Rollback, Fault::Fail)],
            failed(Phase::AfterCommit),
        ),
    ];
    for (n, (faults, verdict)) in cases.into_iter().enumerate() {
        let trace = run(
            &format!("hook-conf-first-table-{n}"),
            AgentHost::first_install(),
            |harness| harness.scenario(faults),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("{faults:?}: the fake cannot make it"));
        assert_eq!(trace.verdict, verdict, "{faults:?}: {:?}", trace.steps);
    }
    // What a first install has no way to end with.
    for faults in [
        [(Step::Prepare, Fault::Defer)],
        [(Step::Cutover, Fault::Defer)],
    ] {
        let made = run(
            "hook-conf-first-none",
            AgentHost::first_install(),
            |harness| harness.scenario(&faults),
        )
        .unwrap();
        assert_eq!(made, None, "{faults:?}");
    }
}

/// What the harness still refuses on a first install, and only that: its crash-resume and its
/// sequences run a `rollback` after `cutover` and require it to succeed, and the hook's fails
/// `no_previous_release` by design (rollback.rs, item 3). The first-install allowance covers
/// the `rollback` of the run-twice check alone (update_hook::conformance::Allowances). Catches a
/// first install that fails anything else on the way, and tells when the harness extends the
/// allowance: then these become the checks themselves. The harness's refusal carries no
/// reason; conformance_runs.rs drives the same paths without it and asserts the reason, the
/// unchanged state and the updater's verdict, and kills the first install at every kill point.
#[test]
fn on_a_first_install_the_harness_refuses_only_the_designed_rollback_after_cutover() {
    let designed = |refused: Violation| {
        assert!(
            matches!(
                refused,
                Violation::UnexpectedOutcome {
                    step: Step::Rollback,
                    wanted: update_hook::Outcome::Success,
                    got: update_hook::Outcome::Failure,
                }
            ),
            "{refused}"
        );
    };
    designed(
        run(
            "hook-conf-first-resume",
            AgentHost::first_install(),
            |harness| harness.crash_resume(),
        )
        .unwrap_err(),
    );
    for (n, step) in [Step::Cutover, Step::Verify].into_iter().enumerate() {
        designed(
            run(
                &format!("hook-conf-first-rb-{n}"),
                AgentHost::first_install(),
                |harness| harness.scenario(&[(step, Fault::Fail)]),
            )
            .unwrap_err(),
        );
    }
}

/// Undeclared, each of the three behaviours is refused by the check that finds it: the
/// allowances are what lets them through, and nothing else is.
#[test]
fn without_the_allowances_the_harness_refuses_exactly_the_three_designed_behaviours() {
    let strict =
        |name: &str, fake: AgentHost, check: &dyn Fn(&mut Conformance<'_>) -> Violation| {
            let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
            let hook = hook();
            let first_install = fake.first_install;
            let mut fake = fake;
            let harness = Conformance::new(&hook, &mut fake, tmp.path());
            let mut harness = if first_install {
                harness.with_first_install()
            } else {
                harness
            };
            check(&mut harness)
        };
    let c1 = strict("hook-conf-strict-c1", AgentHost::update(), &|h| {
        h.idempotence().unwrap_err()
    });
    assert!(
        matches!(
            c1,
            Violation::RerunReversibleChanged {
                step: Step::Rollback,
                first: false,
                second: true
            }
        ),
        "{c1}"
    );
    let c2 = strict("hook-conf-strict-c2", AgentHost::update(), &|h| {
        h.crash_resume().unwrap_err()
    });
    match &c2 {
        Violation::CrashResumeDiverges {
            step: Step::Prepare,
            changes,
            ..
        } => assert_eq!(changes, &["added cordon.json"]),
        other => panic!("{other}"),
    }
    let c3 = strict("hook-conf-strict-c3", AgentHost::first_install(), &|h| {
        h.idempotence().unwrap_err()
    });
    assert!(
        matches!(
            c3,
            Violation::UnexpectedOutcome {
                step: Step::Rollback,
                ..
            }
        ),
        "{c3}"
    );
}
