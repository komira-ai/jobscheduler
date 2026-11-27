//! `drain` (hostkeeper's maintenance-updates design, 5.2 to 5.4), in process over the fakes'
//! world: what it writes, what it cordons, how long it waits and how it ends. `resume` is in
//! steps_resume.rs; both run over the rig of maintain_rig.rs.

use std::fs;

use js_agent::lifecycle::hold::HOLD_FILE;
use js_agent::update_hook::{Class, Done, Lease, LeaseError, Outcome, POLL_SECS, Reason, Report};

use super::fakes::{FACTS, START_MS};
use super::maintain_rig::{
    LEASE_SECS, Probe, RETRY_SECS, Rig, environment_failure, hello, note_text, set_job, succeeded,
};

/// The words of a drain that ran out of time.
const DEADLINE_DETAIL: &str = "the drain deadline passed with work on the host: the host is back \
                               in service and no work was stopped";

/// The changes a drain makes, each after a kill point.
const DRAIN_CHANGES: [&str; 5] = [
    "lease.cordon",
    "lease.uncordon",
    "hold.hold",
    "hold.release",
    "hold.released",
];

/// What a deferred drain ends as.
fn deferred() -> Done {
    Done {
        outcome: Outcome::Deferred,
        report: Report {
            reason: Some(Reason::DrainDeadline),
            class: Class::Environment,
            retry_after_secs: Some(RETRY_SECS),
            detail: Some(DEADLINE_DETAIL.to_owned()),
            ..Report::default()
        },
    }
}

/// Asserts that a drain of an idle host writes the hold file with its note, cordons the host
/// for the cordon's lease, and returns at once, with no pause. It catches a drain that waits
/// when nothing runs, a hold file without the note and a cordon of another length.
#[test]
fn drain_of_an_idle_host_cordons_holds_and_returns_without_a_pause() {
    let rig = Rig::new("hook-drain-idle");
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.expiry(), Some(START_MS + LEASE_SECS * 1000));
    assert_eq!(rig.world.ticks(), 0);
    assert_eq!(probe.count("clock.pause"), 0);
}

/// Asserts that a drain of a host with a job returns only after the job ended, that it read the
/// host while the job ran, asked for the cordon once, and that the cordon is in force when it
/// returns. It catches a drain that returns before the work ends or leaves the cordon unplaced.
#[test]
fn drain_with_a_running_job_returns_only_after_the_job_ends() {
    let rig = Rig::new("hook-drain-busy");
    rig.busy();
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 3 {
            set_job(p.world, false);
        }
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.world.ticks(), 3);
    // One read before the hold, three that found the job, one that found the host idle.
    assert_eq!(probe.count("lease.host"), 5);
    assert_eq!(probe.count("lease.cordon"), 1);
    assert_eq!(rig.expiry(), Some(START_MS + LEASE_SECS * 1000));
    assert!(rig.hold_text().is_some());
}

/// Asserts that each sign of work on the host keeps the drain waiting: a lease, a lease count,
/// a host that is releasing, a host that is set aside. It catches an idle test that misses one
/// of them.
#[test]
fn each_sign_of_work_keeps_the_drain_waiting_until_the_deadline() {
    for name in ["lease", "leases", "releasing", "mark"] {
        let mut rig = Rig::new(&format!("hook-drain-sign-{name}"));
        rig.deadline_in(15);
        rig.world.alter_host(|host| match name {
            "lease" => host.lease = Some("lease-1".to_owned()),
            "leases" => host.leases = 1,
            "releasing" => host.releasing = true,
            _ => host.mark = Some("draining".to_owned()),
        });
        let probe = Probe::new(&rig);
        assert_eq!(probe.drain(&rig.params), deferred(), "{name}");
        assert_eq!(rig.world.ticks(), 2, "{name}");
    }
}

/// Asserts that a deadline that passes while the host is busy puts the host back (no hold file,
/// no renamed file, no cordon) and ends deferred with the reason `drain_deadline`, the class
/// environment, the retry hint of the params and the exact detail. It catches a deferral that
/// leaves the host cordoned or held, or one that says nothing of why.
#[test]
fn a_deadline_that_passes_while_busy_puts_the_host_back_and_defers() {
    let mut rig = Rig::new("hook-drain-deadline");
    rig.deadline_in(25);
    rig.busy();
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), deferred());
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.expiry(), None);
    assert_eq!(rig.world.ticks(), 3);
    assert_eq!(probe.count("lease.cordon"), 1);
    assert_eq!(probe.count("lease.uncordon"), 1);
}

/// Asserts that a deadline that has passed does not fail a drain of an idle host: the host is
/// idle, so the drain succeeds, held and cordoned. It catches a deadline that is checked before
/// the host is read.
#[test]
fn a_deadline_already_passed_on_an_idle_host_still_succeeds() {
    let mut rig = Rig::new("hook-drain-deadline-idle");
    rig.params.drain_deadline_unix = Some(1);
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert_eq!(rig.expiry(), Some(START_MS + LEASE_SECS * 1000));
    assert_eq!(rig.world.ticks(), 0);
}

/// Asserts that a deadline that passes while the server still does not know the host defers
/// the drain as well, and that the uncordon the server answers with `unknown-host` does not
/// fail it. It catches a deferral that ends in a lease failure, or a drain that waits for a host
/// the server never learns of.
#[test]
fn a_deadline_that_passes_before_the_server_knows_the_host_defers() {
    let mut rig = Rig::unseen("hook-drain-deadline-unknown");
    rig.deadline_in(15);
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), deferred());
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(rig.world.ticks(), 2);
    assert_eq!(probe.count("lease.cordon"), 3);
    assert_eq!(probe.count("lease.uncordon"), 1);
}

/// Asserts that a deferred drain leaves in force a cordon that was there before the drain, with
/// its expiry, and makes no uncordon call. It catches a deferral that lifts an operator's
/// cordon.
#[test]
fn a_deferred_drain_leaves_a_cordon_that_was_there_before() {
    let mut rig = Rig::new("hook-drain-deadline-before");
    rig.deadline_in(15);
    rig.busy();
    let operator = rig.world.cordon(900).unwrap().host.cordon.unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), deferred());
    assert_eq!(rig.expiry(), Some(operator.expires_at_ms));
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), None);
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that a drain run twice is one cordon and one hold: the second finds the hold and
/// writes nothing, asks for the cordon again and leaves its expiry, and the hold file's text
/// (and so its note, which says no cordon was in force before) is the same. It catches a second
/// drain that notes its own cordon as an operator's, or one that moves the expiry.
#[test]
fn drain_twice_is_one_cordon_and_one_hold() {
    let rig = Rig::new("hook-drain-twice");
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    let (text, expiry) = (rig.hold_text(), rig.expiry());
    assert_eq!(text, Some(note_text(false, Some(FACTS))));
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), text);
    assert_eq!(rig.expiry(), expiry);
    assert_eq!(probe.count("hold.hold"), 1);
    assert_eq!(probe.count("lease.cordon"), 2);
}

/// Asserts that a drain whose host the server does not know yet (its hello has not arrived)
/// writes the hold, pauses without reading the host, and cordons as soon as the hello is
/// delivered at a pause. It catches a drain that fails on `unknown-host`, one that reads a
/// host that is not there, and one that never cordons.
#[test]
fn drain_waits_for_the_server_to_know_the_host_and_then_cordons() {
    let rig = Rig::unseen("hook-drain-unknown");
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 2 {
            hello(p);
        }
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert_eq!(rig.world.ticks(), 2);
    assert_eq!(probe.count("lease.cordon"), 3);
    // The read before the hold, and the read that found the host idle.
    assert_eq!(probe.count("lease.host"), 2);
    assert_eq!(rig.expiry(), Some(START_MS + 20_000 + LEASE_SECS * 1000));
}

/// Asserts that a cordon in force before the drain is noted in the hold file as
/// `cordoned_before=true`, and that the drain succeeds with the cordon in force. It catches a
/// note that forgets the operator's cordon, which `resume` would then lift.
#[test]
fn a_cordon_in_force_before_the_drain_is_noted_in_the_hold() {
    let rig = Rig::new("hook-drain-before");
    let operator = rig.world.cordon(900).unwrap().host.cordon.unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(true, Some(FACTS))));
    assert_eq!(rig.expiry(), Some(operator.expires_at_ms));
}

/// Asserts that a hold a `resume` renamed and did not finish is written again by a drain with
/// its own note (not a new one made from what the host reads now), and that the drain succeeds.
/// It catches a drain that forgets the note of the hold it takes up again.
#[test]
fn a_hold_a_killed_resume_left_is_written_again_with_its_note() {
    let rig = Rig::new("hook-drain-releasing");
    rig.world.cordon(900).unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    let text = rig.hold_text();
    assert_eq!(text, Some(note_text(true, Some(FACTS))));
    rig.rename_hold();
    rig.world.uncordon().unwrap();
    rig.world.set_facts("os=linux arch=aarch64");
    assert_eq!(rig.hold_text(), None);
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), text);
    assert_eq!(probe.count("hold.hold"), 2);
}

/// Asserts that facts the drain cannot read leave a note without them: the hold file holds the
/// text and the cordon line only, and the drain succeeds. It catches a drain that fails on
/// facts, or one that writes a facts line it never read.
#[test]
fn facts_unreadable_at_the_drain_leave_a_note_without_facts() {
    let rig = Rig::new("hook-drain-facts");
    let probe = Probe::new(&rig);
    probe.fail("facts.read", 1, "gpu probe failed");
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.hold_text(), Some(note_text(false, None)));
}

/// Asserts that a drain asks for the cordon again once half of `cordon_lease_secs` has passed
/// (at the polls at 20 s and 40 s of a 30 s lease, three calls in all), and that each ask
/// extends the expiry. It catches a drain that lets the cordon lapse during a long wait.
#[test]
fn the_cordon_is_asked_again_once_half_the_lease_has_passed() {
    let mut rig = Rig::new("hook-drain-renew");
    rig.params.cordon_lease_secs = 30;
    rig.busy();
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 5 {
            set_job(p.world, false);
        }
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(probe.count("lease.cordon"), 3);
    assert_eq!(rig.expiry(), Some(START_MS + 40_000 + 30_000));
}

/// Asserts that the cordon is not asked again before half the lease has passed: a 10 s wait on
/// a 30 s lease makes one call. It catches a drain that asks at every poll.
#[test]
fn the_cordon_is_not_asked_again_before_half_the_lease() {
    let mut rig = Rig::new("hook-drain-no-renew");
    rig.params.cordon_lease_secs = 30;
    rig.busy();
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 1 {
            set_job(p.world, false);
        }
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(probe.count("lease.cordon"), 1);
}

/// Asserts that the cordon is asked again at exactly half the lease: 10 s into a 20 s lease. It
/// catches a comparison that waits for more than half.
#[test]
fn the_cordon_is_asked_again_at_exactly_half_the_lease() {
    let mut rig = Rig::new("hook-drain-renew-half");
    rig.params.cordon_lease_secs = 2 * POLL_SECS;
    rig.busy();
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| {
        if taken == 1 {
            set_job(p.world, false);
        }
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(probe.count("lease.cordon"), 2);
}

/// Asserts that a server that forgets the host during the wait (it restarted and the host has
/// not said hello) is asked for the cordon again once it knows the host: the drain cordons a
/// second time, and the cordon is in force when it returns. It catches a drain that fails on
/// `unknown-host` after the cordon, or that waits for an idle host without a cordon.
#[test]
fn a_server_that_forgets_the_host_during_the_wait_is_asked_to_cordon_again() {
    let rig = Rig::new("hook-drain-forgot");
    rig.busy();
    let probe = Probe::new(&rig);
    probe.at_pause(|p, taken| match taken {
        1 => p.world.lose_state_file(),
        _ => hello(p),
    });
    assert_eq!(probe.drain(&rig.params), succeeded());
    assert_eq!(rig.world.ticks(), 2);
    assert_eq!(probe.count("lease.cordon"), 2);
    assert_eq!(rig.expiry(), Some(START_MS + 20_000 + LEASE_SECS * 1000));
}

/// Asserts that a server that refuses the first read of the host fails the drain with
/// `lease_unreachable`, class environment and the server's words, before anything is written
/// or cordoned. It catches a drain that holds a host it could not read.
#[test]
fn a_refused_first_read_fails_the_drain_before_the_hold() {
    let rig = Rig::new("hook-drain-read-refused");
    let probe = Probe::new(&rig);
    probe.fail_lease(
        "lease.host",
        1,
        LeaseError::Refused("host-certificate-required".to_owned()),
    );
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(
            Reason::LeaseUnreachable,
            "refused: host-certificate-required"
        )
    );
    assert_eq!(rig.hold_text(), None);
    assert_eq!(probe.count("lease.cordon"), 0);
}

/// Asserts that a server that cannot be reached when the drain cordons fails it with
/// `lease_unreachable` and the transport's words, and that the hold file stays (the agent keeps
/// the host cordoned). It catches a drain that removes the hold or ends well without a cordon.
#[test]
fn an_unreachable_server_at_the_cordon_fails_the_drain_and_the_hold_stays() {
    let rig = Rig::new("hook-drain-cordon-unreachable");
    let probe = Probe::new(&rig);
    probe.fail_lease(
        "lease.cordon",
        1,
        LeaseError::Unreachable("connection refused".to_owned()),
    );
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::LeaseUnreachable, "connection refused")
    );
    assert_eq!(rig.hold_text(), Some(note_text(false, Some(FACTS))));
    assert_eq!(rig.expiry(), None);
}

/// Asserts that a server that refuses the read of the host during the wait fails the drain
/// with `lease_unreachable`, and that the hold and the cordon stay. It catches a drain that
/// treats a refusal as work still running and polls on.
#[test]
fn a_refused_read_during_the_wait_fails_the_drain_and_the_hold_stays() {
    let rig = Rig::new("hook-drain-wait-refused");
    let probe = Probe::new(&rig);
    probe.fail_lease("lease.host", 2, LeaseError::Refused("http-500".to_owned()));
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::LeaseUnreachable, "refused: http-500")
    );
    assert!(rig.hold_text().is_some());
    assert!(rig.expiry().is_some());
    assert_eq!(rig.world.ticks(), 0);
}

/// Asserts that a deferred drain whose uncordon the server cannot answer fails with
/// `lease_unreachable` instead of deferring, and leaves the renamed hold file for a `resume` to
/// finish. It catches a deferral that claims the host is back in service when it is not.
#[test]
fn a_failed_uncordon_in_a_deferral_fails_the_drain_and_leaves_the_renamed_hold() {
    let mut rig = Rig::new("hook-drain-deadline-uncordon");
    rig.deadline_in(15);
    rig.busy();
    let probe = Probe::new(&rig);
    probe.fail_lease(
        "lease.uncordon",
        1,
        LeaseError::Unreachable("timed out".to_owned()),
    );
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::LeaseUnreachable, "timed out")
    );
    assert_eq!(rig.hold_text(), None);
    assert_eq!(rig.resume_text(), Some(note_text(false, Some(FACTS))));
    assert!(rig.expiry().is_some());
}

/// Asserts that a hold file that cannot be read fails the drain with `hold_failed`, class
/// environment and the exact detail, before the lease server is called. It catches a drain that
/// goes on without knowing whether the host is held.
#[test]
fn a_hold_state_that_cannot_be_read_fails_the_drain_before_any_lease_call() {
    let rig = Rig::new("hook-drain-state-fails");
    let probe = Probe::new(&rig);
    probe.fail("hold.state", 1, "disk gone");
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: disk gone")
    );
    assert_eq!(probe.lease_calls(), 0);
}

/// Asserts that a hold file that cannot be written fails the drain with `hold_failed` and the
/// exact detail, and that no cordon was placed. It catches a drain that cordons a host it does
/// not hold.
#[test]
fn a_hold_that_cannot_be_written_fails_the_drain_and_places_no_cordon() {
    let rig = Rig::new("hook-drain-hold-fails");
    let probe = Probe::new(&rig);
    probe.fail("hold.hold", 1, "disk full");
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: disk full")
    );
    assert_eq!(probe.count("lease.cordon"), 0);
    assert_eq!(rig.expiry(), None);
}

/// Asserts that a directory in the hold file's place fails the drain with `hold_failed` and the
/// words of the state root, and that nothing is cordoned. It catches a drain that takes an
/// object that is not a hold file for none.
#[test]
fn a_directory_in_the_hold_files_place_fails_the_drain() {
    let rig = Rig::new("hook-drain-hold-dir");
    fs::create_dir(rig.root().join(HOLD_FILE)).unwrap();
    let probe = Probe::new(&rig);
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(
            Reason::HoldFailed,
            "the hold file: hold is not a regular file (a link is not accepted)"
        )
    );
    assert_eq!(probe.lease_calls(), 0);
}

/// Asserts that a hold file that cannot be renamed fails a deferred drain with `hold_failed`
/// and leaves the cordon in force, with no uncordon made. It catches a deferral that lifts the
/// cordon while the hold is still on.
#[test]
fn a_hold_that_cannot_be_renamed_fails_a_deferral_and_lifts_nothing() {
    let mut rig = Rig::new("hook-drain-deadline-release");
    rig.deadline_in(15);
    rig.busy();
    let probe = Probe::new(&rig);
    probe.fail("hold.release", 1, "rename failed");
    assert_eq!(
        probe.drain(&rig.params),
        environment_failure(Reason::HoldFailed, "the hold file: rename failed")
    );
    assert!(rig.hold_text().is_some());
    assert!(rig.expiry().is_some());
    assert_eq!(probe.count("lease.uncordon"), 0);
}

/// Asserts that each change of a drain follows a kill point and each kill point precedes one:
/// the hold, the cordon, and for a deferral the rename, the uncordon and the removal. It catches
/// a change a test cannot end the step before.
#[test]
fn every_change_of_a_drain_passes_a_kill_point_first() {
    let rig = Rig::new("hook-drain-gate");
    let probe = Probe::new(&rig);
    assert_eq!(probe.drain(&rig.params), succeeded());
    probe.assert_gated(&DRAIN_CHANGES);
    assert_eq!(probe.count("gate"), 2);

    let mut late = Rig::new("hook-drain-gate-deadline");
    late.deadline_in(15);
    late.busy();
    let probe = Probe::new(&late);
    assert_eq!(probe.drain(&late.params), deferred());
    probe.assert_gated(&DRAIN_CHANGES);
    assert_eq!(probe.count("gate"), 5);
}
