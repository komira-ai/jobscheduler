//! A native run's account that the helper could not delete: the runtime ends the run and
//! answers its removal with the accounts held (`RuntimeError::account_held`), and the
//! lifecycle goes on with the cleanup, reports it, and taints the host from two held accounts
//! (`account-not-deleted`), which the agent never clears by itself. The runtime is the fake
//! behind the [`Deferring`] gate, which answers the removals a test names that way.

use js_agent::lifecycle::taint::{ACCOUNTS_HELD_TAINT, reason};
use js_agent::lifecycle::{Event, clearable, code};
use js_agent_wire::Taint;
use serde_json::json;

use super::deferring::{ACCOUNT_HELD, Deferring};
use super::{Harness, cleanup, job, names, reply_json};

/// The wait between re-checks of a taint a re-check may clear, in milliseconds.
const EVERY: u64 = 60_000;

/// The taint of the second held account, which ended `run_id`.
fn not_deleted(run_id: &str) -> Taint {
    Taint {
        reason: reason::ACCOUNT_NOT_DELETED.to_owned(),
        run_id: Some(run_id.to_owned()),
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
/// names the run and the finding's code, is written to the marker before the finding, and the
/// cleanup still goes on and frees the slot, and a start is refused as the host is tainted.
/// Catches a count that is not read (the second account never taints) and a taint that stops
/// the cleanup.
#[test]
fn the_second_held_account_taints_the_host() {
    let (mut h, gate) = Deferring::harness("account-held-second");
    gate.hold_accounts(&[1, 2]);
    assert_eq!(names(&finish(&mut h, "r1"))[1], "finding r1 account-held");
    assert!(!h.life.is_tainted(), "one held account does not taint");

    let events = finish(&mut h, "r2");
    let mut expected = vec![
        "result r2".to_owned(),
        "tainted r2 account-not-deleted".to_owned(),
        "finding r2 account-held".to_owned(),
    ];
    expected.extend(cleanup("r2"));
    assert_eq!(names(&events), expected);
    assert_eq!(h.life.pending_taint(), Some(not_deleted("r2")));
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
    assert!(!clearable(&not_deleted("r2")));
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
    assert_eq!(h.life.pending_taint(), Some(not_deleted("r2")));
    h.life.apply(&reply_json(&json!({ "taint_ack": true })));
    for _ in 0..4 {
        h.clock.advance(EVERY);
        h.life.step();
    }
    assert!(!rechecked(&h));
    assert!(h.life.is_tainted());
    assert_eq!(h.life.pending_taint_clear(), None);
}
