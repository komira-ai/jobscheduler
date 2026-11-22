//! A native run's account that the helper could not delete: the runtime ends the run and
//! answers its removal with the accounts held (`RuntimeError::account_held`), and the
//! lifecycle goes on with the cleanup and reports it. The host is tainted from two held
//! accounts (`account-not-deleted`) wherever the runtime counts them (`Runtime::held_accounts`:
//! after a removal, a failed start, or at the start of the agent), and the agent never clears
//! that taint by itself. The runtime is the fake behind the [`Deferring`] gate, which answers
//! the removals a test names that way and counts the accounts a test says it holds.

use js_agent::lifecycle::taint::{ACCOUNTS_HELD_TAINT, reason};
use js_agent::lifecycle::{Event, Lifecycle, clearable, code};
use js_agent_wire::Taint;
use serde_json::json;

use super::deferring::Control;
use super::deferring::{ACCOUNT_HELD, Deferring};
use super::{Harness, cleanup, hook, job, names, reply_json};

/// The wait between re-checks of a taint a re-check may clear, in milliseconds.
const EVERY: u64 = 60_000;

/// The taint of the host that holds two accounts.
fn not_deleted() -> Taint {
    Taint {
        reason: reason::ACCOUNT_NOT_DELETED.to_owned(),
        run_id: None,
        check: Some(code::ACCOUNT_HELD.to_owned()),
    }
}

/// Runs `run_id` to its end and returns the events of the step that cleans it up.
fn finish(h: &mut Harness, run_id: &str) -> Vec<Event> {
    h.start(job(run_id));
    h.fake.exit(run_id, 0);
    h.life.step()
}

/// A harness whose first two removals each leave an account held (the first with one account
/// held, the second with two), after both runs ended.
fn two_held() -> Harness {
    let (mut h, gate) = Deferring::harness("account-held-two");
    gate.hold_accounts(&[1, 2]);
    finish(&mut h, "r1");
    finish(&mut h, "r2");
    h
}

/// Asserts that one held account is a finding and not a taint: the run's result is made, the
/// finding says what the helper said, the cleanup goes on through every step (container,
/// scratch, checks), the slot is freed, and the host is not tainted (no marker is written). It
/// catches a held account answered as a failed removal, which taints the host at once and
/// keeps the slot booked.
#[test]
fn one_held_account_is_a_finding_and_no_taint() {
    let (mut h, gate) = Deferring::harness("account-held-one");
    gate.hold_accounts(&[1]);
    let events = finish(&mut h, "r1");
    let mut expected = vec!["result r1".to_owned(), "finding r1 account-held".to_owned()];
    expected.extend(cleanup("r1"));
    assert_eq!(names(&events), expected);
    let said = events.iter().find_map(|event| match event {
        Event::Finding(finding) => Some(finding.message.as_str()),
        _ => None,
    });
    assert_eq!(said, Some(ACCOUNT_HELD));
    assert!(!h.life.is_tainted());
    assert!(!h.marker().exists());
    assert_eq!(h.life.free().cpu_millis, 4000, "the slot is freed");
}

/// Asserts that the second held account taints the host `account-not-deleted`: the taint
/// carries the finding's code, is written to the marker, and the cleanup still goes on and
/// frees the slot, and a start is refused as the host is tainted. Catches a count that is not
/// read (the second account never taints) and a taint that stops the cleanup.
#[test]
fn the_second_held_account_taints_the_host() {
    let (mut h, gate) = Deferring::harness("account-held-second");
    gate.hold_accounts(&[1, 2]);
    assert_eq!(names(&finish(&mut h, "r1"))[1], "finding r1 account-held");
    assert!(!h.life.is_tainted(), "one held account does not taint");

    let events = finish(&mut h, "r2");
    let mut expected = vec!["result r2".to_owned(), "finding r2 account-held".to_owned()];
    expected.extend(cleanup("r2"));
    expected.push("tainted - account-not-deleted".to_owned());
    assert_eq!(names(&events), expected);
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));
    assert!(h.marker().exists());
    assert_eq!(h.life.free().cpu_millis, 4000, "the slots are freed");
    assert_eq!(names(&h.start(job("r3"))), ["refused r3 host-tainted"]);
}

/// Asserts the boundary of the taint: a removal that reports `ACCOUNTS_HELD_TAINT` accounts
/// held, or more, taints the host, however many there were before this agent saw one (the
/// runtime counts the accounts a start left too); one fewer does not. Catches a comparison off
/// by one.
#[test]
fn the_taint_starts_at_the_count_and_stays_above_it() {
    for held in 1..=ACCOUNTS_HELD_TAINT + 3 {
        let (mut h, gate) = Deferring::harness("account-held-count");
        gate.hold_accounts(&[held]);
        finish(&mut h, "r1");
        assert_eq!(h.life.is_tainted(), held >= ACCOUNTS_HELD_TAINT, "{held}");
    }
    assert_eq!(ACCOUNTS_HELD_TAINT, 2);
}

/// Asserts that the agent never clears the taint of held accounts by itself: no re-check runs
/// however many waits pass with the host idle and the report acknowledged, `clearable` says
/// no, and a restarted agent reads the marker and keeps the host tainted, pinned the same
/// way. Catches a reason a re-check may clear, which would put a host with accounts the helper
/// cannot delete back in service.
#[test]
fn an_account_not_deleted_taint_never_clears_itself() {
    let mut h = two_held();
    assert!(!clearable(&not_deleted()));
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));
    let rechecked = |h: &Harness| {
        let log = h.log.borrow();
        log.iter()
            .any(|line| line.starts_with("check taint-recheck"))
    };
    for _ in 0..4 {
        h.clock.advance(EVERY);
        h.life.step();
    }
    assert!(!rechecked(&h));
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);

    h.restart().unwrap();
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));
    for _ in 0..4 {
        h.clock.advance(EVERY);
        h.life.step();
    }
    assert!(!rechecked(&h));
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
}

/// A harness whose lifecycle started over a runtime that already holds `held` accounts, as one
/// whose reclaim or boot reset found some, and the events of that start.
fn started_holding(name: &str, held: usize) -> (Harness, Control, Vec<Event>) {
    let mut h = Harness::new(name);
    let (runtime, gate) = Deferring::around(h.fake.clone());
    gate.set_held(held);
    let scratch = h.dir.path("scratch");
    let checks = hook(&h.log, &h.fake, &scratch, &[]);
    let (life, events) = Lifecycle::adopt(
        h.config.clone(),
        Box::new(runtime),
        checks,
        Box::new(h.clock.clone()),
    )
    .unwrap();
    h.life = life;
    (h, gate, events)
}

/// Asserts that two accounts held at the start of the agent taint the host before any job:
/// the taint is the same `account-not-deleted`, the marker is written, the start's events say
/// it, and the first job is refused. Catches a count read only after a removal, which leaves a
/// host with two undeletable accounts in service until a third job fails.
#[test]
fn two_accounts_held_at_start_taint_before_any_job() {
    let (mut h, _gate, events) = started_holding("account-held-at-start", ACCOUNTS_HELD_TAINT);
    assert_eq!(names(&events), ["tainted - account-not-deleted"]);
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));
    assert!(h.marker().exists());
    assert_eq!(names(&h.start(job("r1"))), ["refused r1 host-tainted"]);
}

/// Asserts that one account held at start does not taint the host, and that a job then runs
/// to its end with the host in service. Catches a threshold below two.
#[test]
fn one_account_held_at_start_does_not_taint() {
    let (mut h, _gate, events) = started_holding("account-held-at-start-one", 1);
    assert!(names(&events).is_empty());
    assert!(!h.life.is_tainted());
    assert!(!h.marker().exists());
    assert_eq!(names(&h.start(job("r1")))[0], "started r1");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!h.life.is_tainted());
}

/// Asserts that accounts the runtime came to hold between calls (its boot reset ran again
/// before a stop, a start or a removal) taint the host at the next step, and at the next reply
/// when no step came first. Catches a check made only where a removal fails.
#[test]
fn accounts_the_runtime_came_to_hold_taint_at_the_next_call() {
    let (mut h, gate) = Deferring::harness("account-held-between");
    gate.set_held(ACCOUNTS_HELD_TAINT);
    assert!(!h.life.is_tainted(), "nothing was asked yet");
    assert_eq!(names(&h.life.step()), ["tainted - account-not-deleted"]);
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));

    let (mut h, gate) = Deferring::harness("account-held-between-reply");
    gate.set_held(ACCOUNTS_HELD_TAINT);
    let events = names(&h.start(job("r1")));
    assert_eq!(
        events.last().map(String::as_str),
        Some("tainted - account-not-deleted")
    );
    assert!(h.life.is_tainted());
    assert_eq!(names(&h.start(job("r2"))), ["refused r2 host-tainted"]);
}

/// Asserts that a start that failed and whose discard left the second account held taints the
/// host, and that one that left the first does not. Catches accounts held by a failed start
/// that are never counted, because no removal follows.
#[test]
fn a_failed_start_that_holds_the_second_account_taints_the_host() {
    let (mut h, gate) = Deferring::harness("account-held-start-fails");
    gate.fail_starts_holding(1);
    let events = names(&h.start(job("r1")));
    assert!(
        events.iter().any(|name| name.starts_with("finding r1")),
        "{events:?}"
    );
    assert!(!h.life.is_tainted(), "one held account does not taint");

    gate.fail_starts_holding(ACCOUNTS_HELD_TAINT);
    let events = names(&h.start(job("r2")));
    assert!(
        events.contains(&"tainted - account-not-deleted".to_owned()),
        "{events:?}"
    );
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));
    assert!(h.marker().exists());
}

/// Asserts that a count that falls back below two after a retry deleted an account does not
/// clear the taint, and that it is not sealed twice when the count rises again: the host stays
/// tainted, `clearable` says no, the report is the same, and no second taint event is made.
/// Catches a taint that follows the count, which would put a host back in service that an
/// operator has not looked at.
#[test]
fn a_count_that_falls_back_does_not_clear_the_taint() {
    let (mut h, gate, _) = started_holding("account-held-falls", ACCOUNTS_HELD_TAINT);
    assert!(h.life.is_tainted());
    gate.set_held(1);
    for _ in 0..3 {
        h.clock.advance(EVERY);
        assert!(names(&h.life.step()).is_empty());
    }
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint(), Some(not_deleted()));
    assert!(!clearable(&not_deleted()));
    assert_eq!(h.life.pending_taint_clear(), None);
    gate.set_held(ACCOUNTS_HELD_TAINT + 1);
    assert!(names(&h.life.step()).is_empty(), "sealed once");
    assert!(h.life.is_tainted());
}
