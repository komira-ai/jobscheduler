//! `resume` (hostkeeper's maintenance-updates design, 5.2 and 5.4), in process over the fakes'
//! world: what it changes after a `drain`, when it restarts the agent, and how it ends. `drain`
//! is in steps_drain.rs; both run over the rig of maintain_rig.rs.

use js_agent::lifecycle::hold::{HOLD_FILE, HOLD_TEXT};
use js_agent::update_hook::step::{RESTART_HELLO_SECS, RESTART_STOP_SECS};
use js_agent::update_hook::{Lease, LeaseError, POLL_SECS, Reason, Units};

use super::fakes::FACTS;
use super::maintain_rig::{
    Probe, Rig, UNIT, environment_failure, hello, note_text, set_job, succeeded, succeeded_with,
};

/// The facts of a host after work on it changed them.
const CHANGED: &str = "os=linux arch=aarch64 cpu_millis=4000";

/// A host after a `drain` that found it idle: held, cordoned, with the facts noted.
fn drained(name: &str) -> Rig {
    let rig = Rig::new(name);
    {
        let probe = Probe::new(&rig);
        assert_eq!(probe.drain(&rig.params), succeeded());
    }
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert!(rig.expiry().is_some());
    rig
}

/// A drained host whose facts changed since.
fn changed(name: &str) -> Rig {
    let rig = drained(name);
    rig.world.set_facts(CHANGED);
    rig
}

/// Asserts that a resume after a drain renames the hold file, lifts the cordon the drain
/// placed, removes the renamed file, and restarts nothing when the facts are the same, reading
/// neither the unit nor the host. It catches a resume that restarts an agent whose facts did not
/// change, or one that leaves the host cordoned or held.
#[test]
fn resume_lifts_the_cordon_and_the_hold_and_restarts_nothing_on_equal_facts() {
    let rig = drained("hook-resume-equal");
    let probe = Probe::new(&rig);
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.expiry(), None);
    assert_eq!(
        probe.changes(),
        ["hold.release", "lease.uncordon", "hold.released"]
    );
    assert_eq!(probe.count("units.show"), 0);
    assert_eq!(probe.count("lease.host"), 0);
}

/// Asserts that a second resume makes no lease call and changes nothing: one uncordon in all.
/// It catches a resume that lifts a cordon an operator placed after the first.
#[test]
fn resume_twice_is_one_uncordon() {
    let rig = drained("hook-resume-twice");
    let probe = Probe::new(&rig);
    assert_eq!(probe.resume(&rig.params), succeeded());
    let (calls, changes) = (probe.lease_calls(), probe.changes());
    // An operator cordons after the first resume: the second leaves it.
    let operator = rig.world.cordon(900).unwrap().host.cordon.unwrap();
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(probe.lease_calls(), calls);
    assert_eq!(probe.changes(), changes);
    assert_eq!(probe.count("lease.uncordon"), 1);
    assert_eq!(rig.expiry(), Some(operator.expires_at_ms));
}

/// Asserts that a resume with no drain before it succeeds with nothing to say, changes nothing
/// and makes no lease call, and that a cordon in force (an operator's) stays. It catches a
/// resume that lifts a cordon it did not place.
#[test]
fn resume_with_no_drain_changes_nothing_and_leaves_an_operators_cordon() {
    let rig = Rig::new("hook-resume-none");
    let operator = rig.world.cordon(900).unwrap().host.cordon.unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(probe.lease_calls(), 0);
    assert_eq!(probe.changes(), Vec::<&str>::new());
    assert_eq!(rig.expiry(), Some(operator.expires_at_ms));
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
}

/// Asserts that a cordon in force before the drain stays in force after the resume, which
/// ends the hold and makes no uncordon call. It catches a resume that lifts an operator's
/// cordon.
#[test]
fn resume_leaves_a_cordon_that_was_in_force_before_the_drain() {
    let rig = Rig::new("hook-resume-before");
    let operator = rig.world.cordon(900).unwrap().host.cordon.unwrap();
    {
        let probe = Probe::new(&rig);
        assert_eq!(probe.drain(&rig.params), succeeded());
    }
    let probe = Probe::new(&rig);
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(probe.count("lease.uncordon"), 0);
    assert_eq!(probe.changes(), ["hold.release", "hold.released"]);
    assert_eq!(rig.expiry(), Some(operator.expires_at_ms));
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
}

/// Asserts that a renamed hold file a killed resume left is finished by the next resume: the
/// cordon is lifted, the file removed, nothing renamed again and the agent not restarted even
/// though the facts changed. It catches a resume that restarts the agent after the hold was
/// ended, or one that cannot finish what it began.
#[test]
fn a_hold_a_killed_resume_renamed_is_finished_without_a_restart() {
    let rig = changed("hook-resume-killed");
    rig.rename_hold();
    let probe = Probe::new(&rig);
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.expiry(), None);
    assert_eq!(probe.changes(), ["lease.uncordon", "hold.released"]);
    assert_eq!(probe.count("units.stop"), 0);
    assert_eq!(probe.count("lease.host"), 0);
}

/// Asserts that a resume after the facts changed on an idle host stops the unit, starts it,
/// waits for the hello of a new session, and only then ends the hold and lifts the cordon, in
/// that order. It catches a cordon lifted before the agent's new facts reach the server, or a
/// restart that does not wait for the hello.
#[test]
fn changed_facts_on_an_idle_host_restart_the_agent_before_the_cordon_is_lifted() {
    let rig = changed("hook-resume-restart");
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 1 {
            hello(p);
        }
    });
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(
        probe.changes(),
        [
            "units.stop",
            "units.start",
            "hello",
            "hold.release",
            "lease.uncordon",
            "hold.released"
        ]
    );
    let unit = rig.world.show(UNIT).unwrap();
    assert_eq!(
        (unit.active.as_str(), unit.sub.as_str()),
        ("active", "running")
    );
    assert_eq!(rig.expiry(), None);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.world.ticks(), 1);
}

/// Asserts that a restart waits for the unit to go down: a unit that is `deactivating` and then
/// inactive is waited for, the start comes after, and the hello after that. It catches a start
/// of a unit that is still stopping.
#[test]
fn the_restart_waits_for_the_unit_to_go_down_before_it_starts_it() {
    let rig = changed("hook-resume-stopping");
    let probe = Probe::new(&rig);
    probe.stop_ignored.set(true);
    probe.at_pause(|p, taken| match taken {
        1 => p.world.seed_unit(UNIT, "deactivating", "stop-sigterm"),
        2 => p.world.seed_unit(UNIT, "inactive", "dead"),
        _ => hello(p),
    });
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(rig.world.ticks(), 3);
    assert_eq!(
        probe.changes(),
        [
            "units.stop",
            "units.start",
            "hello",
            "hold.release",
            "lease.uncordon",
            "hold.released"
        ]
    );
}

/// Asserts that a restart treats a server that does not answer, a host that is `awaiting` and a
/// live host with no session as no hello yet, and goes on once a hello arrives. It catches a
/// restart that fails on one unanswered read or takes a host that has not said hello for one
/// that has.
#[test]
fn the_restart_takes_no_answer_an_awaiting_host_and_no_session_for_no_hello() {
    let rig = changed("hook-resume-no-hello-yet");
    let probe = Probe::new(&rig);
    // The first read is the idle test, the second the first look for the hello.
    probe.fail_lease(
        "lease.host",
        2,
        LeaseError::Unreachable("timed out".to_owned()),
    );
    probe.at_pause(|p, taken| match taken {
        1 => p.world.alter_host(|host| host.session_started_ms = None),
        2 => p.world.forget_host(),
        _ => hello(p),
    });
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(rig.world.ticks(), 3);
    assert_eq!(rig.expiry(), None);
}

/// Asserts that facts that changed on a host that is not idle restart nothing, end the hold and
/// lift the cordon, and that the success says so by its exact detail. It catches a restart that
/// cuts the job short, or a success that hides that the agent still has the old facts.
#[test]
fn changed_facts_on_a_busy_host_restart_nothing_and_the_success_says_so() {
    let rig = changed("hook-resume-busy");
    set_job(&rig.world, true);
    let probe = Probe::new(&rig);
    assert_eq!(
        probe.resume(&rig.params),
        succeeded_with(
            "the host facts changed and the host is not idle: the agent was not restarted, \
             and says hello with the facts it read when it started"
        )
    );
    assert_eq!(probe.count("units.stop"), 0);
    assert_eq!(probe.count("units.start"), 0);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.expiry(), None);
}

/// Asserts that facts the resume cannot read restart nothing, and that the success carries the
/// reason of the read in its detail while the hold ends. It catches a resume that fails, or
/// restarts the agent, on facts it cannot compare.
#[test]
fn facts_unreadable_at_the_resume_restart_nothing_and_the_success_says_why() {
    let rig = changed("hook-resume-facts");
    let probe = Probe::new(&rig);
    probe.fail("facts.read", 1, "gpu probe failed");
    assert_eq!(
        probe.resume(&rig.params),
        succeeded_with(
            "the host facts cannot be read (gpu probe failed): the agent was not restarted"
        )
    );
    assert_eq!(probe.count("units.stop"), 0);
    assert_eq!(probe.count("lease.host"), 0);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.expiry(), None);
}

/// Asserts that a hold noted without facts (the drain could not read them) restarts nothing
/// when the resume reads them, and that the success says so. It catches a resume that treats the
/// missing note as a change.
#[test]
fn facts_unreadable_at_the_drain_restart_nothing_at_the_resume() {
    let rig = Rig::new("hook-resume-note-without-facts");
    {
        let probe = Probe::new(&rig);
        probe.fail("facts.read", 1, "gpu probe failed");
        assert_eq!(probe.drain(&rig.params), succeeded());
    }
    assert_eq!(rig.hold_text(), Some(note_text(false, None)));
    rig.world.set_facts(CHANGED);
    let probe = Probe::new(&rig);
    assert_eq!(
        probe.resume(&rig.params),
        succeeded_with("the drain could not read the host facts: the agent was not restarted")
    );
    assert_eq!(probe.count("units.stop"), 0);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.expiry(), None);
}

/// Asserts that a hold file a person wrote (only the text, no note, and a line nothing reads)
/// is resumed: the cordon is lifted, the file removed, and nothing restarted. It catches a
/// resume that needs the note, or one that leaves a cordon it must lift.
#[test]
fn a_hold_a_person_wrote_is_resumed_with_the_cordon_lifted_and_no_restart() {
    let rig = Rig::new("hook-resume-person");
    rig.world.cordon(900).unwrap();
    std::fs::write(
        rig.root().join(HOLD_FILE),
        format!("{HOLD_TEXT}see ticket 12\n"),
    )
    .unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(
        probe.resume(&rig.params),
        succeeded_with("the drain could not read the host facts: the agent was not restarted")
    );
    assert_eq!(rig.expiry(), None);
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(probe.count("units.stop"), 0);
}

/// Asserts that a unit that cannot be stopped fails the resume with `stop_failed`, class
/// environment and the exact detail, with the hold and the cordon left as they are and no start
/// made. It catches a resume that ends the hold of a host whose agent kept its old facts.
#[test]
fn a_stop_that_fails_fails_the_resume_and_the_hold_stays() {
    let rig = changed("hook-resume-stop-fails");
    let probe = Probe::new(&rig);
    probe.fail("units.stop", 1, "unit masked");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(
            Reason::StopFailed,
            &format!("cannot stop {UNIT}: unit masked")
        )
    );
    assert_eq!(probe.count("units.start"), 0);
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert!(rig.expiry().is_some());
}

/// Asserts that a unit that does not go down within `RESTART_STOP_SECS` fails the resume with
/// `stop_failed` and the exact detail, after that many seconds of polls, with no start made, the
/// hold left and no cordon lifted. It catches a restart that waits for ever or starts a unit that
/// never stopped.
#[test]
fn a_unit_that_never_goes_down_fails_the_resume_after_the_stop_bound() {
    let rig = changed("hook-resume-never-down");
    let probe = Probe::new(&rig);
    probe.stop_ignored.set(true);
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(
            Reason::StopFailed,
            &format!("{UNIT} is still active after {RESTART_STOP_SECS} s")
        )
    );
    assert_eq!(rig.world.ticks() as u64, RESTART_STOP_SECS / POLL_SECS);
    assert_eq!(probe.count("units.start"), 0);
    assert!(rig.hold_text().is_some());
    // The fake clock has passed the drain's lease, so its cordon has lapsed on its own (on a
    // host the held agent renews it); what the step must not do is lift it.
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that a unit state that cannot be read while the resume waits for the stop fails it
/// with `stop_failed` and the exact detail. It catches a wait that takes an unreadable unit for
/// a stopped one.
#[test]
fn a_unit_state_that_cannot_be_read_fails_the_resume_with_stop_failed() {
    let rig = changed("hook-resume-show-fails");
    let probe = Probe::new(&rig);
    probe.fail("units.show", 1, "dbus gone");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(
            Reason::StopFailed,
            &format!("cannot read the state of {UNIT}: dbus gone")
        )
    );
    assert_eq!(probe.count("units.start"), 0);
    assert!(rig.hold_text().is_some());
}

/// Asserts that a unit that cannot be started fails the resume with `start_failed` and the
/// exact detail, with the hold and the cordon left. It catches a resume that lifts the cordon
/// of a host whose agent is down.
#[test]
fn a_start_that_fails_fails_the_resume_and_the_hold_stays() {
    let rig = changed("hook-resume-start-fails");
    let probe = Probe::new(&rig);
    probe.fail("units.start", 1, "unit masked");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(
            Reason::StartFailed,
            &format!("cannot start {UNIT}: unit masked")
        )
    );
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert!(rig.expiry().is_some());
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that an agent that says no hello within `RESTART_HELLO_SECS` fails the resume with
/// `no_hello` and the exact detail, after that many seconds of polls, with the hold left and no
/// cordon lifted. It catches a resume that lifts the cordon before the new agent is known to the
/// server.
#[test]
fn an_agent_that_says_no_hello_fails_the_resume_with_no_hello() {
    let rig = changed("hook-resume-no-hello");
    let probe = Probe::new(&rig);
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(
            Reason::NoHello,
            &format!("the restarted agent said no hello in {RESTART_HELLO_SECS} s")
        )
    );
    assert_eq!(rig.world.ticks() as u64, RESTART_HELLO_SECS / POLL_SECS);
    assert!(rig.hold_text().is_some());
    // The fake clock has passed the drain's lease, so its cordon has lapsed on its own (on a
    // host the held agent renews it); what the step must not do is lift it.
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that a server that cannot be read for the idle test fails the resume with
/// `lease_unreachable` and the server's words, before anything is stopped, with the hold and the
/// cordon left. It catches a restart of an agent on a host that was not known to be idle.
#[test]
fn a_refused_idle_read_fails_the_resume_before_any_restart() {
    let rig = changed("hook-resume-read-refused");
    let probe = Probe::new(&rig);
    probe.fail_lease("lease.host", 1, LeaseError::Refused("http-503".to_owned()));
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(Reason::LeaseUnreachable, "refused: http-503")
    );
    assert_eq!(probe.count("units.stop"), 0);
    assert!(rig.hold_text().is_some());
    assert!(rig.expiry().is_some());
}

/// Asserts that an uncordon the server cannot answer fails the resume with `lease_unreachable`,
/// leaves the renamed hold file, and that a second resume finishes the end of the hold: the
/// cordon lifted, the file removed, nothing renamed again. It catches a resume that cannot be
/// run again after a failed uncordon.
#[test]
fn a_failed_uncordon_fails_the_resume_and_the_next_resume_finishes_it() {
    let rig = drained("hook-resume-uncordon-fails");
    let probe = Probe::new(&rig);
    probe.fail_lease(
        "lease.uncordon",
        1,
        LeaseError::Unreachable("timed out".to_owned()),
    );
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(Reason::LeaseUnreachable, "timed out")
    );
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), Some(note_text(false, Some(FACTS))));
    assert!(rig.expiry().is_some());

    let again = Probe::new(&rig);
    assert_eq!(again.resume(&rig.params), succeeded());
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.expiry(), None);
    assert_eq!(again.changes(), ["lease.uncordon", "hold.released"]);
}

/// Asserts that a server that does not know the host at the uncordon (it restarted) does not
/// fail the resume: there is no cordon to lift, and the hold ends. It catches a resume that
/// fails on `unknown-host`.
#[test]
fn an_unknown_host_at_the_uncordon_does_not_fail_the_resume() {
    let rig = drained("hook-resume-uncordon-unknown");
    let probe = Probe::new(&rig);
    probe.fail_lease("lease.uncordon", 1, LeaseError::UnknownHost);
    assert_eq!(probe.resume(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
}

/// Asserts that a hold file that cannot be read fails the resume with `hold_failed`, class
/// environment and the exact detail, before the lease server is called. It catches a resume
/// that takes an unreadable hold for none.
#[test]
fn a_hold_state_that_cannot_be_read_fails_the_resume() {
    let rig = drained("hook-resume-state-fails");
    let probe = Probe::new(&rig);
    probe.fail("hold.state", 1, "disk gone");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: disk gone")
    );
    assert_eq!(probe.lease_calls(), 0);
    assert!(rig.hold_text().is_some());
}

/// Asserts that a hold file that cannot be renamed fails the resume with `hold_failed` and the
/// exact detail, with the hold on and the cordon in force and no uncordon made. It catches a
/// resume that lifts the cordon while the agent still renews it.
#[test]
fn a_hold_that_cannot_be_renamed_fails_the_resume_and_lifts_nothing() {
    let rig = drained("hook-resume-release-fails");
    let probe = Probe::new(&rig);
    probe.fail("hold.release", 1, "rename failed");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: rename failed")
    );
    assert!(rig.hold_text().is_some());
    assert!(rig.expiry().is_some());
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that a renamed hold file that cannot be removed fails the resume with `hold_failed`
/// and the exact detail after the cordon was lifted, and that the next resume removes it. It
/// catches a resume that cannot finish a hold it ended.
#[test]
fn a_renamed_hold_that_cannot_be_removed_fails_the_resume_and_the_next_one_removes_it() {
    let rig = drained("hook-resume-released-fails");
    let probe = Probe::new(&rig);
    probe.fail("hold.released", 1, "unlink failed");
    assert_eq!(
        probe.resume(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: unlink failed")
    );
    assert_eq!(rig.resume_text(), Some(note_text(false, Some(FACTS))));
    assert_eq!(rig.expiry(), None);

    let again = Probe::new(&rig);
    assert_eq!(again.resume(&rig.params), succeeded());
    assert_eq!(rig.resume_text(), None);
}

/// Asserts that each change of a resume that restarts the agent follows a kill point: the stop,
/// the start, the rename, the uncordon and the removal. It catches a change a test cannot end
/// the step before.
#[test]
fn every_change_of_a_resume_passes_a_kill_point_first() {
    let rig = changed("hook-resume-gate");
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 1 {
            hello(p);
        }
    });
    assert_eq!(probe.resume(&rig.params), succeeded());
    probe.assert_gated(&[
        "units.stop",
        "units.start",
        "hold.release",
        "lease.uncordon",
        "hold.released",
    ]);
    assert_eq!(probe.count("gate"), 5);
}
