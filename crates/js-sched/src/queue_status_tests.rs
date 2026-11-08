//! Each outcome of a pass, from each code a job can show, and the note it records.

use std::time::Duration;

use js_elastic::{Instant, RefusalCode};
use js_types::{HostId, JobEventKind, ProviderId, QueueCode, QueueNote, RequestId};

use super::{Outcome, note, notes};
use crate::failover::{PassedOver, Refused};

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

fn refused(code: RefusalCode, zone: Option<&str>) -> Refused {
    let provider = ProviderId::new("cloud").unwrap();
    let passed_over = (code == RefusalCode::Quota).then(|| {
        let key = PassedOver {
            provider: provider.clone(),
            instance_type: "big".to_owned(),
            region: Some("r".to_owned()),
        };
        (
            key,
            Instant::from_millis(5_000).saturating_add(Duration::from_secs(1)),
        )
    });
    Refused {
        provider,
        offer: "big".to_owned(),
        zone: zone.map(str::to_owned),
        code,
        passed_over,
        request: RequestId::new("job-1").ok(),
    }
}

fn waiting(detail: &str, event: Option<JobEventKind>) -> QueueNote {
    QueueNote {
        code: QueueCode::WaitingForCapacity,
        detail: detail.to_owned(),
        event,
        host: None,
        attempt: false,
    }
}

/// A job placed nowhere waits for room, with an event only when it did not wait for room
/// already, and a detail that says whether any open host could run it.
#[test]
fn a_job_placed_nowhere_waits_for_capacity_with_an_event_on_the_change_only() {
    let room = "no open host that can run the job has room for it now";
    let first = note(&Outcome::Waiting { capable: true }, QueueCode::Unspecified);
    assert_eq!(
        first,
        Some(waiting(room, Some(JobEventKind::WaitingForCapacity)))
    );
    let again = note(
        &Outcome::Waiting { capable: true },
        QueueCode::WaitingForCapacity,
    );
    assert_eq!(again, Some(waiting(room, None)));
    let none = note(
        &Outcome::Waiting { capable: false },
        QueueCode::WaitingForCapacity,
    );
    assert_eq!(none, Some(waiting("no open host can run the job", None)));
}

/// A reason pass 2 gave outlives pass 1's "nothing is free": a job backing off on quota or
/// failing over records nothing when it is placed nowhere again, or when no whole host is free
/// and none is being emptied for it; a host being emptied for it is recorded.
#[test]
fn nothing_free_records_nothing_on_a_job_that_pass_2_explains() {
    let explained = [
        QueueCode::QuotaBackoff,
        QueueCode::FailingOver,
        QueueCode::WaitingForMachine,
    ];
    for current in explained {
        for capable in [true, false] {
            assert_eq!(note(&Outcome::Waiting { capable }, current), None);
        }
        assert_eq!(note(&Outcome::WholeHost { emptying: None }, current), None);
        let emptying = Outcome::WholeHost {
            emptying: Some(host("h-9")),
        };
        assert_eq!(
            note(&emptying, current).map(|note| (note.code, note.event)),
            Some((
                QueueCode::WaitingForWholeHost,
                Some(JobEventKind::WaitingForWholeHost)
            ))
        );
    }
}

/// A sequence of outcomes in one pass is decided in order, each against the code the ones
/// before it left: a refusal then pass 1's "no room" keeps failing over, a second refusal is
/// a second attempt, and a placement ends it.
#[test]
fn a_sequence_of_outcomes_is_decided_in_order() {
    let outcomes = [
        Outcome::Waiting { capable: false },
        Outcome::Refused(refused(RefusalCode::Capacity, None)),
        Outcome::Waiting { capable: true },
        Outcome::Refused(refused(RefusalCode::Capacity, Some("r-b"))),
        Outcome::Placed { host: host("h-4") },
    ];
    let noted = notes(QueueCode::Unspecified, &outcomes);
    let seen: Vec<(QueueCode, Option<JobEventKind>, bool)> = noted
        .iter()
        .map(|note| (note.code, note.event, note.attempt))
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                QueueCode::WaitingForCapacity,
                Some(JobEventKind::WaitingForCapacity),
                false
            ),
            (
                QueueCode::FailingOver,
                Some(JobEventKind::FailingOver),
                true
            ),
            (
                QueueCode::FailingOver,
                Some(JobEventKind::FailingOver),
                true
            ),
            (QueueCode::Placed, Some(JobEventKind::Placed), true),
        ]
    );
    assert!(
        notes(
            QueueCode::FailingOver,
            &[Outcome::Waiting { capable: true }]
        )
        .is_empty()
    );
}

/// A whole-host job that was not committed waits for a whole host, naming the host being
/// emptied for it when it has one; the event comes with the change of code only.
#[test]
fn a_whole_host_job_waits_for_a_whole_host_naming_its_mark() {
    let marked = note(
        &Outcome::WholeHost {
            emptying: Some(host("h-1")),
        },
        QueueCode::Unspecified,
    );
    assert_eq!(
        marked,
        Some(QueueNote {
            code: QueueCode::WaitingForWholeHost,
            detail: "host h-1 is being emptied for the job".to_owned(),
            event: Some(JobEventKind::WaitingForWholeHost),
            host: None,
            attempt: false,
        })
    );
    let unmarked = note(
        &Outcome::WholeHost { emptying: None },
        QueueCode::WaitingForWholeHost,
    );
    assert_eq!(
        unmarked,
        Some(QueueNote {
            code: QueueCode::WaitingForWholeHost,
            detail: "no whole host is free".to_owned(),
            event: None,
            host: None,
            attempt: false,
        })
    );
}

/// A capacity refusal fails over, a quota refusal backs off: each is an attempt with its own
/// event, also when the job already showed that code.
#[test]
fn a_refused_acquire_is_an_attempt_with_an_event_every_time() {
    let capacity = note(
        &Outcome::Refused(refused(RefusalCode::Capacity, Some("r-a"))),
        QueueCode::FailingOver,
    );
    assert_eq!(
        capacity,
        Some(QueueNote {
            code: QueueCode::FailingOver,
            detail: "cloud refused offer big in zone r-a for capacity; the next zone, offer or provider is asked".to_owned(),
            event: Some(JobEventKind::FailingOver),
            host: None,
            attempt: true,
        })
    );
    let quota = note(
        &Outcome::Refused(refused(RefusalCode::Quota, None)),
        QueueCode::WaitingForCapacity,
    );
    assert_eq!(
        quota,
        Some(QueueNote {
            code: QueueCode::QuotaBackoff,
            detail: "cloud refused offer big for quota; it is passed over until 6000 ms".to_owned(),
            event: Some(JobEventKind::QuotaBackoff),
            host: None,
            attempt: true,
        })
    );
}

/// A quota refusal recorded without what it passes over says no end.
#[test]
fn a_quota_refusal_without_a_pass_over_names_no_end() {
    let refused = Refused {
        passed_over: None,
        ..refused(RefusalCode::Quota, Some("z"))
    };
    let quota = note(&Outcome::Refused(refused), QueueCode::Unspecified);
    assert_eq!(
        quota.map(|note| note.detail),
        Some("cloud refused offer big in zone z for quota".to_owned())
    );
}

/// A refusal for shape or a denied acquire is no state of the world: it records nothing.
#[test]
fn a_shape_or_denied_refusal_records_nothing() {
    for code in [RefusalCode::Shape, RefusalCode::Denied] {
        assert_eq!(
            note(
                &Outcome::Refused(refused(code, None)),
                QueueCode::Unspecified
            ),
            None
        );
    }
}

/// A machine launched for the job takes it out of failing over: it waits for the machine,
/// named, with an event for the change; and pass 1's "no room" does not take its place.
#[test]
fn a_launch_ends_failing_over_and_waits_for_the_machine() {
    assert_eq!(
        note(
            &Outcome::Waiting { capable: true },
            QueueCode::WaitingForMachine
        ),
        None
    );
    let launched = note(
        &Outcome::Launched {
            machine: "m-7".to_owned(),
        },
        QueueCode::FailingOver,
    );
    assert_eq!(
        launched,
        Some(QueueNote {
            code: QueueCode::WaitingForMachine,
            detail: "machine m-7 was launched for the job; it waits for the machine to join"
                .to_owned(),
            event: Some(JobEventKind::WaitingForMachine),
            host: None,
            attempt: false,
        })
    );
}

/// A placement is an attempt with a placed event naming its host, from any code.
#[test]
fn a_placement_is_an_attempt_with_its_host() {
    for current in [
        QueueCode::Unspecified,
        QueueCode::Placed,
        QueueCode::QuotaBackoff,
    ] {
        let placed = note(&Outcome::Placed { host: host("h-2") }, current);
        assert_eq!(
            placed,
            Some(QueueNote {
                code: QueueCode::Placed,
                detail: "placed on host h-2".to_owned(),
                event: Some(JobEventKind::Placed),
                host: Some(host("h-2")),
                attempt: true,
            })
        );
    }
}

/// A failed whole-host prepare sends the job back to wait for a whole host, with a requeue
/// event naming the host; it is no attempt of its own.
#[test]
fn a_requeue_waits_for_a_whole_host_with_its_host() {
    let requeued = note(&Outcome::Requeued { host: host("h-3") }, QueueCode::Placed);
    assert_eq!(
        requeued,
        Some(QueueNote {
            code: QueueCode::WaitingForWholeHost,
            detail: "the whole-host prepare on host h-3 failed; the job is queued again".to_owned(),
            event: Some(JobEventKind::Requeued),
            host: Some(host("h-3")),
            attempt: false,
        })
    );
}
