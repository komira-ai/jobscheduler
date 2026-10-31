//! `prepare` (host-updates design, 4.2), on the fakes: what it refuses and with which status,
//! what it records before it waits, how it waits and renews, and how it ends when the bound
//! passes. The unit and the link are never touched in `prepare`; the call log shows it.

use std::cell::RefCell;
use std::rc::Rc;

use js_agent::update_hook::{Cordon, Done, Outcome, Reason, SCHEMA, Units};
use serde_json::Value;

use super::step_rig::{
    DRAIN_WAIT_SECS, LEASE_SECS, POLL_SECS, RETRY_AFTER_SECS, Rig, START, Spy, UNIT, digest, line,
};

fn reason(done: &Done) -> Value {
    line(done)["reason"].clone()
}

/// Nothing was done: no record, no call but the version check, the cordon as it was.
fn nothing_changed(rig: &Rig, spy: &Spy<'_>) {
    assert_eq!(rig.record_bytes(), None);
    let acts: Vec<_> = spy
        .log()
        .into_iter()
        .filter(|call| !call.starts_with("run("))
        .collect();
    assert_eq!(acts, Vec::<String>::new());
    assert_eq!(rig.world.ticks(), 0);
    assert_eq!(rig.cordon_expiry(), None);
}

#[test]
fn a_clean_prepare_records_then_cordons_waits_for_the_drain_and_succeeds() {
    let rig = Rig::new("hook-prepare-clean");
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    let rig_ref = &rig;
    spy.on_pause(move |spy, ticks| {
        sink.borrow_mut()
            .push((ticks, rig_ref.record(), rig_ref.cordon_expiry()));
        if ticks == 3 {
            spy.world.alter_host(|host| {
                host.leases = 0;
                host.lease = None;
            });
        }
    });

    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.report.reason, None);
    assert!(done.report.reversible);
    assert_eq!(rig.world.ticks(), 3, "one pause per poll that saw a lease");
    // The version check was made through the runner, on the staged agent.
    let staged = rig.params.staged_release.join("js-agent");
    assert_eq!(spy.log()[0], format!("run({} --version)", staged.display()));
    // The record was durable before the first pause, with the cordon placed.
    let seen = seen.borrow();
    let (ticks, before_wait, expiry) = &seen[0];
    assert_eq!(*ticks, 1);
    let record = before_wait.as_ref().expect("the record exists at the wait");
    assert_eq!(
        record.cordon,
        Cordon {
            placed: true,
            first_ms: START,
            expires_ms: START + LEASE_SECS * 1000
        }
    );
    assert_eq!(*expiry, Some(START + LEASE_SECS * 1000));
    // Each poll renews: the cordon moves on with the clock, and the record follows.
    let expiries: Vec<_> = seen.iter().map(|(_, _, expiry)| expiry.unwrap()).collect();
    let step = POLL_SECS * 1000;
    let first = START + LEASE_SECS * 1000;
    assert_eq!(expiries, [first, first + step, first + 2 * step]);
    let end = rig.record().unwrap();
    assert_eq!(end.cordon.placed, true);
    assert_eq!(end.cordon.first_ms, START);
    assert_eq!(end.cordon.expires_ms, first + 3 * step);
    assert_eq!(rig.cordon_expiry(), Some(end.cordon.expires_ms));
    // What the record says of the update.
    assert_eq!(end.schema, SCHEMA);
    assert_eq!(end.update, rig.fresh_record().update);
    assert_eq!(end.update.artifact_digest, digest());
    assert_eq!(end.update.unit, UNIT);
    assert_eq!(
        (
            end.stop_begun,
            end.link_moved,
            end.start_issued,
            end.verify_ok
        ),
        (false, false, false, false)
    );
    // The cordon calls, and nothing else done to the machine.
    let acts: Vec<_> = spy.log().into_iter().skip(1).collect();
    assert_eq!(acts, vec![format!("cordon({LEASE_SECS})"); 4]);
    assert_eq!(rig.host().cordon.is_some(), true, "left for cutover");
}

#[test]
fn a_host_with_no_lease_needs_no_wait() {
    let rig = Rig::new("hook-prepare-idle");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(rig.world.ticks(), 0);
    assert_eq!(spy.log().len(), 2, "the version check and one cordon");
    assert_eq!(rig.record().unwrap().cordon.placed, true);
}

#[test]
fn a_host_that_only_has_runs_to_finish_is_not_drained() {
    let rig = Rig::new("hook-prepare-releasing");
    rig.world.alter_host(|host| host.releasing = true);
    let spy = Spy::new(&rig);
    spy.on_pause(|spy, ticks| {
        if ticks == 2 {
            spy.world.alter_host(|host| host.releasing = false);
        }
    });
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.world.ticks(), 2);
}

#[test]
fn a_taint_halts_with_nothing_changed() {
    let rig = Rig::new("hook-prepare-taint");
    rig.taint();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(done.outcome.code(), 76);
    assert_eq!(reason(&done), "tainted");
    assert_eq!(line(&done)["class"], "release");
    nothing_changed(&rig, &spy);
}

#[test]
fn an_owed_check_halts_with_nothing_changed() {
    let rig = Rig::new("hook-prepare-owed");
    rig.state_root().owe("run-7").unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Halt);
    assert_eq!(reason(&done), "owed_check");
    assert!(done.report.detail.unwrap().contains("run-7"));
    nothing_changed(&rig, &spy);
}

#[test]
fn a_state_root_that_cannot_be_listed_fails_and_changes_nothing() {
    let rig = Rig::new("hook-prepare-owed-unreadable");
    let spy = Spy::new(&rig);
    spy.fail("owed");
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(reason(&done), "state_invalid");
    nothing_changed(&rig, &spy);
}

#[test]
fn the_staged_agent_must_report_the_target_version_and_the_crun_runtime_if_it_names_runtimes() {
    let cases = [
        ("js-agent 1.3.9\n", Reason::VersionMismatch),
        ("js-agent 1.4.0 extra\n", Reason::BadArtifact),
        (
            "js-agent 1.4.0\nruntimes: docker\n",
            Reason::VersionMismatch,
        ),
        ("other 1.4.0\n", Reason::BadArtifact),
        ("1.4.0\n", Reason::BadArtifact),
        ("", Reason::BadArtifact),
    ];
    for (n, (printed, expected)) in cases.into_iter().enumerate() {
        let rig = Rig::new(&format!("hook-prepare-version-{n}"));
        let spy = Spy::new(&rig);
        *spy.version.borrow_mut() = Ok(printed.to_owned());
        let done = spy.prepare(&rig);
        assert_eq!(done.outcome, Outcome::Failure, "{printed:?}");
        assert_eq!(done.report.reason, Some(expected), "{printed:?}");
        nothing_changed(&rig, &spy);
    }
    for (n, printed) in [
        "js-agent 1.4.0\n",
        "js-agent 1.4.0\nruntimes: docker crun\n",
        "js-agent 1.4.0\nruntimes: crun\n",
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(&format!("hook-prepare-version-ok-{n}"));
        let spy = Spy::new(&rig);
        *spy.version.borrow_mut() = Ok(printed.to_owned());
        assert_eq!(spy.prepare(&rig).outcome, Outcome::Success, "{printed:?}");
    }
}

#[test]
fn an_agent_that_does_not_run_or_a_release_without_one_is_refused_and_nothing_changes() {
    let rig = Rig::new("hook-prepare-no-run");
    let spy = Spy::new(&rig);
    *spy.version.borrow_mut() = Err("exec failed".to_owned());
    let done = spy.prepare(&rig);
    assert_eq!(
        (done.outcome, done.report.reason),
        (Outcome::Failure, Some(Reason::BadArtifact))
    );
    nothing_changed(&rig, &spy);

    let rig = Rig::new("hook-prepare-no-binary");
    std::fs::remove_file(rig.params.staged_release.join("js-agent")).unwrap();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(
        (done.outcome, done.report.reason),
        (Outcome::Failure, Some(Reason::BadArtifact))
    );
    nothing_changed(&rig, &spy);
}

#[test]
fn a_taint_is_checked_before_the_version_and_after_the_record() {
    // Halting comes first: no version can repair a taint, so the agent is not even run.
    let rig = Rig::new("hook-prepare-taint-first");
    rig.taint();
    let spy = Spy::new(&rig);
    *spy.version.borrow_mut() = Ok("js-agent 9.9.9\n".to_owned());
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Halt);
    assert_eq!(spy.log(), Vec::<String>::new());

    // The record is validated first: a record that is not this update's is refused as such.
    let rig = Rig::new("hook-prepare-record-first");
    rig.taint();
    let mut other = rig.fresh_record();
    other.update.target = "1.5.0".to_owned();
    rig.put_record(&other);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
}

#[test]
fn a_bound_that_passes_with_a_lease_defers_lifts_the_cordon_and_stops_nothing() {
    let rig = Rig::new("hook-prepare-deferred");
    rig.set_leases(1);
    rig.world.seed_unit(UNIT, "active", "running");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);

    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.outcome.code(), 75);
    assert_eq!(done.report.reason, Some(Reason::DrainBound));
    assert_eq!(done.report.retry_after_secs, Some(RETRY_AFTER_SECS));
    assert!(done.report.reversible);
    assert!(done.report.detail.unwrap().contains("lease"));
    assert_eq!(rig.world.ticks() as u64, DRAIN_WAIT_SECS / POLL_SECS);
    // The cordon we placed is lifted, and the record no longer says there is one to lift.
    assert_eq!(rig.cordon_expiry(), None);
    let record = rig.record().unwrap();
    assert_eq!(record.cordon, Cordon::default());
    assert_eq!(spy.log().last().map(String::as_str), Some("uncordon"));
    // No stop, no start, no relink, no signal: the agent runs as it did.
    let touched: Vec<_> = spy
        .log()
        .into_iter()
        .filter(|call| call.starts_with("units.") || call.starts_with("link."))
        .collect();
    assert_eq!(touched, Vec::<String>::new());
    let unit = Units::show(&rig.world, UNIT).unwrap();
    assert_eq!(
        (unit.active.as_str(), unit.sub.as_str()),
        ("active", "running")
    );
}
