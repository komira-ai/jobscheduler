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

use js_agent::update_hook::{Lease, NoGate, Report as AgentReport, swap_link};
use js_testkit::dir::TestDir;
use update_hook::conformance::{
    Allowances, Conformance, FakeService, Fault, HookCommand, LapsingCordon, Sandbox, Violation,
};
use update_hook::{Params, Scalar, Step};

use super::fakes::harness::{
    self, CONTROL_VAR, DEFECT_VAR, Defect, HOST, OWNED_CORDON, PARAMS, ParamValue, RELEASE_LINK,
    STATE_DIR_VAR, UNIT, WORLD_VAR,
};
use super::fakes::{World, relative_to_link};

const BIN: &str = env!("CARGO_BIN_EXE_js-update-hook-fake");

/// The version the harness's params name as the previous one (`update_hook::conformance`): the
/// agent that runs when the update begins says hello with it.
const PREVIOUS_VERSION: &str = "1.0.0";

/// The js-agent host the harness acts on: the agent of the previous release runs on the unit,
/// the release link points at it, the lease server knows the host and holds no lease, and the
/// state root is clean. On a first install there is no agent, no link and no host.
struct AgentHost {
    first_install: bool,
    defect: Option<Defect>,
}

impl AgentHost {
    fn update() -> AgentHost {
        AgentHost {
            first_install: false,
            defect: None,
        }
    }

    fn first_install() -> AgentHost {
        AgentHost {
            first_install: true,
            defect: None,
        }
    }

    fn with_defect(defect: Defect) -> AgentHost {
        AgentHost {
            defect: Some(defect),
            ..AgentHost::update()
        }
    }

    /// The fakes' own files (the clock, the releases, the faults): beside the sandbox, never
    /// in it, because the harness requires every step to leave the sandbox outside `world/` and
    /// `record/` as it was, and the fake clock moves.
    fn control(sandbox: &Sandbox) -> PathBuf {
        sandbox.root().with_extension("fake")
    }

    fn world(sandbox: &Sandbox) -> World {
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
        if !reachable {
            return Ok(false);
        }
        harness::arrange(&Self::control(sandbox), step, fault);
        Ok(true)
    }

    /// The hook's record, and a cordon in force on the host.
    fn residue(&self, sandbox: &Sandbox) -> io::Result<Vec<String>> {
        let mut left = Vec::new();
        if sandbox.record().join("record.json").exists() {
            left.push("the hook record".to_owned());
        }
        if Self::world(sandbox)
            .host()
            .is_ok_and(|host| host.cordon.is_some())
        {
            left.push("the cordon".to_owned());
        }
        Ok(left)
    }
}

/// The hook's step of the harness's.
fn agent_step(step: Step) -> js_agent::update_hook::Step {
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
fn allowances() -> Allowances {
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
    assert_eq!(ours.count(), Some(1));
    let theirs = gate::Gate::parse(dir, None).unwrap();
    assert_eq!(theirs.count(), 2);
    assert_eq!(ours.count(), Some(3));
    // Outside the harness a point counts nothing.
    assert_eq!(harness::HarnessGate::new(None, None).count(), None);
    let unwritable = harness::HarnessGate::new(Some(tmp.path().join("absent").into()), None);
    assert_eq!(unwritable.count(), None);
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

/// The release link the params name is inside the declaration's write paths, as the hook's
/// `cutover` requires, and the declaration parses over the harness's record directory.
#[test]
fn the_declaration_of_the_fake_parses_and_covers_the_release_link() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-decl");
    let record = tmp.path().join("record");
    fs::create_dir_all(&record).unwrap();
    let declaration = harness::declaration(&record).unwrap();
    assert_eq!(declaration.state_dir, record);
    assert!(
        declaration
            .write_paths
            .iter()
            .any(|path| Path::new(RELEASE_LINK).starts_with(path))
    );
}

/// A clock of the test's: a pause moves it on by what was asked.
struct TestClock(std::cell::Cell<u64>);

impl js_agent::update_hook::Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }

    fn pause(&self, span: std::time::Duration) {
        self.0
            .set(self.0.get() + u64::try_from(span.as_millis()).unwrap());
    }
}

/// The guard lets a step wait up to its declared bound and fires on the first pause past it,
/// with the total asked; the clock it guards moves only for the pauses it lets through.
#[test]
fn the_hang_guard_fires_on_the_first_wait_past_the_bound_and_not_before() {
    use js_agent::update_hook::Clock;
    use std::cell::RefCell;
    use std::time::Duration;
    let clock = TestClock(std::cell::Cell::new(0));
    let fired = RefCell::new(Vec::new());
    let fire = |asked: Duration| fired.borrow_mut().push(asked);
    let guard = harness::Guard::new(&clock, Duration::from_secs(30), &fire);
    for _ in 0..3 {
        guard.pause(Duration::from_secs(10));
    }
    assert_eq!(fired.borrow().as_slice(), &[] as &[Duration]);
    assert_eq!(
        (guard.now_ms(), guard.asked()),
        (30_000, Duration::from_secs(30))
    );
    guard.pause(Duration::from_secs(1));
    assert_eq!(fired.borrow().as_slice(), [Duration::from_secs(31)]);
    assert_eq!(
        guard.now_ms(),
        30_000,
        "the wait past the bound is not taken"
    );
}

/// On a clock that stands still the guard still counts what is asked: a step whose own
/// deadline never comes is stopped all the same.
#[test]
fn the_hang_guard_counts_the_waits_asked_even_when_the_clock_stands_still() {
    use js_agent::update_hook::Clock;
    use std::cell::Cell;
    use std::time::Duration;
    let clock = TestClock(Cell::new(5));
    let frozen = harness::Frozen(&clock);
    let fired = Cell::new(0);
    let fire = |_: Duration| fired.set(fired.get() + 1);
    let budget = harness::Guard::budget_of(js_agent::update_hook::Step::Verify);
    assert_eq!(budget, Duration::from_secs(615));
    let guard = harness::Guard::new(&frozen, budget, &fire);
    let mut pauses = 0;
    while fired.get() == 0 {
        guard.pause(Duration::from_secs(10));
        pauses += 1;
    }
    assert_eq!(pauses, 62);
    assert_eq!(guard.now_ms(), 5);
}

/// The faults each step of the hook can be made to end with, and the words of the faults and
/// the defects.
#[test]
fn the_faults_a_step_can_end_with_and_the_words_that_name_faults_and_defects() {
    use harness::Fault as F;
    use js_agent::update_hook::Step as S;
    let reachable: Vec<(S, F)> = [S::Prepare, S::Cutover, S::Verify, S::Rollback, S::Cleanup]
        .into_iter()
        .flat_map(|step| [F::Fail, F::Defer, F::Halt].map(|fault| (step, fault)))
        .filter(|(step, fault)| fault.reachable(*step))
        .collect();
    assert_eq!(
        reachable,
        [
            (S::Prepare, F::Fail),
            (S::Prepare, F::Defer),
            (S::Prepare, F::Halt),
            (S::Cutover, F::Fail),
            (S::Cutover, F::Defer),
            (S::Cutover, F::Halt),
            (S::Verify, F::Fail),
            (S::Verify, F::Halt),
            (S::Rollback, F::Fail),
            (S::Cleanup, F::Fail),
        ]
    );
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "hook-conf-faults");
    assert_eq!(harness::arranged(tmp.path(), S::Verify), None);
    for fault in [F::Fail, F::Defer, F::Halt] {
        harness::arrange(tmp.path(), S::Verify, fault);
        assert_eq!(harness::arranged(tmp.path(), S::Verify), Some(fault));
    }
    fs::write(tmp.path().join("faults/verify"), "melt").unwrap();
    assert_eq!(harness::arranged(tmp.path(), S::Verify), None);
    for defect in [Defect::NoLastLine, Defect::CutoverRestarts, Defect::Hang] {
        assert_eq!(Defect::from_word(Some(&defect.word().into())), Some(defect));
    }
    assert_eq!(Defect::from_word(Some(&"melt".into())), None);
    assert_eq!(Defect::from_word(None), None);
}

/// A first install through the updater's table: success, and every fault the steps can end with
/// on a first install whose rollback, if any, runs before the link moved or is itself made to
/// fail. Each ends with the verdict the table gives it.
#[test]
fn a_first_install_through_the_updater_table_ends_as_each_fault_asks() {
    use update_hook::{Class, Phase, Verdict};
    let failed = |phase| Verdict::Failed {
        phase,
        class: Class::Release,
    };
    let cases: [(&[(Step, Fault)], Verdict); 7] = [
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
        [(Step::Cleanup, Fault::Fail)],
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
/// allowance: then these become the checks themselves.
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
