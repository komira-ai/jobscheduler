//! `prepare` (host-updates design, 4.2), on the fakes: what it refuses and with which status,
//! what it records before it waits, how it waits and renews, and how it ends when the bound
//! passes. The unit and the link are never touched in `prepare`; the call log shows it.

use std::cell::RefCell;
use std::rc::Rc;

use js_agent::update_hook::{Class, Cordon, Done, Outcome, Reason, SCHEMA, Units};
use serde_json::Value;

use super::step_rig::{
    DRAIN_WAIT_SECS, MAX_SECS, POLL_SECS, RETRY_AFTER_SECS, Rig, START, Spy, UNIT, digest, line,
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
    // A lease of 30 s, renewed when less than half of it is left: after the 20 s and 40 s polls.
    let rig = Rig::with_cordon("hook-prepare-clean", 30, MAX_SECS);
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    let rig_ref = &rig;
    spy.on_pause(move |spy, ticks| {
        sink.borrow_mut()
            .push((ticks, rig_ref.record(), rig_ref.cordon_expiry()));
        if ticks == 5 {
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
    assert_eq!(rig.world.ticks(), 5, "one pause per poll that saw a lease");
    // The version check was made through the runner, on the staged agent.
    let staged = rig.params.staged_release.join("js-agent");
    assert_eq!(spy.log()[0], format!("run({} --version)", staged.display()));
    // The record was durable before the first pause, with the cordon placed and confirmed.
    let seen = seen.borrow();
    let (ticks, before_wait, _) = &seen[0];
    assert_eq!(*ticks, 1);
    let record = before_wait.as_ref().expect("the record exists at the wait");
    assert_eq!(
        record.cordon,
        Cordon {
            placed: true,
            provisional: false,
            first_ms: START,
            expires_ms: START + 30_000
        }
    );
    // The cordon moves on only when less than half the lease is left.
    let expiries: Vec<_> = seen.iter().map(|(_, _, expiry)| expiry.unwrap()).collect();
    assert_eq!(
        expiries,
        [30_000, 30_000, 50_000, 50_000, 70_000].map(|ms| START + ms)
    );
    let end = rig.record().unwrap();
    assert!(end.cordon.placed);
    assert_eq!(end.cordon.first_ms, START);
    assert_eq!(end.cordon.expires_ms, START + 70_000);
    assert_eq!(rig.cordon_expiry(), Some(end.cordon.expires_ms));
    // What the record says of the update: the digest is the updater's.
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
    assert_eq!(acts, vec!["cordon(30)"; 3]);
    assert!(rig.host().cordon.is_some(), "left for cutover");
}

#[test]
fn a_host_with_no_lease_needs_no_wait() {
    let rig = Rig::new("hook-prepare-idle");
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(rig.world.ticks(), 0);
    assert_eq!(spy.log().len(), 2, "the version check and one cordon");
    assert!(rig.record().unwrap().cordon.placed);
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
    // The unit's runtime is `crun` on Linux and `native` on a Mac, where the agent has no crun.
    let ok = if cfg!(target_os = "macos") {
        [
            "js-agent 1.4.0\n",
            "js-agent 1.4.0\nruntimes: docker native\n",
            "js-agent 1.4.0\nruntimes: native\n",
        ]
    } else {
        [
            "js-agent 1.4.0\n",
            "js-agent 1.4.0\nruntimes: docker crun\n",
            "js-agent 1.4.0\nruntimes: crun\n",
        ]
    };
    for (n, printed) in ok.into_iter().enumerate() {
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
    assert_eq!(done.report.class, Class::Environment);
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

#[test]
fn the_drain_wait_and_the_retry_are_the_params_not_constants() {
    let mut rig = Rig::new("hook-prepare-short-wait");
    rig.params.drain_wait_secs = 120;
    rig.params.defer_retry_secs = 45;
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.report.retry_after_secs, Some(45));
    assert_eq!(rig.world.ticks() as u64, 120 / POLL_SECS);
    assert_eq!(rig.cordon_expiry(), None);
    assert!(done.report.detail.unwrap().contains("120 s"));
}

#[test]
fn a_rerun_with_another_artifact_digest_is_refused_and_changes_nothing() {
    let mut rig = Rig::new("hook-prepare-other-digest");
    rig.put_record(&rig.fresh_record());
    rig.params.artifact_digest = format!("sha256:{}", "f".repeat(64));
    let before = rig.record_bytes();
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(rig.record_bytes(), before);
    assert_eq!(spy.log(), Vec::<String>::new());
}

#[test]
fn a_fresh_record_comes_after_the_taint_check_and_never_hashes_the_release() {
    // A taint halts before the record is built, whatever the order would break.
    let mut rig = Rig::new("hook-prepare-taint-before-record");
    rig.taint();
    rig.params.previous_release = Some(rig.params.staged_release.clone());
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Halt);
    // The record's digest is the updater's, though the staged tree has no agent file at all.
    let rig = Rig::new("hook-prepare-no-rehash");
    std::fs::remove_file(rig.params.staged_release.join("js-agent")).unwrap();
    let spy = Spy::new(&rig);
    assert_eq!(spy.prepare(&rig).outcome, Outcome::Success);
    assert_eq!(rig.record().unwrap().update.artifact_digest, digest());
}

#[test]
fn a_declared_prepare_timeout_shorter_than_the_wait_plus_the_margin_is_state_invalid() {
    let mut rig = Rig::new("hook-prepare-timeout");
    rig.declaration.timeouts.prepare = 3659;
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    let detail = done.report.detail.unwrap();
    assert!(
        detail.contains("3659") && detail.contains("3600"),
        "{detail}"
    );
    nothing_changed(&rig, &spy);

    // A record that is there is refused the same way, by any step.
    let mut rig = Rig::new("hook-prepare-timeout-cleanup");
    rig.put_record(&rig.fresh_record());
    rig.declaration.timeouts.prepare = 3659;
    let spy = Spy::new(&rig);
    let done = spy.cleanup(&rig);
    assert_eq!(done.report.reason, Some(Reason::StateInvalid));
    assert_eq!(spy.log(), Vec::<String>::new());
}

#[test]
fn a_retry_of_zero_defers_with_retry_after_zero() {
    let mut rig = Rig::new("hook-prepare-retry-zero");
    rig.params.drain_wait_secs = 20;
    rig.params.defer_retry_secs = 0;
    rig.set_leases(1);
    let spy = Spy::new(&rig);
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Deferred);
    assert_eq!(done.report.retry_after_secs, Some(0));
    assert_eq!(line(&done)["retry_after_secs"], 0);
}

#[cfg(target_os = "macos")]
#[test]
fn a_staged_agent_that_lists_crun_but_not_native_is_refused_on_a_mac() {
    let rig = Rig::new("hook-prepare-version-mac-crun");
    let spy = Spy::new(&rig);
    *spy.version.borrow_mut() = Ok("js-agent 1.4.0\nruntimes: docker crun\n".to_owned());
    let done = spy.prepare(&rig);
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::VersionMismatch));
    nothing_changed(&rig, &spy);
}
