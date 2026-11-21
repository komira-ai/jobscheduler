//! An update that hands the runs over in place: `prepare` decides it when both agents list
//! `in-place` (on any systemd) and otherwise hands over by stop and start only on a systemd older
//! than 258, and drains everywhere else; `cutover` and `rollback` write the handover file and
//! signal the unit's main process (SIGHUP) instead of stopping and starting it, and wait until the
//! agent has taken the request; and a start that fails says what is installed and that no agent
//! runs.

use std::path::PathBuf;

use js_agent::lifecycle::handover::HANDOVER_FILE;
use js_agent::update_hook::{
    CUTOVER_MARGIN_SECS, Class, Outcome, Reason, START_WAIT_SECS, Units, validate,
};
use serde_json::json;

use super::step_rig::{
    OnSignal, Rig, SYSTEMD_257, SYSTEMD_258, Spy, UNIT, end_state, killable, line, marks,
};

/// What an agent that hands over in place prints to `--version`.
const IN_PLACE: &str = "js-agent 1.4.0\nruntimes: docker crun\nstops: drain handover in-place\n";
/// What an agent that hands over only by being stopped prints.
const STOPS: &str = "js-agent 1.4.0\nruntimes: docker crun\nstops: drain handover\n";

const REQUEST: &str = "handover.request";
const DRAINS: &str = "the cutover drains the host first: ";
const OWN_CGROUP: &str = "the host's systemd starts a service's first process in the unit's own \
     cgroup (258 or newer, or its version cannot be read), which the jobs a stop leaves keep from \
     taking one, and the previous and staged agents do not both hand over in place (no `in-place` \
     on their `stops:` line)";
const IN_PLACE_NOTE: &str = "the cutover hands the running jobs over in place (the unit is not \
     stopped): no cordon and no drain";
const STOP_START_NOTE: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";

fn kill() -> String {
    format!("systemctl.kill --kill-whom=main --signal=SIGHUP -- {UNIT}")
}

fn start() -> String {
    format!("units.start({UNIT})")
}

fn stop() -> String {
    format!("units.stop({UNIT})")
}

fn file(rig: &Rig) -> PathBuf {
    rig.state_root().path().join(HANDOVER_FILE)
}

/// A spy whose staged and previous agents print `staged` and `previous`, over the unit
/// manager that prints `systemd`.
fn agents<'a>(rig: &'a Rig, previous: &str, staged: &str, systemd: Result<&str, &str>) -> Spy<'a> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(staged.to_owned());
    *spy.previous_version.borrow_mut() = Some(Ok(previous.to_owned()));
    *spy.systemd.borrow_mut() = systemd.map(str::to_owned).map_err(str::to_owned);
    spy
}

/// A busy host whose `prepare` chose to hand over in place, with the agent running.
fn in_place(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let done = agents(&rig, IN_PLACE, IN_PLACE, Ok(SYSTEMD_258)).prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(rig.record().unwrap().in_place);
    rig.kill.disarm();
    rig
}

/// An in-place update that has been cut over and verified.
fn cut_over(name: &str) -> Rig {
    let rig = in_place(name);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(Spy::new(&rig).verify(&rig).outcome, Outcome::Success);
    rig.kill.disarm();
    rig
}

/// Asserts that `prepare` hands the runs over in place, with no cordon and no drain and the
/// note that says so, whatever systemd prints (257, 258, a number that cannot be read, no
/// answer), when both the previous and the staged agent list `in-place`, and records it.
/// Catches an in-place update refused on a new systemd, which is the one that needs it.
#[test]
fn prepare_hands_over_in_place_when_both_agents_list_it_on_any_systemd() {
    let answers: [Result<&str, &str>; 4] = [
        Ok(SYSTEMD_257),
        Ok(SYSTEMD_258),
        Ok("not systemd at all"),
        Err("systemctl is not there"),
    ];
    for (k, answer) in answers.into_iter().enumerate() {
        let rig = Rig::new(&format!("hook-in-place-prepare-{k}"));
        rig.world.seed_unit(&rig.unit, "active", "running");
        rig.set_leases(2);
        let done = agents(&rig, IN_PLACE, IN_PLACE, answer).prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{done:?}");
        assert_eq!(done.report.detail.as_deref(), Some(IN_PLACE_NOTE), "{k}");
        let record = rig.record().unwrap();
        assert!(record.handover && record.in_place, "{k}");
        assert!(!record.cordon.placed, "{k}");
        assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    }
}

/// Asserts that an update hands the runs over by stop and start on a systemd older than 258
/// when one of the two agents lacks `in-place`, and records no `in-place`. Catches a systemd
/// that stops the unit being asked for an in-place hand-over by an agent that cannot.
#[test]
fn prepare_hands_over_by_stop_and_start_on_systemd_257_unless_both_agents_list_in_place() {
    for (name, previous, staged) in [
        ("neither", STOPS, STOPS),
        ("previous-only", IN_PLACE, STOPS),
        ("staged-only", STOPS, IN_PLACE),
    ] {
        let rig = Rig::new(&format!("hook-in-place-stop-start-{name}"));
        rig.world.seed_unit(&rig.unit, "active", "running");
        rig.set_leases(2);
        let done = agents(&rig, previous, staged, Ok(SYSTEMD_257)).prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(
            done.report.detail.as_deref(),
            Some(STOP_START_NOTE),
            "{name}"
        );
        let record = rig.record().unwrap();
        assert!(record.handover, "{name}");
        assert!(!record.in_place, "{name}");
    }
}

/// Asserts that an update drains, naming the systemd cause in full, on a systemd 258 or newer
/// and on one whose version cannot be read, when the two agents do not both list `in-place`; and
/// that the host that drains is cordoned and the record says no hand-over. Catches a stop and
/// start of a unit whose jobs keep a systemd 258 from starting it.
#[test]
fn prepare_drains_on_systemd_258_or_an_unreadable_version_unless_both_agents_list_in_place() {
    let cases: [(&str, &str, &str, Result<&str, &str>); 6] = [
        ("258-neither", STOPS, STOPS, Ok(SYSTEMD_258)),
        ("258-previous-only", IN_PLACE, STOPS, Ok(SYSTEMD_258)),
        ("258-staged-only", STOPS, IN_PLACE, Ok(SYSTEMD_258)),
        ("newer", STOPS, STOPS, Ok("systemd 301 (301.1)\n")),
        ("garbage", STOPS, STOPS, Ok("something else\n")),
        ("no-systemctl", STOPS, STOPS, Err("no systemctl")),
    ];
    for (name, previous, staged, answer) in cases {
        let rig = Rig::new(&format!("hook-in-place-drain-{name}"));
        rig.world.seed_unit(&rig.unit, "active", "running");
        let spy = agents(&rig, previous, staged, answer);
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        assert_eq!(
            done.report.detail,
            Some(format!("{DRAINS}{OWN_CGROUP}")),
            "{name}"
        );
        let record = rig.record().unwrap();
        assert!(!record.handover && !record.in_place, "{name}");
        assert!(record.cordon.placed, "{name}");
        let asked = spy
            .log()
            .iter()
            .filter(|call| call.ends_with("systemctl --version)"))
            .count();
        assert_eq!(asked, 1, "{name}: the version is read once");
    }
}

/// Asserts that the in-place cutover of a busy host writes the request, then marks the link
/// and the start, then signals the main process, in that order, with the marks on disk at each
/// act; that it stops and starts nothing, leaves the unit active and the jobs running, waits for
/// nothing, and ends with the request taken and a new session. Catches a cutover that stops the
/// unit (which the hand-over in place exists to avoid), one that marks the start after the
/// signal, and one that signals before the request is on disk.
#[test]
fn the_in_place_cutover_writes_the_request_marks_and_signals_without_stopping_the_unit() {
    let rig = in_place("hook-in-place-cutover");
    let session = rig.host().session_started_ms;
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(line(&done)["reversible"], json!(false));
    let acts: Vec<(String, [bool; 3])> = spy
        .acts
        .borrow()
        .iter()
        .map(|act| (act.call.clone(), act.marks))
        .collect();
    assert_eq!(
        acts,
        [
            (REQUEST.to_owned(), [true, false, false]),
            (kill(), [true, true, true]),
        ]
    );
    assert_eq!(rig.record().unwrap().pre_stop_session_ms, session);
    assert_eq!(marks(&rig), [true, true, true]);
    assert!(!file(&rig).exists(), "the agent took the request");
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
    assert_ne!(rig.host().session_started_ms, session, "a new session");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
}

/// Asserts that an in-place cutover killed at any kill point and run again ends as an
/// uninterrupted one, with no request left, and that a stop is never issued. Catches a rerun
/// that signals with no request on disk, or that does the hand-over twice.
#[test]
fn an_in_place_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let baseline = in_place("hook-in-place-sweep-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    assert!(
        points >= 4,
        "the request, the marks and the signal: {points}"
    );
    let end = end_state(&baseline);
    for k in 1..=points {
        let rig = in_place(&format!("hook-in-place-sweep-{k}"));
        rig.kill.arm(k);
        assert!(
            killable(|| Spy::new(&rig).cutover(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let rerun = Spy::new(&rig);
        let done = rerun.cutover(&rig);
        assert_eq!(done.outcome, Outcome::Success, "killed at point {k}");
        assert_eq!(end_state(&rig), end, "killed at point {k}");
        assert!(!file(&rig).exists(), "killed at point {k}");
        assert!(
            !rerun.act_names().contains(&stop()) && !rerun.act_names().contains(&start()),
            "killed at point {k}"
        );
    }
}

/// Asserts that a cutover killed while it waits for the agent to take the request leaves
/// the request on disk and the marks set, that the rerun signals again and waits, and that a run
/// after that does nothing. Catches a rerun that treats a request still on disk as taken.
#[test]
fn a_cutover_rerun_after_the_signal_signals_again_while_the_request_is_there() {
    let rig = in_place("hook-in-place-rerun");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ignores);
    spy.on_pause(|_, ticks| assert!(ticks < 3, "killed during the wait"));
    assert!(killable(|| spy.cutover(&rig)).is_none());
    assert_eq!(spy.act_names(), [REQUEST, &kill()]);
    assert!(file(&rig).exists());
    assert_eq!(marks(&rig), [true, true, true]);

    let rerun = Spy::new(&rig);
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [kill()]);
    assert!(!file(&rig).exists());

    let again = Spy::new(&rig);
    assert_eq!(again.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(again.act_names(), Vec::<String>::new());
}

/// Asserts that a signal the unit manager refuses fails the cutover as an environment matter
/// with the request left on disk, and that the rerun, finding it, signals again and ends as an
/// uninterrupted cutover; the same for a rollback. Catches a rerun that reads a request the hook
/// withdrew as one the agent took, and reports an update done while the old agent still runs.
#[test]
fn a_signal_the_unit_manager_refuses_leaves_the_request_and_the_rerun_signals_again() {
    let rig = in_place("hook-in-place-refused");
    let spy = Spy::new(&rig);
    spy.fail("kill");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(format!("cannot signal {UNIT} to hand its runs over in place: kill is down").as_str())
    );
    assert!(!done.report.reversible);
    assert_eq!(spy.act_names(), [REQUEST, &kill()]);
    assert!(file(&rig).exists());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));

    let rerun = Spy::new(&rig);
    let done = rerun.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), [kill()]);
    assert!(!file(&rig).exists());
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.4.0"));

    let rig = cut_over("hook-in-place-rb-refused");
    let spy = Spy::new(&rig);
    spy.fail("kill");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(spy.act_names(), [REQUEST, &kill()]);
    assert!(file(&rig).exists());
    let rerun = Spy::new(&rig);
    let done = rerun.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), [kill()]);
    assert!(rig.record().is_none());
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
}

/// Asserts that a request that cannot be looked at ends the wait as an environment failure that
/// names it. Catches a wait that reads an unreadable state root as a taken request.
#[test]
fn a_request_that_cannot_be_looked_at_fails_the_cutover_as_an_environment_matter() {
    let rig = in_place("hook-in-place-unreadable");
    let spy = Spy::new(&rig);
    spy.fail("pending");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot look at the handover request: pending is down")
    );
}

/// Asserts that an agent that ends instead of running again is started by the cutover as a start
/// is, and succeeds when the unit comes up; and that the same agent whose unit then fails ends
/// the step as a failure of the release that says the staged release is installed and its agent
/// is not running. Catches a cutover that reports success over a unit that is down, and one that
/// leaves the operator guessing what runs.
#[test]
fn an_agent_that_ends_instead_of_running_again_is_started_and_judged_as_a_start() {
    let rig = in_place("hook-in-place-ends");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ends);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [REQUEST, &kill(), &start()]);
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");

    let rig = in_place("hook-in-place-ends-and-fails");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ends);
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{UNIT} is failed after it was started; the staged release 1.4.0 is installed and its agent is not running (systemd retries the unit: Restart=always)"
            )
            .as_str()
        )
    );
    assert!(!done.report.reversible);
}

/// Asserts that a request the agent has not taken in the cutover's bound fails the step as an
/// environment matter that says so and leaves the request for the agent. Catches a wait with no
/// end, and a request withdrawn from an agent that may still take it.
#[test]
fn a_request_not_taken_within_the_bound_fails_the_cutover_as_an_environment_matter() {
    let rig = in_place("hook-in-place-not-taken");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ignores);
    let done = spy.cutover(&rig);
    let bound = rig.params.settings.timeouts.cutover - START_WAIT_SECS - CUTOVER_MARGIN_SECS;
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "the agent of {UNIT} has not handed over in place in {bound} s: the request is left for it"
            )
            .as_str()
        )
    );
    assert!(file(&rig).exists(), "the request is left for the agent");
    assert!(!done.report.reversible);
}

/// Asserts that a unit that is not up when an in-place cutover begins has no agent to hand over:
/// the cutover starts it, with no request and no signal. Catches a signal sent to nothing.
#[test]
fn an_in_place_cutover_of_a_unit_that_is_down_starts_it_with_no_signal() {
    let rig = in_place("hook-in-place-down");
    rig.world.seed_unit(UNIT, "inactive", "dead");
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts = spy.act_names();
    assert!(acts.contains(&start()), "{acts:?}");
    assert!(
        !acts.contains(&kill()) && !acts.contains(&REQUEST.to_owned()) && !acts.contains(&stop()),
        "{acts:?}"
    );
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
}

/// Asserts that the cutover that stops and starts says, when the start fails, that the staged
/// release is installed and its agent is not running, in full. Catches a report that does not
/// say what the node runs.
#[test]
fn a_failed_start_of_a_stop_and_start_cutover_says_no_agent_runs() {
    let rig = Rig::new("hook-in-place-start-fails");
    rig.world.seed_unit(&rig.unit, "active", "running");
    let spy = agents(&rig, STOPS, STOPS, Ok(SYSTEMD_257));
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert!(!rig.record().unwrap().in_place);
    let cut = Spy::new(&rig);
    *cut.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = cut.cutover(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{UNIT} is failed after it was started; the staged release 1.4.0 is installed and its agent is not running (systemd retries the unit: Restart=always)"
            )
            .as_str()
        )
    );
}

/// Asserts that the in-place rollback hands the runs back to the previous agent: the marks of
/// the stop and the link, the request, the mark of the start and the signal, in that order and
/// with the marks on disk at each act; no stop and no start; the previous agent's hello on a new
/// session; and the record gone. Catches a rollback that stops the new agent, and one that
/// signals before the start is marked.
#[test]
fn the_in_place_rollback_hands_the_runs_back_without_stopping_the_unit() {
    let rig = cut_over("hook-in-place-rollback");
    let session = rig.host().session_started_ms;
    let ticks = rig.world.ticks();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let acts: Vec<(String, [bool; 5])> = spy
        .acts
        .borrow()
        .iter()
        .map(|act| (act.call.clone(), act.rb))
        .collect();
    assert_eq!(
        acts,
        [
            (REQUEST.to_owned(), [false, true, true, false, false]),
            (kill(), [false, true, true, true, false]),
        ]
    );
    assert!(rig.record().is_none());
    assert!(!file(&rig).exists());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert_ne!(rig.host().session_started_ms, session);
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.world.ticks(), ticks, "nothing is waited for");
}

/// Asserts that an in-place rollback killed at any kill point and run again ends with the
/// previous agent back, no request left and no stop or start. Catches a rerun that waits for a
/// hello of an agent that was never signalled.
#[test]
fn an_in_place_rollback_killed_at_any_point_and_run_again_ends_with_the_previous_agent() {
    let baseline = cut_over("hook-in-place-rb-sweep-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points >= 5, "{points}");
    for k in 1..=points {
        let rig = cut_over(&format!("hook-in-place-rb-sweep-{k}"));
        rig.kill.arm(rig.kill.points() + k);
        assert!(
            killable(|| Spy::new(&rig).rollback(&rig)).is_none(),
            "point {k}"
        );
        rig.kill.disarm();
        let rerun = Spy::new(&rig);
        let done = rerun.rollback(&rig);
        assert_eq!(done.outcome, Outcome::Success, "point {k}: {done:?}");
        assert!(rig.record().is_none(), "point {k}");
        assert!(!file(&rig).exists(), "point {k}");
        assert_eq!(
            rig.host().agent_version.as_deref(),
            Some("1.3.0"),
            "point {k}"
        );
        let acts = rerun.act_names();
        assert!(
            !acts.contains(&stop()) && !acts.contains(&start()),
            "point {k}: {acts:?}"
        );
    }
}

/// Asserts that a rollback killed while it waits for the previous agent to take the request
/// is run again by signalling again, with no second request, and then waits for the hello.
/// Catches a rerun that goes to the hello of an agent that has not taken the request.
#[test]
fn a_rollback_rerun_after_its_signal_signals_again_while_the_request_is_there() {
    let rig = cut_over("hook-in-place-rb-rerun");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ignores);
    let base = rig.world.ticks();
    spy.on_pause(move |_, ticks| assert!(ticks < base + 3, "killed during the wait"));
    assert!(killable(|| spy.rollback(&rig)).is_none());
    assert_eq!(spy.act_names(), [REQUEST, &kill()]);
    assert!(file(&rig).exists());

    let rerun = Spy::new(&rig);
    let done = rerun.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rerun.act_names(), [kill()]);
    assert!(rig.record().is_none());
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
}

/// Asserts that an in-place rollback whose previous agent ends instead of running again, and
/// whose unit then fails to start, says that the previous release is installed again and that
/// its agent is not running, in full; and that so does a rollback that stops and starts. Catches
/// a report that leaves the operator believing the host runs the old agent.
#[test]
fn a_failed_start_of_the_previous_agent_says_the_update_is_undone_on_disk_only() {
    let said = format!(
        "{UNIT} is failed after it was started; the previous release 1.3.0 is installed again (the release link is on it) but its agent is not running, so the update is undone on disk but the host runs no agent until the unit starts"
    );
    let rig = cut_over("hook-in-place-rb-fails");
    let spy = Spy::new(&rig);
    spy.on_signal.set(OnSignal::Ends);
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.class, Class::Release);
    assert_eq!(done.report.detail.as_deref(), Some(said.as_str()));
    assert!(!done.report.reversible);

    let rig = Rig::new("hook-in-place-rb-fails-stop-start");
    rig.world.seed_unit(&rig.unit, "active", "running");
    assert_eq!(
        agents(&rig, STOPS, STOPS, Ok(SYSTEMD_257))
            .prepare(&rig)
            .outcome,
        Outcome::Success
    );
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let spy = Spy::new(&rig);
    *spy.start_leaves.borrow_mut() = Some(("failed", "failed"));
    let done = spy.rollback(&rig);
    assert_eq!(done.report.reason, Some(Reason::StartFailed));
    assert_eq!(done.report.detail.as_deref(), Some(said.as_str()));
}

/// Asserts the record's `in_place` field is additive: left out of the file unless set, read
/// back as false from a record without it (one an earlier hook wrote), written and read back as
/// true for an update that hands over in place; and that the rules refuse a record that says
/// in-place without a hand-over and accept one that says both. Catches a record an earlier hook
/// can no longer read, and an in-place record of an update that drains.
#[test]
fn the_record_carries_in_place_only_when_set_and_the_rules_need_a_handover_for_it() {
    let drains = Rig::new("hook-in-place-record-drains");
    drains.world.seed_unit(&drains.unit, "active", "running");
    assert_eq!(
        agents(&drains, STOPS, STOPS, Ok(SYSTEMD_258))
            .prepare(&drains)
            .outcome,
        Outcome::Success
    );
    let text: serde_json::Value = serde_json::from_slice(&drains.record_bytes().unwrap()).unwrap();
    assert!(text.get("in_place").is_none(), "{text}");
    assert!(text.get("handover").is_none(), "{text}");

    let hands = Rig::new("hook-in-place-record-stop-start");
    hands.world.seed_unit(&hands.unit, "active", "running");
    assert_eq!(
        agents(&hands, STOPS, STOPS, Ok(SYSTEMD_257))
            .prepare(&hands)
            .outcome,
        Outcome::Success
    );
    let text: serde_json::Value = serde_json::from_slice(&hands.record_bytes().unwrap()).unwrap();
    assert_eq!(text["handover"], json!(true));
    assert!(text.get("in_place").is_none(), "{text}");

    let rig = in_place("hook-in-place-record");
    let text: serde_json::Value = serde_json::from_slice(&rig.record_bytes().unwrap()).unwrap();
    assert_eq!(text["in_place"], json!(true));
    let record = rig.record().unwrap();
    assert!(record.in_place);
    assert_eq!(validate(&record, &rig.params), Ok(()));

    let mut broken = record.clone();
    broken.handover = false;
    let error = validate(&broken, &rig.params).unwrap_err();
    assert_eq!(error.to_string(), "in_place is set but handover is not");
}
