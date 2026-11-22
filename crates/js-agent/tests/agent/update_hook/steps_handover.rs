//! An update that hands the agent's runs over to the new agent instead of draining the host:
//! `prepare` decides it once (a crun host on systemd, the unit unchanged, a previous agent whose
//! `--version` lists the handover, and, on a host with an egress ceiling in the flags, one
//! that lists `egress-handover` too) and records it;
//! `prepare` then places no cordon and waits for nothing; `cutover` and `rollback` write the
//! handover file before they stop the unit, remove it once the unit is down, and never wait for
//! the host to be idle. Every other host drains as before, and the outcome names the cause.

use std::path::PathBuf;

use js_agent::lifecycle::handover::{HANDOVER_FILE, HANDOVER_TEXT};
use js_agent::update_hook::{Class, Lease, Outcome, Reason, Units};
use serde_json::json;

use super::step_rig::{DRAIN_WAIT_SECS, Rig, Spy, UNIT, end_state, killable, line, marks};

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
    handing_on(name, false)
}

/// A spy that reads the host's flags as `egress` says, with agents that keep egress wiring.
fn spy_on(rig: &Rig, egress: bool) -> Spy<'_> {
    let spy = spy(rig);
    spy.egress.set(egress);
    if egress {
        *spy.version.borrow_mut() = Ok(KEEPS_EGRESS.to_owned());
    }
    spy
}

/// [`handing`] on a host whose flags set an egress ceiling when `egress` is true: both agents
/// list `egress-handover`, so the update hands over all the same.
fn handing_on(name: &str, egress: bool) -> Rig {
    let rig = Rig::new(name);
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(2);
    let done = spy_on(&rig, egress).prepare(&rig);
    assert!(rig.record().unwrap().handover);
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

const DRAINS: &str = "the cutover drains the host first: ";
const NO_HANDOVER: &str = "the previous release's agent does not hand its jobs over \
     (no `handover` on its `stops:` line, or it could not be run)";
const NO_KEEP: &str = "the host has an egress ceiling (or its flags file cannot be read) and the \
     previous release's agent does not keep the jobs' egress wiring \
     (no `egress-handover` on its `stops:` line)";
const NO_ADOPT: &str = "the host has an egress ceiling (or its flags file cannot be read) and the \
     staged release's agent does not adopt the jobs' egress wiring \
     (no `egress-handover` on its `stops:` line)";

/// What an agent that keeps and adopts egress wiring prints to `--version`.
const KEEPS_EGRESS: &str =
    "js-agent 1.4.0\nruntimes: docker crun\nstops: drain handover egress-handover\n";

/// Asserts that every host that cannot hand over drains as before (the record says no
/// handover, and the cordon is placed) and that the detail of the success names the one
/// condition that made it drain, in full: a previous agent from before the handover (its
/// `--version` has no `stops:` line, or one without the handover, or cannot be run), a staged
/// unit that differs, the container engine's runtime, and a Mac; and that a host that can hand
/// over says nothing of a drain. Catches a handover asked of an agent that would drain on it,
/// and a drain whose journal line leaves the operator guessing why.
#[test]
fn every_host_that_cannot_hand_over_drains_and_says_why() {
    type Setup = fn(&Rig, &Spy<'_>);
    type Case = (&'static str, fn(&str) -> Rig, Setup, Option<String>);
    let unit = "the unit changes (Service.DelegateSubgroup, Service.ExecStart): cutover starts the agent on the new unit";
    let cases: [Case; 8] = [
        (
            "old-agent",
            Rig::new,
            |_, spy| {
                *spy.version.borrow_mut() =
                    Ok("js-agent 1.4.0\nruntimes: docker crun\n".to_owned());
            },
            Some(format!("{DRAINS}{NO_HANDOVER}")),
        ),
        (
            "drain-only",
            Rig::new,
            |_, spy| {
                *spy.version.borrow_mut() = Ok("js-agent 1.4.0\nstops: drain\n".to_owned());
            },
            Some(format!("{DRAINS}{NO_HANDOVER}")),
        ),
        (
            "previous-agent-fails",
            Rig::new,
            |_, spy| {
                *spy.previous_version.borrow_mut() = Some(Err("exec failed".to_owned()));
            },
            Some(format!("{DRAINS}{NO_HANDOVER}")),
        ),
        (
            "unit-changed",
            Rig::new,
            |rig, _| rig.stage_changed_unit(),
            Some(unit.to_owned()),
        ),
        (
            "docker",
            Rig::new,
            |_, _| {},
            Some(format!("{DRAINS}the jobs do not run on the crun runtime")),
        ),
        (
            "mac",
            Rig::mac,
            |_, _| {},
            Some(format!(
                "{DRAINS}the host does not run the agent under systemd"
            )),
        ),
        ("handover", Rig::new, |_, _| {}, Some(HANDOVER.to_owned())),
        (
            "egress-agents",
            Rig::new,
            |_, spy| {
                spy.egress.set(true);
                *spy.version.borrow_mut() = Ok(KEEPS_EGRESS.to_owned());
            },
            Some(HANDOVER.to_owned()),
        ),
    ];
    for (name, build, setup, detail) in cases {
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
        assert_eq!(done.report.detail, detail, "{name}");
        let record = rig.record().unwrap();
        let hands_over = name.ends_with("handover") || name == "egress-agents";
        assert_eq!(record.handover, hands_over, "{name}");
        assert_eq!(record.cordon.placed, !hands_over, "{name}");
    }
}

const HANDOVER: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";

/// Asserts the egress rule: a host whose flags set an egress ceiling (or cannot be read) hands
/// over, with no cordon and the handover note, when both the previous agent and the staged
/// agent list `egress-handover`; drains naming the previous agent when only it lacks the word
/// (a stopped agent that does not leave the wiring would cut the jobs' network); and drains
/// naming the staged agent when only it lacks the word (the new agent would remove the jobs it
/// cannot adopt). A host without a ceiling still hands over between agents with only
/// `handover`. Catches an egress host handed over to or from an agent that does not keep its
/// wiring, and an egress host that drains although both agents keep it.
#[test]
fn an_egress_host_hands_over_only_between_agents_that_both_keep_egress_wiring() {
    let cases = [
        (
            "both",
            Some(KEEPS_EGRESS),
            Some(KEEPS_EGRESS),
            true,
            HANDOVER.to_owned(),
        ),
        (
            "previous-only-handover",
            Some(TAKES),
            Some(KEEPS_EGRESS),
            false,
            format!("{DRAINS}{NO_KEEP}"),
        ),
        (
            "staged-lacks",
            Some(KEEPS_EGRESS),
            Some(TAKES),
            false,
            format!("{DRAINS}{NO_ADOPT}"),
        ),
        (
            "previous-fails",
            None,
            Some(KEEPS_EGRESS),
            false,
            format!("{DRAINS}{NO_HANDOVER}"),
        ),
        (
            "neither",
            Some(TAKES),
            Some(TAKES),
            false,
            format!("{DRAINS}{NO_KEEP}"),
        ),
    ];
    for (name, previous, staged, hands_over, detail) in cases {
        let rig = Rig::new(&format!("hook-handover-egress-{name}"));
        rig.world.seed_unit(&rig.unit, "active", "running");
        rig.set_leases(2);
        let spy = Spy::new(&rig);
        spy.egress.set(true);
        *spy.version.borrow_mut() = Ok(staged.unwrap().to_owned());
        *spy.previous_version.borrow_mut() =
            Some(previous.map_or(Err("exec failed".to_owned()), |text| Ok(text.to_owned())));
        let done = spy.prepare(&rig);
        // A host that drains waits out its two leases and defers; the cause is in that detail.
        let ended = match hands_over {
            true => done.report.detail.clone(),
            false => {
                assert_eq!(done.outcome, Outcome::Deferred, "{name}: {done:?}");
                let text = done.report.detail.clone().unwrap();
                text.split_once("; draining because ")
                    .map(|(_, why)| format!("{DRAINS}{why}"))
            }
        };
        assert_eq!(ended, Some(detail), "{name}: {done:?}");
        let record = rig.record().unwrap();
        assert_eq!(record.handover, hands_over, "{name}");
        assert!(!record.cordon.placed, "{name}: the deferral lifted it");
    }
    // Without a ceiling, an agent that lists only `handover` is enough.
    let rig = Rig::new("hook-handover-egress-none");
    rig.world.seed_unit(&rig.unit, "active", "running");
    let spy = spy(&rig);
    assert_eq!(spy.prepare(&rig).report.detail.as_deref(), Some(HANDOVER));
    assert!(rig.record().unwrap().handover);
}

/// Asserts that a drained prepare, run through the hook's entry as hostkeeper runs it, writes
/// a journal line `js-agent update-hook: prepare succeeded: ...` that names the cause, and
/// that the hand-over writes its own note. Catches a drain an operator reading the journal
/// cannot tell from a hand-over.
#[test]
fn the_journal_line_of_a_drained_prepare_names_the_cause() {
    use js_agent::update_hook::{Host, VERB, run};
    let rig = Rig::new("hook-handover-journal");
    rig.world.seed_unit(&rig.unit, "active", "running");
    let spy = Spy::new(&rig);
    let file = rig.tmp.path().join("params-for-the-verb.toml");
    std::fs::write(&file, &rig.params_text).unwrap();
    let staged = rig.params.staged_release.clone();
    let step = |_: &js_agent::update_hook::Invocation, _: &js_agent::update_hook::Params| {
        spy.prepare(&rig)
    };
    let maintain = |_: &js_agent::update_hook::MaintainInvocation,
                    _: &js_agent::update_hook::DrainParams| {
        unreachable!("an update step ran a maintenance step")
    };
    let args: Vec<std::ffi::OsString> = [
        "js-agent".as_ref(),
        VERB.as_ref(),
        "prepare".as_ref(),
        staged.as_os_str(),
        "--params".as_ref(),
        file.as_os_str(),
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run(
        &args,
        &mut out,
        &mut err,
        &Host {
            step: &step,
            maintain: &maintain,
        },
    );
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!("js-agent update-hook: prepare succeeded: {DRAINS}{NO_HANDOVER}\n")
    );
}

/// Asserts that a drain that passes its bound with a lease still running defers with the cause
/// of the drain after the lease count: here a previous agent that does not hand over. Catches a
/// deferral that leaves the operator unable to tell why the host was draining at all.
#[test]
fn the_deferral_of_a_drain_names_why_the_host_was_draining() {
    let rig = Rig::new("hook-handover-drain-bound");
    rig.world.seed_unit(&rig.unit, "active", "running");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(
        done.report.detail,
        Some(format!(
            "the host still held 1 lease(s) after {DRAIN_WAIT_SECS} s; draining because {NO_HANDOVER}"
        ))
    );
}

/// Asserts that a prepare run again on the record of a drain says why again, and that when the
/// conditions now allow a hand-over it keeps the drain it began with and says it was decided
/// then. Catches a rerun that goes silent, and one that flips the decision or tells the
/// operator the update hands over while the record drains.
#[test]
fn a_rerun_of_a_drained_prepare_says_why_and_keeps_the_decision() {
    let rig = Rig::new("hook-handover-rerun");
    rig.world.seed_unit(&rig.unit, "active", "running");
    let first = Spy::new(&rig);
    assert_eq!(first.prepare(&rig).outcome, Outcome::Success);
    assert!(!rig.record().unwrap().handover);
    let second = Spy::new(&rig);
    assert_eq!(
        second.prepare(&rig).report.detail,
        Some(format!("{DRAINS}{NO_HANDOVER}"))
    );
    let third = spy(&rig);
    let done = third.prepare(&rig);
    assert_eq!(
        done.report.detail,
        Some(format!("{DRAINS}decided when the update began"))
    );
    assert!(!rig.record().unwrap().handover);
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
    cutover_sweep(false);
}

/// The sweep of the cutover above on a host with an egress ceiling whose agents both keep
/// egress wiring: the hand-over, and so every kill point, is the same.
#[test]
fn an_egress_handover_cutover_killed_at_any_point_and_run_again_ends_as_an_uninterrupted_one() {
    cutover_sweep(true);
}

fn cutover_sweep(egress: bool) {
    let baseline = handing_on("hook-handover-sweep-base", egress);
    assert_eq!(
        spy_on(&baseline, egress).cutover(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points();
    assert!(
        points >= 6,
        "the marks, the request, the withdrawal and the acts: {points}"
    );
    let end = end_state(&baseline);
    for k in 1..=points {
        let rig = handing_on(&format!("hook-handover-sweep-{k}"), egress);
        rig.kill.arm(k);
        assert!(
            killable(|| spy_on(&rig, egress).cutover(&rig)).is_none(),
            "kill point {k} of {points} is reached"
        );
        rig.kill.disarm();
        let rerun = spy_on(&rig, egress);
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
    rollback_sweep(false);
}

/// The sweep of the rollback above on a host with an egress ceiling whose agents both keep
/// egress wiring: the roles swap, and the rollback hands the runs back all the same.
#[test]
fn an_egress_handover_rollback_killed_at_any_point_and_run_again_ends_with_the_previous_agent() {
    rollback_sweep(true);
}

fn rollback_sweep(egress: bool) {
    let setup = |name: &str| {
        let rig = handing_on(name, egress);
        assert_eq!(spy_on(&rig, egress).cutover(&rig).outcome, Outcome::Success);
        rig.kill.disarm();
        rig
    };
    let baseline = setup("hook-handover-rb-sweep-base");
    let before = baseline.kill.points();
    assert_eq!(
        spy_on(&baseline, egress).rollback(&baseline).outcome,
        Outcome::Success
    );
    let points = baseline.kill.points() - before;
    assert!(points >= 5, "{points}");
    for k in 1..=points {
        let rig = setup(&format!("hook-handover-rb-sweep-{k}"));
        rig.kill.arm(rig.kill.points() + k);
        assert!(
            killable(|| spy_on(&rig, egress).rollback(&rig)).is_none(),
            "point {k}"
        );
        rig.kill.disarm();
        let rerun = spy_on(&rig, egress);
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

/// Asserts that a stop the unit manager refuses, in `cutover` and in `rollback`, takes the
/// request back: the step fails as an environment matter with the agent still running, and no
/// handover file is left for a later stop of that agent, which an operator means as a drain.
/// Catches a request left behind by a stop that never reached the agent.
#[test]
fn a_stop_the_unit_manager_refuses_takes_the_request_back() {
    let rig = handing("hook-handover-stop-refused");
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.cutover(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert_eq!(done.report.class, Class::Environment);
    assert!(spy.act_names().contains(&REQUEST.to_owned()));
    assert!(!file(&rig).exists());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");

    let rig = handing("hook-handover-rb-stop-refused");
    assert_eq!(Spy::new(&rig).cutover(&rig).outcome, Outcome::Success);
    let spy = Spy::new(&rig);
    spy.fail("stop");
    let done = spy.rollback(&rig);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::StopFailed));
    assert!(spy.act_names().contains(&REQUEST.to_owned()));
    assert!(!file(&rig).exists());
    assert_eq!(rig.world.show(UNIT).unwrap().active, "active");
}

/// Asserts the record's `handover` field is additive: the record of an update that drains is
/// written without it, as before the field existed, and such a record reads back as one that
/// drains; the record of a handover carries `"handover": true` and reads back as one. Catches a
/// record that an earlier hook can no longer read for an update that drains, and a record of
/// a handover read as a drain after a rerun.
#[test]
fn the_record_carries_the_handover_only_when_there_is_one() {
    let drains = Rig::new("hook-handover-record-drains");
    drains.world.seed_unit(&drains.unit, "active", "running");
    assert_eq!(Spy::new(&drains).prepare(&drains).outcome, Outcome::Success);
    let bytes = drains.record_bytes().unwrap();
    let text: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(text.get("handover").is_none(), "{text}");
    assert!(!drains.record().unwrap().handover);

    let hands = handing("hook-handover-record-hands");
    let text: serde_json::Value = serde_json::from_slice(&hands.record_bytes().unwrap()).unwrap();
    assert_eq!(text["handover"], json!(true));
    assert!(hands.record().unwrap().handover);
}
