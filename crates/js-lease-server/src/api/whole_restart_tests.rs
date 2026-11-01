//! The whole-node reservations across a restart of the server, over the rig of the mark
//! tests: a server is stopped and another started over the same lease file with
//! `--boot resume`, on the same manual clock, and the agent's host says hello again. The
//! tick hands the registry's ledger to the lease table, which writes it with the leases.
//! Nothing here sleeps and no test asserts how long anything takes on a wall clock.

use js_lease::Timestamp;
use js_registry::{Exclusion, WholeCounts};
use js_types::{HostId, RequestId};

use super::whole_view_tests::{Rig, T0};
use crate::reservations::RESERVATION_BOOT_GRACE;

fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

const GRACE_MS: u64 = RESERVATION_BOOT_GRACE.as_secs() * 1000;

/// A server whose only host is busy with a share lease, where `r1` reserved the host,
/// gave it up at its deadline thirty seconds in and is kept off it for five minutes, and
/// `r2`, behind it, was set the host at that moment. The clock reads `T0` plus 31 s, and
/// the tick has handed the table the ledger.
async fn two_waiting(name: &str) -> Rig {
    let mut rig = Rig::new(name).await;
    rig.lease().await;
    rig.ask("r1");
    rig.ask("r2");
    rig.advance(31);
    rig.ask("r1");
    rig.shared.tick().await;
    rig
}

/// Asserts a restart over the same lease file with `resume`, the red test of the
/// persistence: the host is marked for the same request (`r2`) with the instant it was
/// set aside; the requests wait in the same order (`r1`, then `r2`); `r1` keeps its one
/// give-up and the five-minute exclusion of the host. The consumer then presents both
/// again and nothing moves: the mark, the places and the count are as they were. It
/// catches a restart that forgets the wait list, one that resets the give-up count, and
/// a presentation that sends a restored request to the back.
#[tokio::test]
async fn a_restart_with_resume_keeps_the_mark_the_place_and_the_give_up_count() {
    let rig = two_waiting("restart_keeps").await;
    let before = rig.registry.reservations();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].request, id("r2"));
    assert_eq!(rig.registry.request_record(&id("r1")).unwrap().give_ups, 1);

    let mut back = rig.restart(true).await;
    let after = back.registry.reservations();
    assert_eq!(after.len(), 1);
    let (mark, was) = (&after[0], &before[0]);
    assert_eq!((&mark.request, &mark.host), (&id("r2"), &was.host));
    assert_eq!((mark.at, mark.assigned), (was.at, false));
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.registry.queue_position(&id("r2")), Some(1));
    let kept = back.registry.request_record(&id("r1")).unwrap();
    assert_eq!(kept.give_ups, 1);
    let host = HostId::new("a").unwrap();
    let until = at(T0 + 31_000 + 300_000);
    assert_eq!(kept.exclusions.get(&host), Some(&Exclusion::Until(until)));
    assert_eq!(back.registry.dropped_reservations(), 0);

    back.ask("r1");
    back.ask("r2");
    assert_eq!(back.registry.reservations(), after);
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.registry.queue_position(&id("r2")), Some(1));
    assert_eq!(back.registry.request_record(&id("r1")).unwrap().give_ups, 1);
    let counts = back.registry.whole_counts();
    assert_eq!(
        counts,
        WholeCounts {
            reserved: 1,
            held: 0,
            queued: 2
        }
    );
}

/// Asserts the floor on a restored deadline. The reservation of `r1` is due thirty
/// seconds after it was made; the server is down for a hundred, so the stored deadline
/// is past at the boot. The restored deadline is the boot plus the boot grace, not the
/// stored one and not the stored one plus the grace: the request, presented at the boot,
/// keeps the host; one second short of the floor it still does; at the floor it gives the
/// host up. It catches a restart that gives up a reservation the moment its request
/// returns, and one that stretches the deadline beyond the floor.
#[tokio::test]
async fn a_deadline_that_passed_while_the_server_was_down_is_floored_not_fired() {
    let mut rig = Rig::new("restart_floor").await;
    rig.lease().await;
    rig.ask("r1");
    assert_eq!(rig.registry.reservations()[0].deadline, at(T0 + 30_000));
    rig.shared.tick().await;
    rig.advance(100);

    let mut back = rig.restart(true).await;
    let boot = T0 + 100_000;
    let mark = back.registry.reservations().remove(0);
    assert_eq!(mark.deadline, at(boot + GRACE_MS));
    assert_eq!(mark.at, at(T0));
    back.ask("r1");
    assert_eq!(back.registry.reservations(), vec![mark]);

    back.advance(RESERVATION_BOOT_GRACE.as_secs() - 1);
    back.ask("r1");
    assert_eq!(back.registry.reservations().len(), 1);
    back.advance(1);
    back.ask("r1");
    assert!(
        back.registry.reservations().is_empty(),
        "given up at the floor"
    );
    assert_eq!(back.registry.request_record(&id("r1")).unwrap().give_ups, 1);
}

/// Asserts the orphan case at the lease server. After a restart nobody presents the two
/// requests again; at the regain grace (ten minutes in the rig's pool) both are gone
/// from the wait list, the mark with them, and two restored reservations are counted
/// dropped; the give-up count of `r1` outlives the wait. The tick after hands the table
/// the empty ledger, so a second restart finds no place for either request. It catches
/// an unclaimed record kept for ever, a drop that is not counted, and a ledger that is
/// not written again after it shrinks.
#[tokio::test]
async fn a_request_that_does_not_come_back_is_dropped_counted_and_forgotten_by_the_file() {
    let rig = two_waiting("restart_orphans").await;
    let back = rig.restart(true).await;
    back.advance(599);
    back.shared.tick().await;
    assert_eq!(back.registry.queue_position(&id("r1")), Some(0));
    assert_eq!(back.registry.dropped_reservations(), 0);
    back.advance(1);
    back.shared.tick().await;
    assert!(back.registry.reservations().is_empty());
    assert_eq!(back.registry.queue_position(&id("r1")), None);
    assert_eq!(back.registry.queue_position(&id("r2")), None);
    assert_eq!(back.registry.dropped_reservations(), 2);
    assert_eq!(back.registry.request_record(&id("r1")).unwrap().give_ups, 1);
    back.shared.tick().await;

    let again = back.restart(true).await;
    assert!(again.registry.reservations().is_empty());
    assert_eq!(again.registry.queue_position(&id("r1")), None);
    assert_eq!(again.registry.queue_position(&id("r2")), None);
    assert_eq!(again.registry.dropped_reservations(), 0);
}

/// A lease file as every build wrote it before the reservation ledger: no
/// `reservation_count`, no rows.
const OLD_FILE: &str = "version = 1\nterm = 3\nnext_sequence = 1\nlease_count = 0\n";

/// A lease file with reservation rows as the ledger writes them, the second a row that
/// only holds a place.
const NEW_FILE: &str = "version = 1\nterm = 3\nnext_sequence = 1\nlease_count = 0\n\
    reservation_count = 2\n\n\
    [[reservation]]\nrequest_id = \"r9\"\nhost = \"a\"\nreserved_at_ms = 1000000\n\
    deadline_ms = 1030000\nqueue_place = 0\nqueued_since_ms = 1000000\ngive_ups = 1\n\n\
    [[reservation]]\nrequest_id = \"r10\"\nqueue_place = 1\nqueued_since_ms = 1000000\n";

/// Asserts that a lease file written before the ledger existed resumes: the server
/// starts, nothing is reserved or waiting and nothing is counted dropped. It is the
/// fixture of an upgrade: the new build reads the old file whole.
#[tokio::test]
async fn a_lease_file_from_before_the_ledger_resumes_with_no_reservations() {
    let rig = Rig::over_file("restart_old_file", OLD_FILE).await;
    assert!(rig.registry.reservations().is_empty());
    assert_eq!(rig.registry.whole_counts(), WholeCounts::default());
    assert_eq!(rig.registry.dropped_reservations(), 0);
}

/// Asserts that a lease file with ledger rows resumes with them: `r9` holds the host
/// with its deadline raised to the boot plus the boot grace, `r10` waits behind it, and
/// the give-up count of `r9` is one. It is the other half of the upgrade fixture, and it
/// catches a row that is read and not acted on.
#[tokio::test]
async fn a_lease_file_with_ledger_rows_resumes_with_them() {
    let rig = Rig::over_file("restart_new_file", NEW_FILE).await;
    let marks = rig.registry.reservations();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].request, id("r9"));
    assert_eq!(marks[0].host.as_str(), "a");
    assert_eq!(marks[0].at, at(T0));
    assert_eq!(marks[0].deadline, at(T0 + GRACE_MS));
    assert_eq!(rig.registry.queue_position(&id("r9")), Some(0));
    assert_eq!(rig.registry.queue_position(&id("r10")), Some(1));
    assert_eq!(rig.registry.request_record(&id("r9")).unwrap().give_ups, 1);
}
