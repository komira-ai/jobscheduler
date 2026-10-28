//! The scheduler's read-only accessors on one operation and one worker, and a `Started`
//! report that does not match the lease the operation holds.
//!
//! A module of the `scheduler` test binary, for its harness.

use js_sched::{Event, OpState};
use js_types::sched::{LeaseId, NodePhase, OperationId, Qos};

use super::{GIB, Harness, request, w};

/// Catches: an accessor that answers for an operation or worker the scheduler does not
/// hold, or that gives another operation's request, QoS or size key; a term that is not
/// the leader's.
#[test]
fn the_accessors_answer_for_the_operation_or_worker_they_are_asked_about() {
    let mut h = Harness::new();
    h.worker("a", 8_000, 8 * GIB);
    h.submit(1, request(1));
    h.submit(2, request(2));
    let first = h.s.twin(&request(1)).unwrap();
    let second = h.s.twin(&request(2)).unwrap();
    assert_ne!(first, second);

    assert_eq!(h.s.term(), 1);
    assert_eq!(h.s.request(first), Some(&request(1)));
    assert_eq!(h.s.request(second), Some(&request(2)));
    assert_eq!(h.s.request(OperationId(99)), None);
    assert_eq!(h.s.qos(first), Some(&Qos::Ci));
    assert_eq!(h.s.qos(OperationId(99)), None);

    assert_eq!(h.s.size_key(first), None);
    h.s.set_size_key(first, 42);
    assert_eq!(h.s.size_key(first), Some(42));
    assert_eq!(h.s.size_key(second), None);

    assert_eq!(h.s.phase(&w("a")), Some(&NodePhase::Serving));
    assert_eq!(h.s.phase(&w("b")), None);
}

/// Catches: a `Started` report for a lease the operation no longer holds (a report that
/// overtook a re-grant) that moves the operation to running, and so ends the start grace
/// of the lease it does hold.
#[test]
fn a_started_report_for_another_lease_leaves_the_operation_leased() {
    let mut h = Harness::new();
    h.worker("a", 8_000, 8 * GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit(js_types::sched::ControlRecord::Lease(grant.clone()));

    h.feed(Event::Started {
        operation: grant.operation,
        lease: LeaseId::new(grant.lease.term, grant.lease.seq + 1),
    });
    assert!(matches!(
        h.s.state(grant.operation),
        Some(OpState::Leased {
            committed: true,
            ..
        })
    ));

    h.feed(Event::Started {
        operation: grant.operation,
        lease: grant.lease,
    });
    assert!(h.running(&grant));
}
