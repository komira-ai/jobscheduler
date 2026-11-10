//! The hook binary driven step by step as the harness drives it, for what the harness's checks
//! do not express: a first install killed at every kill point of its success path (the harness
//! reaches its rollback path first), the outcome and the verdict of a first install's rollback
//! after a failed `verify`, a kill between a unit's start and its agent's
//! hello, a fault that is cleared before the step runs again, and rollback killed at every kill
//! point after a `prepare` or a `cutover` that was cut short.
//!
//! A [`Bed`] is one sandbox of the harness's layout (`hostkeeper_hook::conformance::Sandbox`), set
//! up by the conformance tests' [`AgentHost`] and given one params file, as the harness writes
//! it. Each run is a process of its own, in a process group of its own, and is stopped at a kill
//! point the way the harness stops one: on the parked line, the whole group is killed.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use hostkeeper_hook::conformance::{FakeService, Sandbox, Tree, changes, snapshot};
use hostkeeper_hook::{
    ARTIFACT_DIGEST_KEY, Context, Next, Outcome, PREVIOUS_RELEASE_KEY, PREVIOUS_VERSION_KEY,
    Report, STAGED_RELEASE_KEY, Scalar, Step, TARGET_VERSION_KEY, Verdict, next,
};
use js_testkit::dir::TestDir;

use super::conformance::{AgentHost, BIN, PREVIOUS_VERSION, agent_step};
use super::fakes::harness::{self, COUNT_FILE, GATE_DIR_VAR, KILL_AT_VAR, PARKED, UNIT};

const TARGET_VERSION: &str = "2.0.0";
const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The steps of an update that succeeds.
const SUCCESS: [Step; 4] = [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup];

/// How one run ended.
#[derive(Debug)]
struct Ran {
    /// The exit status; `None` when it was killed.
    code: Option<i32>,
    /// The last line, read strictly; `None` for a run killed at a kill point.
    report: Option<Report>,
    /// Kill points reached.
    points: u64,
}

impl Ran {
    fn outcome(&self) -> Outcome {
        Outcome::from_exit(self.code)
    }

    fn ok(self, step: Step) -> Ran {
        assert_eq!(self.code, Some(0), "{step}: {self:?}");
        self
    }
}

/// One sandbox and its fake host.
struct Bed {
    _tmp: TestDir,
    sandbox: Sandbox,
    host: AgentHost,
}

impl Bed {
    fn new(name: &str, host: AgentHost) -> Bed {
        let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let sandbox = Sandbox::create(tmp.path().join("run")).unwrap();
        let mut fake = host;
        fake.init(&sandbox).unwrap();
        let mut params = fake.params().with_promotion_ceiling_secs(3600);
        let text = |path: &Path| Scalar::Text(path.to_string_lossy().into_owned());
        params.insert(STAGED_RELEASE_KEY, text(&sandbox.artifact()));
        params.insert(TARGET_VERSION_KEY, Scalar::Text(TARGET_VERSION.to_owned()));
        params.insert(ARTIFACT_DIGEST_KEY, Scalar::Text(DIGEST.to_owned()));
        if !host.first_install {
            fs::write(sandbox.previous().join("VERSION"), PREVIOUS_VERSION).unwrap();
            params.insert(PREVIOUS_RELEASE_KEY, text(&sandbox.previous()));
            params.insert(
                PREVIOUS_VERSION_KEY,
                Scalar::Text(PREVIOUS_VERSION.to_owned()),
            );
        }
        fs::write(sandbox.control().join("params.toml"), params.to_toml()).unwrap();
        Bed {
            _tmp: tmp,
            sandbox,
            host,
        }
    }

    fn update(name: &str) -> Bed {
        Bed::new(name, AgentHost::update())
    }

    fn first_install(name: &str) -> Bed {
        Bed::new(name, AgentHost::first_install())
    }

    /// Runs `step`; with `kill_at` above zero, kills it at that kill point.
    fn run(&self, step: Step, kill_at: u64) -> Ran {
        self.run_grouped(step, kill_at, true)
    }

    /// [`Bed::run`], in a process group of the run's own (as the harness starts a step) or,
    /// with `own_group` false, in the test's.
    fn run_grouped(&self, step: Step, kill_at: u64, own_group: bool) -> Ran {
        let count = self.sandbox.control().join(COUNT_FILE);
        fs::write(&count, b"").unwrap();
        let mut command = Command::new(BIN);
        command.arg("update-hook").arg(step.name());
        if step == Step::Prepare {
            command.arg(self.sandbox.artifact());
        }
        command
            .arg("--params")
            .arg(self.sandbox.control().join("params.toml"))
            .env_clear()
            .envs(self.host.env(&self.sandbox))
            .env(GATE_DIR_VAR, self.sandbox.control())
            .env(KILL_AT_VAR, kill_at.to_string())
            .current_dir(self.sandbox.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if own_group {
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        let mut out = child.stdout.take().unwrap();
        let stdout = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = out.read_to_string(&mut text);
            text
        });
        let mut parked = false;
        for line in BufReader::new(child.stderr.take().unwrap()).lines() {
            if line.unwrap() == PARKED {
                parked = true;
                let group = rustix::process::Pid::from_child(&child);
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL).unwrap();
                break;
            }
        }
        let status = child.wait().unwrap();
        let stdout = stdout.join().unwrap();
        let points = fs::metadata(&count).map_or(0, |meta| meta.len());
        assert_eq!(parked, kill_at > 0, "{step} at {kill_at}: parked {parked}");
        Ran {
            code: status.code(),
            report: (!parked).then(|| Report::from_stdout(&stdout).unwrap()),
            points,
        }
    }

    fn ok(&self, step: Step) -> Ran {
        self.run(step, 0).ok(step)
    }

    /// The world and the record, as the harness compares them.
    fn state(&self) -> Tree {
        let mut tree = snapshot(&self.sandbox.world()).unwrap();
        for (path, entry) in snapshot(&self.sandbox.record()).unwrap() {
            tree.insert(Path::new("record").join(path), entry);
        }
        tree
    }

    fn arrange(&self, step: Step, fault: harness::Fault) {
        harness::arrange(&AgentHost::control(&self.sandbox), agent_step(step), fault);
    }

    fn clear(&self, step: Step) {
        let file = AgentHost::control(&self.sandbox)
            .join("faults")
            .join(step.name());
        fs::remove_file(file).unwrap();
    }

    fn epoch(&self) -> u64 {
        use js_agent::update_hook::Lease;
        AgentHost::world(&self.sandbox).host().unwrap().epoch
    }

    fn unit(&self) -> String {
        use js_agent::update_hook::Units;
        let state = AgentHost::world(&self.sandbox).show(UNIT).unwrap();
        format!("{} {}", state.active, state.sub)
    }

    fn pending_hello(&self) -> bool {
        AgentHost::control(&self.sandbox)
            .join("pending-hello")
            .exists()
    }
}

/// `want` and `got` the same, said by the paths that differ.
fn same(want: &Tree, got: &Tree, what: &str) {
    let differ = changes(want, got);
    assert!(differ.is_empty(), "{what}: {differ:?}");
}

/// The kill points of each step of `path`, run to its end on a bed of its own.
fn points_of(name: &str, host: AgentHost, path: &[Step]) -> Vec<u64> {
    let bed = Bed::new(name, host);
    path.iter().map(|step| bed.ok(*step).points).collect()
}

/// A first install killed at every kill point of every step, and every step run again after it
/// finished, then run to the end, reaches the world and the record of an uninterrupted first
/// install. This is the harness's crash-resume over a first install's success path, which the
/// harness itself does not reach (its rollback path comes first).
#[test]
fn a_first_install_killed_at_every_kill_point_resumes_to_the_uninterrupted_end() {
    let whole = Bed::first_install("hook-runs-first-whole");
    for step in SUCCESS {
        whole.ok(step);
    }
    let want = whole.state();
    let points = points_of(
        "hook-runs-first-points",
        AgentHost::first_install(),
        &SUCCESS,
    );
    assert!(points.iter().sum::<u64>() > 0, "{points:?}");
    for (index, gates) in points.iter().enumerate() {
        for kill_at in 0..=*gates {
            let bed = Bed::first_install(&format!("hook-runs-first-{index}-{kill_at}"));
            for step in &SUCCESS[..index] {
                bed.ok(*step);
            }
            bed.run(SUCCESS[index], kill_at);
            for step in &SUCCESS[index..] {
                bed.ok(*step);
            }
            same(
                &want,
                &bed.state(),
                &format!("{} killed at {kill_at}", SUCCESS[index]),
            );
        }
    }
}

/// On a first install, a `rollback` after a `verify` that failed stops the agent `cutover`
/// started and exits 0: the unit is down, the record is gone, and a rerun succeeds and changes
/// nothing. The updater reports the update as the failed `verify` made it (failed after the
/// commit, a release matter), through the rollback's success rather than a rollback failure.
#[test]
fn a_first_install_rollback_after_a_failed_verify_stops_the_agent_and_succeeds() {
    let bed = Bed::first_install("hook-runs-first-rollback");
    bed.ok(Step::Prepare);
    let cutover = bed.ok(Step::Cutover);
    assert_eq!(bed.unit(), "active running");
    let mut ctx = Context::default();
    ctx.observe(Step::Cutover, cutover.report.as_ref().unwrap());
    let Next::Rollback { then } = next(Step::Verify, Outcome::Failure, &Report::default(), &ctx)
    else {
        panic!("a failed verify is rolled back");
    };
    ctx.then = Some(then);
    let ran = bed.run(Step::Rollback, 0);
    assert_eq!(ran.outcome(), Outcome::Success, "{ran:?}");
    let report = ran.report.unwrap();
    assert_eq!(report.reason, None);
    assert!(!report.reversible, "the link was moved");
    assert_eq!(bed.unit(), "inactive dead");
    let record = Path::new("record").join(js_agent::update_hook::RECORD_FILE);
    assert!(!bed.state().contains_key(&record), "the record is deleted");
    assert_eq!(
        next(Step::Rollback, Outcome::Success, &report, &ctx),
        Next::Report(Verdict::Failed {
            phase: hostkeeper_hook::Phase::AfterCommit,
            class: hostkeeper_hook::Class::Release,
        })
    );
    let after = bed.state();
    let again = bed.run(Step::Rollback, 0);
    assert_eq!(again.outcome(), Outcome::Success, "{again:?}");
    same(&after, &bed.state(), "a rollback run again");
}

/// The kill point of `cutover` that falls between the unit's start and the agent's hello: the
/// unit runs, the hello is pending.
fn hello_window(name: &str, before: &[Step], step: Step) -> u64 {
    let gates = {
        let bed = Bed::update(&format!("{name}-n"));
        for s in before {
            bed.ok(*s);
        }
        bed.ok(step).points
    };
    (1..=gates)
        .find(|kill_at| {
            let bed = Bed::update(&format!("{name}-{kill_at}"));
            for s in before {
                bed.ok(*s);
            }
            bed.run(step, *kill_at);
            bed.pending_hello()
        })
        .unwrap_or_else(|| panic!("{step} has no kill point between a start and its hello"))
}

/// Killed after the unit started and before its agent's hello reached the server, `cutover` run
/// again finds the unit running and does not start it again; the hello arrives on its own, and
/// `verify` and `cleanup` end where an uninterrupted update ends. The same holds for the start
/// of the previous release in `rollback`.
#[test]
fn a_kill_between_a_start_and_its_hello_resumes_without_starting_the_unit_again() {
    let whole = Bed::update("hook-runs-hello-whole");
    for step in SUCCESS {
        whole.ok(step);
    }
    let want = whole.state();
    let kill_at = hello_window("hook-runs-hello-cut", &[Step::Prepare], Step::Cutover);
    let bed = Bed::update("hook-runs-hello");
    bed.ok(Step::Prepare);
    let epoch = bed.epoch();
    bed.run(Step::Cutover, kill_at);
    assert!(bed.pending_hello());
    assert_eq!(bed.unit(), "active running");
    assert_eq!(bed.epoch(), epoch, "the hello has not arrived");
    let units = fs::read(bed.sandbox.world().join("units").join(UNIT)).unwrap();
    bed.ok(Step::Cutover);
    assert!(!bed.pending_hello());
    assert_eq!(bed.epoch(), epoch + 1, "one hello, no second start");
    assert_eq!(
        fs::read(bed.sandbox.world().join("units").join(UNIT)).unwrap(),
        units,
        "the unit was not stopped or started again"
    );
    bed.ok(Step::Verify);
    bed.ok(Step::Cleanup);
    same(&want, &bed.state(), "resumed after the hello window");

    let rolled = Bed::update("hook-runs-hello-rb-whole");
    rolled.ok(Step::Prepare);
    rolled.ok(Step::Cutover);
    rolled.ok(Step::Rollback);
    let want = rolled.state();
    let before = [Step::Prepare, Step::Cutover];
    let kill_at = hello_window("hook-runs-hello-rb", &before, Step::Rollback);
    let bed = Bed::update("hook-runs-hello-rb");
    for step in before {
        bed.ok(step);
    }
    bed.run(Step::Rollback, kill_at);
    assert!(bed.pending_hello());
    let epoch = bed.epoch();
    bed.ok(Step::Rollback);
    assert_eq!(bed.epoch(), epoch + 1, "the previous agent said hello once");
    same(
        &want,
        &bed.state(),
        "rollback resumed after the hello window",
    );
}

/// The lease server's view of the host without the session stamps, which follow the fake
/// clock: an update that waited out a deferral says hello later than one that did not.
fn without_stamps(mut tree: Tree) -> Tree {
    use hostkeeper_hook::conformance::Entry;
    for name in ["lease.json"] {
        if let Some(Entry::File(bytes)) = tree.get_mut(Path::new(name)) {
            let mut host: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            host.as_object_mut().unwrap().remove("session_started_ms");
            *bytes = serde_json::to_vec(&host).unwrap();
        }
    }
    tree
}

/// A step that fails or defers ends the same way every time it is run again while the fault
/// lasts, changing nothing more; once the fault is gone it runs to success, and the update ends
/// where an uninterrupted one does (session stamps aside: the clock moved during the waits).
#[test]
fn a_failed_or_deferred_step_repeats_then_resumes_once_the_fault_is_gone() {
    use harness::Fault as F;
    let whole = Bed::update("hook-runs-clear-whole");
    for step in SUCCESS {
        whole.ok(step);
    }
    let want = without_stamps(whole.state());
    let cases = [
        (Step::Prepare, F::Defer, Outcome::Deferred),
        (Step::Prepare, F::Fail, Outcome::Failure),
        (Step::Cutover, F::Defer, Outcome::Deferred),
        (Step::Cutover, F::Fail, Outcome::Failure),
        (Step::Verify, F::Fail, Outcome::Failure),
        (Step::Cleanup, F::Fail, Outcome::Failure),
    ];
    for (n, (faulted, fault, outcome)) in cases.into_iter().enumerate() {
        let bed = Bed::update(&format!("hook-runs-clear-{n}"));
        let at = SUCCESS.iter().position(|s| *s == faulted).unwrap();
        for step in &SUCCESS[..at] {
            bed.ok(*step);
        }
        bed.arrange(faulted, fault);
        let first = bed.run(faulted, 0);
        assert_eq!(first.outcome(), outcome, "{faulted} {fault:?}: {first:?}");
        let after_first = bed.state();
        let again = bed.run(faulted, 0);
        assert_eq!(again.outcome(), outcome);
        assert_eq!(
            again.report.as_ref().map(|r| (&r.reason, r.reversible)),
            first.report.as_ref().map(|r| (&r.reason, r.reversible)),
        );
        same(&after_first, &bed.state(), &format!("{faulted} run again"));
        bed.clear(faulted);
        for step in &SUCCESS[at..] {
            bed.ok(*step);
        }
        same(
            &want,
            &without_stamps(bed.state()),
            &format!("{faulted} {fault:?} cleared"),
        );
    }
}

/// `rollback` killed at every kill point after a `prepare` or a `cutover` that was itself
/// killed at each of its kill points, then run to its end, reaches the world and the record of
/// an uninterrupted rollback from the same cut-short step.
#[test]
fn rollback_killed_at_every_kill_point_after_a_cut_short_step_reaches_its_uninterrupted_end() {
    let mut swept = Vec::new();
    for (path, cut) in [
        (&[][..], Step::Prepare),
        (&[Step::Prepare][..], Step::Cutover),
    ] {
        let gates = {
            let bed = Bed::update(&format!("hook-runs-cut-{cut}-n"));
            for step in path {
                bed.ok(*step);
            }
            bed.ok(cut).points
        };
        for cut_at in 1..=gates {
            let name = format!("hook-runs-cut-{cut}-{cut_at}");
            let setup = |bed: &Bed| {
                for step in path {
                    bed.ok(*step);
                }
                bed.run(cut, cut_at);
            };
            let whole = Bed::update(&format!("{name}-whole"));
            setup(&whole);
            let baseline = whole.run(Step::Rollback, 0);
            assert_eq!(
                baseline.outcome(),
                Outcome::Success,
                "{cut} cut at {cut_at}: the uninterrupted rollback: {baseline:?}"
            );
            let rollback_points = baseline.points;
            swept.push((cut, cut_at, rollback_points));
            let want = whole.state();
            for kill_at in 1..=rollback_points {
                let bed = Bed::update(&format!("{name}-{kill_at}"));
                setup(&bed);
                bed.run(Step::Rollback, kill_at);
                let ran = bed.run(Step::Rollback, 0);
                assert_eq!(
                    ran.outcome(),
                    Outcome::Success,
                    "{cut}@{cut_at} rollback@{kill_at}"
                );
                same(
                    &want,
                    &bed.state(),
                    &format!("{cut} cut at {cut_at}, rollback killed at {kill_at}"),
                );
            }
        }
    }
    // Both cut-short steps were cut at each of their points, and the rollbacks after them were
    // killed at points of their own (a rollback after a `prepare` cut before its record was
    // written has nothing to undo, and no point).
    assert!(
        swept.iter().any(|(cut, ..)| *cut == Step::Prepare),
        "{swept:?}"
    );
    assert!(
        swept.iter().any(|(cut, ..)| *cut == Step::Cutover),
        "{swept:?}"
    );
    for cut in [Step::Prepare, Step::Cutover] {
        let total: u64 = swept.iter().filter(|(c, ..)| *c == cut).map(|s| s.2).sum();
        assert!(total > 0, "{cut}: {swept:?}");
    }
}

/// On a host whose last job ends while `prepare` waits, `prepare` renews the cordon it placed.
/// Killed at each of its kill points, the world's view of the record shows the cordon owned at
/// every point where the record holds it confirmed, so the only point where a cordon is there
/// and not owned is the one right after it was placed: the crash window the declared allowance
/// excuses, and no renewal or later point.
#[test]
fn on_a_renewing_host_only_the_point_after_the_placing_shows_a_cordon_not_owned() {
    use super::fakes::harness::OWNED_CORDON;
    use js_agent::update_hook::Record;
    let gates = points_of("hook-runs-renew-n", AgentHost::draining(), &[Step::Prepare])[0];
    let mut unowned = Vec::new();
    let mut owned = 0;
    for kill_at in 1..=gates {
        let bed = Bed::new(&format!("hook-runs-renew-{kill_at}"), AgentHost::draining());
        bed.run(Step::Prepare, kill_at);
        let cordon = bed.sandbox.world().join("cordon.json").exists();
        let view = bed.sandbox.world().join(OWNED_CORDON).exists();
        let confirmed = fs::read(bed.sandbox.record().join("record.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            .is_some_and(|record| record.cordon.placed && !record.cordon.provisional);
        assert_eq!(view, confirmed, "kill point {kill_at}");
        if cordon && !view {
            unowned.push(kill_at);
        }
        owned += u64::from(cordon && view);
    }
    assert_eq!(unowned.len(), 1, "{unowned:?} of {gates}");
    assert!(owned > 1, "the renewals: {owned} owned points of {gates}");
}

/// A `rollback` that fails while the lease server does not answer fails the same way when run
/// again, changing nothing more; once the server answers, it runs to the end the uninterrupted
/// rollback reaches.
#[test]
fn a_rollback_that_fails_repeats_then_resumes_once_the_fault_is_gone() {
    let whole = Bed::update("hook-runs-rb-clear-whole");
    whole.ok(Step::Prepare);
    whole.ok(Step::Cutover);
    whole.ok(Step::Rollback);
    let want = without_stamps(whole.state());
    let bed = Bed::update("hook-runs-rb-clear");
    bed.ok(Step::Prepare);
    bed.ok(Step::Cutover);
    bed.arrange(Step::Rollback, harness::Fault::Fail);
    let first = bed.run(Step::Rollback, 0);
    assert_eq!(first.outcome(), Outcome::Failure, "{first:?}");
    let report = first.report.as_ref().unwrap();
    assert_eq!(report.reason.as_deref(), Some("lease_unreachable"));
    let after_first = bed.state();
    let again = bed.run(Step::Rollback, 0);
    assert_eq!(again.outcome(), Outcome::Failure);
    assert_eq!(
        again.report.as_ref().map(|r| (&r.reason, r.reversible)),
        Some((&report.reason, report.reversible))
    );
    same(&after_first, &bed.state(), "a failed rollback run again");
    bed.clear(Step::Rollback);
    bed.ok(Step::Rollback);
    same(&want, &without_stamps(bed.state()), "rollback resumed");
}

/// A watchdog that fires in a binary started in its caller's process group kills only the
/// binary: the test that started it is alive to see it die of SIGKILL. The stuck call announces
/// itself, so the watchdog fires without a clock.
#[test]
fn a_watchdog_in_its_callers_process_group_kills_only_the_binary() {
    use super::fakes::harness::Defect;
    use std::os::unix::process::ExitStatusExt;
    let bed = Bed::new(
        "hook-runs-callers-group",
        AgentHost::with_defect(Defect::Stall).on_stall(),
    );
    bed.ok(Step::Prepare);
    bed.ok(Step::Cutover);
    let mut command = Command::new(BIN);
    let status = command
        .args(["update-hook", "verify", "--params"])
        .arg(bed.sandbox.control().join("params.toml"))
        .env_clear()
        .envs(bed.host.env(&bed.sandbox))
        .current_dir(bed.sandbox.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.signal(), Some(9), "{status:?}");
    // This test still runs: its group was not signalled.
    assert!(bed.sandbox.root().exists());
}

/// A step whose caller is gone, and the caller's end of its standard error with it, is still
/// ended by its watchdog: the message the watchdog writes fails, and the kill goes on. The stuck
/// call announces itself, so the watchdog fires without a clock. The wait for the binary is
/// bounded by a count of looks (it is a guard against a binary that never ends, not a measure
/// of how long it takes). Catches a watchdog whose write to a closed pipe panics its thread and
/// leaves the step parked for good, as a test process that died left its steps.
#[test]
fn a_watchdog_whose_caller_closed_its_standard_error_still_kills_the_step() {
    use super::fakes::harness::Defect;
    use std::os::unix::process::ExitStatusExt;
    let bed = Bed::new(
        "hook-runs-closed-stderr",
        AgentHost::with_defect(Defect::Stall).on_stall(),
    );
    bed.ok(Step::Prepare);
    bed.ok(Step::Cutover);
    let mut child = Command::new(BIN)
        .args(["update-hook", "verify", "--params"])
        .arg(bed.sandbox.control().join("params.toml"))
        .env_clear()
        .envs(bed.host.env(&bed.sandbox))
        .current_dir(bed.sandbox.root())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    let mut status = None;
    for _ in 0..1200 {
        status = child.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the step outlived its watchdog with no one reading its standard error");
    };
    assert_eq!(status.signal(), Some(9), "{status:?}");
}

/// On a host whose last job drained while `prepare` waited, `prepare` run again asks the lease
/// server for its cordon again, which moves the cordon's expiry later: the run-twice check reads
/// that as a change. This pins it as it is, the cordon's expiry and the record's, nothing else,
/// the cordon still the hook's and never shortened, so a change to it shows here; whether the
/// hook should skip the re-ask is a question for the hook, not this suite.
#[test]
fn a_second_prepare_on_a_host_it_waited_on_moves_only_its_own_cordons_expiry() {
    use js_agent::update_hook::{CordonView, Record};
    let bed = Bed::new("hook-runs-drain-twice", AgentHost::draining());
    bed.ok(Step::Prepare);
    let before = bed.state();
    let cordon = |bed: &Bed| -> CordonView {
        serde_json::from_slice(&fs::read(bed.sandbox.world().join("cordon.json")).unwrap()).unwrap()
    };
    let record = |bed: &Bed| -> Record {
        serde_json::from_slice(&fs::read(bed.sandbox.record().join("record.json")).unwrap())
            .unwrap()
    };
    let (first_cordon, first_record) = (cordon(&bed), record(&bed));
    bed.ok(Step::Prepare);
    assert_eq!(
        changes(&before, &bed.state()),
        ["changed cordon.json", "changed record/record.json"]
    );
    let (second_cordon, second_record) = (cordon(&bed), record(&bed));
    assert!(second_cordon.expires_at_ms > first_cordon.expires_at_ms);
    assert!(second_record.cordon.placed && !second_record.cordon.provisional);
    assert_eq!(second_record.cordon.first_ms, first_record.cordon.first_ms);
    assert_eq!(second_record.cordon.expires_ms, second_cordon.expires_at_ms);
    let mut same_but_cordon = second_record.clone();
    same_but_cordon.cordon = first_record.cordon;
    assert_eq!(
        same_but_cordon, first_record,
        "nothing else in the record moved"
    );
}

/// On that host, `prepare` killed at each of its kill points and every step then run to its
/// end finishes the update: every step succeeds, the record is gone, and no cordon is left but
/// the one a crash right after the placing leaves to lapse (the declared allowance).
#[test]
fn on_a_host_that_drains_while_prepare_waits_a_killed_prepare_resumes_to_a_finished_update() {
    let gates = points_of(
        "hook-runs-drain-resume-n",
        AgentHost::draining(),
        &[Step::Prepare],
    )[0];
    assert!(gates > 2, "{gates}");
    let mut lapsing = 0;
    for kill_at in 0..=gates {
        let bed = Bed::new(
            &format!("hook-runs-drain-resume-{kill_at}"),
            AgentHost::draining(),
        );
        bed.run(Step::Prepare, kill_at);
        for step in SUCCESS {
            bed.ok(step);
        }
        assert!(
            !bed.sandbox.record().join("record.json").exists(),
            "{kill_at}"
        );
        lapsing += u32::from(bed.sandbox.world().join("cordon.json").exists());
    }
    assert!(lapsing <= 1, "{lapsing} kill points left a cordon");
}
