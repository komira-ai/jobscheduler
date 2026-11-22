//! A native run's account that the helper would not delete: the helper's refusal of a step of
//! the end leaves the account held (its lease stays live in the journal, its uid held), the
//! runtime goes on without the run and says how many accounts are held, the lifecycle taints
//! the host from two, and the runtime ends each held account again at the end of the next job
//! and at its start. A failure that is not a refusal (the socket, a helper that needs its boot
//! reset) is not a hold. The helper is the fake that speaks the real protocol, which refuses a
//! verb of one lease, or of every lease, when a test says so.

use std::fs;

use js_agent::lifecycle::{Event, RunHandle, Runtime as _, Status};
use js_agent::native::{Journal, NativeRuntime};
use js_agent_wire::Taint;
use js_mac_session::helper::RESET_NEEDED;

use super::lifecycle::{Native, job};
use super::{Rig, TERM, config, native};
use crate::lifecycle::{cleanup, names};

/// What the helper says when a deletion's proof does not hold.
const BUSY: &str = "not shown gone: a record has uid 612";

/// What the client makes of [`BUSY`].
const BUSY_SAID: &str = "kbf-mac-session refused: not shown gone: a record has uid 612";

fn journal_of(rig: &Rig) -> Journal {
    Journal::new(&rig.dir.path("native-leases"))
}

/// The leases the journal of `rig` holds live.
fn live(rig: &Rig) -> Vec<String> {
    journal_of(rig).live().unwrap()
}

/// A runtime of the same helper and journal as `rig`'s, as a later process of the agent
/// makes one (its leases are `8.0`, `8.1` and so on).
fn later(rig: &Rig) -> NativeRuntime {
    NativeRuntime::open(rig.fake.client(), config(&rig.fake), TERM + 1).unwrap()
}

fn handle(lease: &str) -> RunHandle {
    RunHandle(lease.to_owned())
}

/// Asserts what a deletion the helper refuses leaves: the removal fails with what the helper
/// said and how many accounts are held, the run is gone from the runtime, the lease stays live
/// in the journal and its home stays, and a second account that is refused makes two. A kill
/// the helper refuses holds the account the same way and asks for no delete (a process of the
/// user may still run). It catches a refusal that keeps the run for the lifecycle to remove
/// again for ever, a lease marked done while its account is there, and a count that does not
/// grow.
#[test]
fn a_refused_step_holds_the_account_and_the_lease_stays_in_the_journal() {
    let mut rig = Rig::new("held-refused");
    let first = rig.start("r1", "/bin/true", &[]);
    rig.fake.refuse("user-delete", BUSY);
    let error = rig.rt.remove(&first).unwrap_err();
    assert_eq!(error.0, format!("user-delete: {BUSY_SAID}"));
    assert_eq!(error.held_accounts(), Some(1));
    assert_eq!(rig.rt.poll(&first).unwrap(), Status::Gone);
    assert_eq!(live(&rig), ["7.0"]);
    assert!(rig.fake.home_of("7.0").is_dir());

    rig.fake.allow("user-delete");
    let second = rig.start("r2", "/bin/true", &[]);
    rig.fake
        .refuse("kill-uid 7.1", "a process of uid 613 is left");
    let error = rig.rt.remove(&second).unwrap_err();
    assert_eq!(
        error.0,
        "kill-uid: kbf-mac-session refused: a process of uid 613 is left"
    );
    assert_eq!(error.held_accounts(), Some(2));
    assert_eq!(rig.fake.verbs().last().unwrap(), "kill-uid 7.1");
    assert_eq!(live(&rig), ["7.0", "7.1"]);
}

/// Asserts that the end of the next job asks the helper again for each held account: a job
/// that ends well tries the held lease after its own removal, which the helper may refuse
/// again (the lease stays live and held), and once the helper deletes the account the next
/// job's end retires the lease from the journal and does not ask again. It catches a held
/// account that nothing ever ends, a lease retired while the helper still refuses it, and one
/// asked about after it was ended.
#[test]
fn the_next_jobs_end_tries_a_held_account_again_and_retires_it_when_it_is_gone() {
    let mut rig = Rig::new("held-retry");
    let first = rig.start("r1", "/bin/true", &[]);
    rig.fake.refuse("user-delete 7.0", BUSY);
    assert_eq!(rig.rt.remove(&first).unwrap_err().held_accounts(), Some(1));

    let second = rig.start("r2", "/bin/true", &[]);
    rig.rt.remove(&second).unwrap();
    assert_eq!(live(&rig), ["7.0"], "the helper still refuses it");
    let verbs = rig.fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 4..],
        [
            "kill-uid 7.1",
            "user-delete 7.1",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );

    rig.fake.allow("user-delete 7.0");
    let third = rig.start("r3", "/bin/true", &[]);
    rig.rt.remove(&third).unwrap();
    assert!(live(&rig).is_empty());
    assert!(!rig.fake.home_of("7.0").exists());

    let fourth = rig.start("r4", "/bin/true", &[]);
    rig.rt.remove(&fourth).unwrap();
    let asked = rig.fake.verbs();
    let asked = asked.iter().filter(|verb| *verb == "user-delete 7.0");
    assert_eq!(
        asked.count(),
        3,
        "refused twice, ended once, not asked again"
    );
}

/// Asserts that a start that failed and could not be undone (the helper refuses the kill and
/// the delete as well as the account) holds its lease like a removal does: the lease stays
/// live in the journal, and the end of the next job retires it once the helper allows it. It
/// catches a failed start whose lease nothing ends before the next start of the agent.
#[test]
fn a_start_that_could_not_be_undone_holds_its_lease_until_a_job_ends() {
    let mut rig = Rig::new("held-discard");
    rig.fake.refuse("user-create", "no free uid in 600-699");
    rig.fake.refuse("kill-uid", "stuck");
    rig.fake.refuse("user-delete", "busy");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(live(&rig), ["7.0"]);

    for verb in ["user-create", "kill-uid", "user-delete"] {
        rig.fake.allow(verb);
    }
    let id = rig.start("r2", "/bin/true", &[]);
    rig.rt.remove(&id).unwrap();
    assert!(live(&rig).is_empty());
    let verbs = rig.fake.verbs();
    assert_eq!(
        verbs[verbs.len() - 4..],
        [
            "kill-uid 7.1",
            "user-delete 7.1",
            "kill-uid 7.0",
            "user-delete 7.0"
        ]
    );
}

/// Asserts that a runtime that starts holds the lease an earlier process left whose end the
/// helper refuses (it is in the journal, and is told), and ends it with the end of its first
/// job once the helper allows it. It catches a lease that stays live for ever because only the
/// start's one try was made.
#[test]
fn a_start_that_cannot_end_a_lease_holds_it_until_a_job_ends() {
    let rig = Rig::new("held-start");
    fs::write(rig.dir.path("native-leases"), "+ 5.1\n").unwrap();
    rig.fake.refuse("user-delete 5.1", BUSY);
    let mut next = later(&rig);
    assert_eq!(
        next.reclaim(),
        [format!(
            "cannot end the lease user 5.1: user-delete: {BUSY_SAID}"
        )]
    );
    assert_eq!(live(&rig), ["5.1"]);

    rig.fake.allow("user-delete 5.1");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let id = next.create_and_start(&spec).unwrap();
    assert_eq!(id, handle("8.0"));
    next.remove(&id).unwrap();
    assert!(live(&rig).is_empty());
}

/// Asserts that a lease the helper's boot reset says it left held is written to the journal
/// when the journal lacks it, and held: with no `reclaim` made, the end of the first job ends
/// it. A lease the journal has already is not written again, and a boot reset the runtime asks
/// for again after the helper restarted is read the same way. It catches a held lease no
/// process of the agent knows of, which nothing would ever end.
#[test]
fn a_lease_the_boot_reset_left_held_is_journaled_and_ended_by_a_job() {
    let rig = Rig::new("held-boot");
    rig.fake.boot_holds(&["4.2"]);
    rig.fake.refuse("user-delete 4.2", BUSY);
    let mut next = later(&rig);
    assert_eq!(live(&rig), ["4.2"]);
    rig.fake.allow("user-delete 4.2");
    let spec = rig.spec("r1", native("/bin/true", &[]));
    let id = next.create_and_start(&spec).unwrap();
    next.remove(&id).unwrap();
    assert!(live(&rig).is_empty());

    let rig = Rig::new("held-boot-known");
    fs::write(rig.dir.path("native-leases"), "+ 4.2\n").unwrap();
    rig.fake.boot_holds(&["4.2"]);
    let _next = later(&rig);
    let text = fs::read_to_string(rig.dir.path("native-leases")).unwrap();
    assert_eq!(text, "+ 4.2\n");

    let mut rig = Rig::new("held-boot-again");
    rig.fake.refuse("user-create", RESET_NEEDED);
    let spec = rig.spec("r1", native("/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    rig.fake.allow("user-create");
    rig.fake.boot_holds(&["4.3"]);
    rig.start("r2", "/bin/true", &[]);
    assert_eq!(live(&rig), ["4.3", "7.1"]);
}

/// Asserts that only a refusal is a hold: a helper that hangs up on the delete, and one that
/// needs its boot reset again, fail the removal without a held count, keep the run in the
/// runtime to be removed again, and leave the lease live, as a failed removal always did; the
/// removal that works after them is whole, and asks for the boot reset the helper needed. It
/// catches a socket error taken for a held account, which would leave the run's files and its
/// lease while the lifecycle moves on.
#[test]
fn a_socket_error_or_a_helper_needing_its_reset_is_not_a_hold() {
    let mut rig = Rig::new("held-not");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.fake.hang_up("user-delete");
    let error = rig.rt.remove(&id).unwrap_err();
    assert!(error.0.starts_with("user-delete: "), "{}", error.0);
    assert_eq!(error.held_accounts(), None);
    assert_ne!(rig.rt.poll(&id).unwrap(), Status::Gone, "the run is kept");
    assert_eq!(live(&rig), ["7.0"]);

    rig.fake.allow("user-delete");
    rig.fake.refuse("kill-uid", RESET_NEEDED);
    let error = rig.rt.remove(&id).unwrap_err();
    assert!(error.0.starts_with("kill-uid: "), "{}", error.0);
    assert_eq!(error.held_accounts(), None);

    rig.fake.allow("kill-uid");
    rig.rt.remove(&id).unwrap();
    assert!(live(&rig).is_empty());
    assert!(!rig.fake.home_of("7.0").exists());
    let resets = rig.fake.verbs();
    let resets = resets.iter().filter(|verb| *verb == "boot-reset");
    assert_eq!(resets.count(), 2, "the open and the ask the helper needed");
}

/// Asserts the whole path through the lifecycle: the first job whose account the helper
/// refuses to delete ends with its result and a finding that says what the helper said, the
/// cleanup goes through every step and frees the slot, the host is not tainted and the lease
/// is live in the journal; the second job's end taints the host `account-not-deleted` before
/// its finding, and the next start is refused as the host is tainted. A removal that fails on
/// the socket taints as `cleanup-failed` as before. It catches a held account answered as a
/// failed removal (a taint at once, and a slot kept for ever) and a second one that taints
/// nothing.
#[test]
fn one_held_account_is_a_finding_and_two_taint_the_host() {
    let mut h = Native::new("held-life");
    h.fake.refuse("user-delete", BUSY);
    assert_eq!(h.start(job("r1", "/bin/true")), ["started r1"]);
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.busy_slots().is_empty());
    let mut want = vec!["result r1".to_owned(), "finding r1 account-held".to_owned()];
    want.extend(cleanup("r1"));
    assert_eq!(names(&events), want);
    let said: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Finding(finding) => Some(finding.message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(said, [format!("user-delete: {BUSY_SAID}")]);
    assert!(!h.life.is_tainted());
    let journal = Journal::new(&h.dir.path("native-leases"));
    assert_eq!(journal.live().unwrap(), ["7.0"]);

    assert_eq!(h.start(job("r2", "/bin/true")), ["started r2"]);
    h.fake.exit("7.1", 0);
    let events = h.until(|life| life.busy_slots().is_empty());
    let mut want = vec![
        "result r2".to_owned(),
        "tainted r2 account-not-deleted".to_owned(),
        "finding r2 account-held".to_owned(),
    ];
    want.extend(cleanup("r2"));
    assert_eq!(names(&events), want);
    assert_eq!(
        h.life.pending_taint(),
        Some(Taint {
            reason: "account-not-deleted".to_owned(),
            run_id: Some("r2".to_owned()),
            check: Some("account-held".to_owned()),
        })
    );
    assert_eq!(h.start(job("r3", "/bin/true")), ["refused r3 host-tainted"]);
    assert_eq!(journal.live().unwrap(), ["7.0", "7.1"]);

    let mut h = Native::new("held-life-socket");
    h.fake.hang_up("user-delete");
    h.start(job("r1", "/bin/true"));
    h.fake.exit("7.0", 0);
    h.until(|life| life.is_tainted());
    let taint = h.life.pending_taint().unwrap();
    assert_eq!(taint.reason, "cleanup-failed");
    assert_eq!(taint.check.as_deref(), Some("container-remove-failed"));
}
