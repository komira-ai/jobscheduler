//! An update that hands the agent's runs over to the new agent instead of draining the host:
//! `prepare` decides it once (a crun host on systemd, the unit unchanged, no egress ceiling in
//! the flags, and a previous agent whose `--version` lists the handover) and records it;
//! `prepare` then places no cordon and waits for nothing; `cutover` and `rollback` write the
//! handover file before they stop the unit, remove it once the unit is down, and never wait for
//! the host to be idle. Every other host drains as before.

use std::path::PathBuf;

use js_agent::lifecycle::handover::{HANDOVER_FILE, HANDOVER_TEXT};
use js_agent::update_hook::{Class, Lease, Outcome, Reason, Units};
use serde_json::json;

use super::step_rig::{Rig, Spy, UNIT, end_state, killable, line, marks};

/// What an agent that takes the handover prints to `--version`.
const TAKES: &str = "js-agent 1.4.0\nruntimes: docker crun\nstops: drain handover\n";

fn stop() -> String {
    format!("units.stop({UNIT})")
}

fn start() -> String {
    format!("units.start({UNIT})")
}

const REQUEST: &str = "handover.request";
const WITHDRAW: &str = "handover.withdraw";

/// A spy over `rig` whose agents, the previous release's among them, take the handover.
fn spy(rig: &Rig) -> Spy<'_> {
    let spy = Spy::new(rig);
    *spy.version.borrow_mut() = Ok(TAKES.to_owned());
    spy
}

/// The handover file of the agent's state root.
fn file(rig: &Rig) -> PathBuf {
    rig.state_root().path().join(HANDOVER_FILE)
}

/// A busy host (two leases) as `prepare` leaves it for an update that hands over.
fn handing(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let done = spy(&rig).prepare(&rig);
    assert_eq!(
        done.outcome,
        Outcome::Success,
        "prepare of the setup: {done:?}"
    );
    rig.kill.disarm();
    rig
}

/// Asserts that `prepare` of a busy crun host whose agent takes the handover records the
/// handover, places no cordon, waits for nothing and says so; that it reads the previous
/// release's agent for it; and that a second run changes nothing. Catches an update that still
/// cordons and drains a busy host for an hour, and one that hands over without asking the
/// agent that would read the request.
#[test]
fn prepare_of_a_busy_host_whose_agent_hands_over_places_no_cordon_and_waits_for_nothing() {
    let rig = Rig::new("hook-handover-prepare");
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let spy = spy(&rig);
    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        done.report.detail.as_deref(),
        Some("the cutover hands the running jobs over to the new agent: no cordon and no drain")
    );
    let record = rig.record().unwrap();
    assert!(record.handover);
    assert!(!record.cordon.placed);
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    let previous = rig.params.previous_release.clone().unwrap();
    let asked = format!("run({}/bin/js-agent --version)", previous.display());
    assert!(spy.log().contains(&asked), "{:?}", spy.log());
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
    assert!(!file(&rig).exists(), "prepare asks nothing of the agent");

    let again = self::spy(&rig);
    assert_eq!(again.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record().unwrap(), record);
}

/// Asserts that every host that cannot hand over drains as before (the record says no
/// handover, and the cordon is placed): a previous agent from before the handover (its
/// `--version` has no `stops:` line, or one without the handover), flags with an egress
/// ceiling, a staged unit that differs, the container engine's runtime, and a Mac. Catches a
/// handover asked of an agent that would drain on it, or of a host whose next agent removes the
/// runs it was to adopt.
#[test]
fn every_host_that_cannot_hand_over_drains_as_before() {
    type Setup = fn(&Rig, &Spy<'_>);
    type Case = (&'static str, fn(&str) -> Rig, Setup);
    let cases: [Case; 7] = [
        ("old-agent", Rig::new, |_, spy| {
            *spy.version.borrow_mut() = Ok("js-agent 1.4.0\nruntimes: docker crun\n".to_owned());
        }),
        ("drain-only", Rig::new, |_, spy| {
            *spy.version.borrow_mut() = Ok("js-agent 1.4.0\nstops: drain\n".to_owned());
        }),
        ("egress", Rig::new, |_, spy| spy.egress.set(true)),
        ("unit-changed", Rig::new, |rig, _| rig.stage_changed_unit()),
        ("docker", Rig::new, |_, _| {}),
        ("mac", Rig::mac, |_, _| {}),
        ("handover", Rig::new, |_, _| {}),
    ];
    for (name, build, setup) in cases {
        let mut rig = build(&format!("hook-handover-not-{name}"));
        if name == "docker" {
            rig.params.settings.runtime = "docker".to_owned();
        }
        rig.world.seed_unit(&rig.unit, "active", "running");
        let spy = spy(&rig);
        if name == "mac" {
            *spy.version.borrow_mut() =
                Ok("js-agent 1.4.0\nruntimes: native\nstops: drain handover\n".to_owned());
        }
        setup(&rig, &spy);
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Success, "{name}: {done:?}");
        let record = rig.record().unwrap();
        let hands_over = name == "handover";
        assert_eq!(record.handover, hands_over, "{name}");
        assert_eq!(record.cordon.placed, !hands_over, "{name}");
    }
}

/// Asserts the cutover of a busy host that hands over: the stop is marked with the session
/// first, the handover file is written just before the stop, the unit is stopped and is down,
/// the file is removed, the new agent is started; no cordon is asked, no lease is waited for,
/// and the leases are still there. Catches a cutover that still defers on a busy host, one that
/// stops the agent without the request (a drain of up to an hour), and one that leaves the
/// request for a later stop of the new agent.
#[test]
fn the_cutover_of_a_busy_host_asks_for_the_handover_stops_and_starts_without_waiting() {
    let rig = handing("hook-handover-cutover");
    let session = rig.host().session_started_ms;
    let spy = Spy::new(&rig);
    let done = spy.cutover(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(
        line(&done)["reversible"],
        json!(false),
        "the stop is the commit"
    );
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
            (stop(), [true, false, false]),
            (WITHDRAW.to_owned(), [true, false, false]),
            (start(), [true, true, true]),
        ]
    );
    assert_eq!(rig.record().unwrap().pre_stop_session_ms, session);
    assert!(!file(&rig).exists());
    assert_eq!(rig.cordon_expiry(), None);
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_eq!(rig.world.ticks(), 0, "nothing is waited for");
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
}

/// Asserts what the agent finds at the stop: the request is on disk, with its text, when the
/// stop is issued. Catches a request written after the stop, which the agent would never read.
#[test]
fn the_request_is_on_disk_when_the_stop_reaches_the_agent() {
    let rig = handing("hook-handover-at-stop");
    let seen = std::cell::RefCell::new(None);
    let path = file(&rig);
    let spy = Spy::new(&rig);
    *spy.stop_leaves.borrow_mut() = Some(("deactivating", "stop-sigterm"));
    spy.on_pause(|spy, _| {
        if seen.borrow().is_none() {
            *seen.borrow_mut() = Some(std::fs::read_to_string(&path).ok());
        }
        spy.world.seed_unit(UNIT, "inactive", "dead");
    });
    assert_eq!(spy.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(*seen.borrow(), Some(Some(HANDOVER_TEXT.to_owned())));
    assert!(!file(&rig).exists());
}

/// Asserts that a cutover that hands over, killed at any kill point and run again, ends as an
/// uninterrupted one, and that no request is left behind. Catches a rerun that skips the request
/// before a stop it issues again, or that leaves the file for the new agent's next stop.
#[test]
fn a_handover_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    let baseline = handing("hook-handover-sweep-base");
    assert_eq!(
        Spy::new(&baseline).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    assert!(
        points >= 6,
        "the marks, the request, the withdrawal and the acts: {points}"
    );
    let end = end_state(&baseline);
    for k in 1..=points {
        let rig = handing(&format!("hook-handover-sweep-{k}"));
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
        // A stop issued again is asked for again.
        let acts = rerun.act_names();
        if let Some(at) = acts.iter().position(|call| *call == stop()) {
            assert!(
                at > 0 && acts[at - 1] == REQUEST,
                "killed at point {k}: {acts:?}"
            );
        }
    }
}

/// Asserts that a request that cannot be written fails the cutover before the stop, as an
/// environment matter, with the agent still running; and that a rerun once the state root is
/// writable again hands over and ends as an uninterrupted cutover. Catches a stop issued with no
/// request, which drains the busy host for up to an hour.
#[test]
fn a_request_that_cannot_be_written_fails_the_cutover_before_the_stop() {
    let rig = handing("hook-handover-unwritable");
    let spy = Spy::new(&rig);
    spy.fail("handover");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(
        done.report.detail.as_deref(),
        Some("cannot ask the agent to hand its runs over: handover is down")
    );
    assert_eq!(spy.act_names(), [REQUEST]);
    assert_eq!(marks(&rig), [true, false, false]);
    let unit = rig.world.show(UNIT).unwrap();
    assert_eq!(unit.active, "active", "not stopped");

    let rerun = Spy::new(&rig);
    assert_eq!(rerun.cutover(&rig).outcome, Outcome::Success);
    assert_eq!(rerun.act_names(), [REQUEST, &stop(), WITHDRAW, &start()]);
    assert!(!file(&rig).exists());
}

/// Asserts that checks owed while the agents run (a job's post-job check under way, its record
/// in the state root) neither halt nor hold an update that hands over, in `prepare`, `cutover`
/// or `verify`, and that a taint still halts it before the stop. Catches an update that halts or
/// waits for every check of a busy host, and one that stops a tainted agent.
#[test]
fn checks_under_way_do_not_hold_a_handover_and_a_taint_still_halts_it() {
    let rig = Rig::new("hook-handover-owed");
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    rig.state_root().owe("run-7").unwrap();
    assert_eq!(spy(&rig).prepare(&rig).outcome, Outcome::Success);
    assert!(rig.record().unwrap().handover);
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    rig.state_root().owe("run-8").unwrap();
    let done = Spy::new(&rig).verify(&rig);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(rig.world.ticks(), 0, "nothing waited for the checks");

    let tainted = handing("hook-handover-tainted");
    tainted.taint_with("check-owed", None);
    let spy = Spy::new(&tainted);
    let done = spy.cutover(&tainted);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.report.reason, Some(Reason::Tainted));
    assert!(spy.act_names().is_empty());
}

/// Asserts the rollback of a handover that was verified, with the jobs still running: the new
/// agent is asked to hand over and stopped (no drain: no cordon is placed and no lease waited
/// for), the request is removed once it is down, and the previous agent is started and says
/// hello. Catches a rollback that waits for a busy host for up to an hour, or that stops the new
/// agent with no request, which would cut the jobs it adopted.
#[test]
fn the_rollback_of_a_handover_hands_the_runs_back_without_a_drain() {
    let rig = handing("hook-handover-rollback");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    assert_eq!(Spy::new(&rig).verify(&rig).outcome, Outcome::Success);
    let new_session = rig.host().session_started_ms;
    let ticks = rig.world.ticks();
    let spy = Spy::new(&rig);
    let done = spy.rollback(&rig);

    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(spy.act_names(), [REQUEST, &stop(), WITHDRAW, &start()]);
    assert!(spy.log().iter().all(|call| !call.starts_with("cordon(")));
    assert_eq!(rig.world.ticks(), ticks, "nothing is waited for");
    assert!(!file(&rig).exists());
    assert_eq!(rig.host().leases, 2, "the jobs run on");
    assert_ne!(rig.host().session_started_ms, new_session);
    assert_eq!(rig.host().agent_version.as_deref(), Some("1.3.0"));
    assert!(rig.record().is_none(), "the rollback finished");
}

/// Asserts that a rollback of a handover killed at any kill point and run again ends with the
/// previous agent back, no request left behind, and a request before every stop it issues.
/// Catches a rerun that stops the new agent without the request.
#[test]
fn a_handover_rollback_killed_at_any_point_and_run_again_ends_with_the_previous_agent() {
    let setup = |name: &str| {
        let rig = handing(name);
        assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
        rig.kill.disarm();
        rig
    };
    let baseline = setup("hook-handover-rb-sweep-base");
    let before = baseline.kill.points();
    assert_eq!(
        Spy::new(&baseline).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points >= 5, "{points}");
    for k in 1..=points {
        let rig = setup(&format!("hook-handover-rb-sweep-{k}"));
        rig.kill.arm(rig.kill.points() + k);
        assert!(
            killable(|| Spy::new(&rig).rollback(&rig)).is_none(),
            "point {k}"
        );
        rig.kill.disarm();
        let rerun = Spy::new(&rig);
        // An agent started by the killed run says hello once it runs, as a real one would.
        rerun.on_pause(|spy, _| {
            let host = spy.world.host().unwrap();
            if spy.world.is_active(UNIT).unwrap() && host.agent_version.as_deref() != Some("1.3.0")
            {
                spy.world.hello("1.3.0").unwrap();
            }
        });
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
        if let Some(at) = acts.iter().position(|call| *call == stop()) {
            assert!(at > 0 && acts[at - 1] == REQUEST, "point {k}: {acts:?}");
        }
    }
}
