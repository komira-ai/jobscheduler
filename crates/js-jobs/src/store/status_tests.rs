//! The queue status a store write applies: [`JobStore::note_queue`] on a queued job, and the
//! notes a placement and a requeue carry into their own transactions.

use std::collections::BTreeMap;
use std::time::Duration;

use js_state::Store as _;
use js_state::fake::{FakeBackend, Flaw};
use js_types::{JobEventKind, QueueCode, QueueNote};

use crate::suite::{host, job_id, rig, share, whole_host_spec};
use crate::{Booked, FailedPrepare, JobEvent, JobState, JobStoreError, Placement, StartFailure};

fn fake() -> FakeBackend {
    FakeBackend::new(Flaw::None)
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

fn placed_note(on: &str) -> QueueNote {
    QueueNote {
        code: QueueCode::Placed,
        detail: format!("placed on host {on}"),
        event: Some(JobEventKind::Placed),
        host: Some(host(on)),
        attempt: true,
    }
}

/// The notes are applied in order at the store's time, in one write: the code's `since` is
/// the store's clock, the attempt is counted, and each event is kept with that time.
#[test]
fn note_queue_applies_the_notes_in_order_at_the_stores_time() {
    let rig = rig(&fake());
    let id = rig.queue("noted");
    rig.clock.advance(Duration::from_secs(42));
    let now = rig.store.now();
    let refused = QueueNote {
        code: QueueCode::FailingOver,
        detail: "refused".to_owned(),
        event: Some(JobEventKind::FailingOver),
        host: None,
        attempt: true,
    };
    let notes = [
        waiting("room", Some(JobEventKind::WaitingForCapacity)),
        refused,
    ];
    let commits = rig.store.commit_stats().txns;
    let written = rig.jobs.note_queue(&id, &notes).expect("writes");
    assert_eq!(
        rig.store.commit_stats().txns,
        commits + 1,
        "one transaction"
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("kept");
    assert_eq!(written, Some(job.clone()));
    assert_eq!(job.queue.code(), QueueCode::FailingOver);
    assert_eq!(job.queue.since(), Some(now));
    assert_eq!(job.queue.attempts(), 1);
    let kinds: Vec<(JobEventKind, _)> = job.queue.events().iter().map(|e| (e.kind, e.at)).collect();
    assert_eq!(
        kinds,
        vec![
            (JobEventKind::WaitingForCapacity, now),
            (JobEventKind::FailingOver, now)
        ]
    );
}

/// Notes that change nothing write nothing, so a job that waits for a long time is written
/// once when it starts waiting, not on every pass.
#[test]
fn notes_that_change_nothing_write_nothing() {
    let rig = rig(&fake());
    let id = rig.queue("steady");
    rig.jobs
        .note_queue(
            &id,
            &[waiting("room", Some(JobEventKind::WaitingForCapacity))],
        )
        .expect("writes");
    let commits = rig.store.commit_stats().txns;
    rig.tick();
    assert_eq!(rig.jobs.note_queue(&id, &[waiting("room", None)]), Ok(None));
    assert_eq!(rig.jobs.note_queue(&id, &[]), Ok(None));
    assert_eq!(
        rig.store.commit_stats().txns,
        commits,
        "nothing was written"
    );
}

/// A job that is not queued (placed, or ended) takes no note: the write is skipped.
#[test]
fn a_job_that_is_not_queued_takes_no_note() {
    let rig = rig(&fake());
    let id = rig.placed("gone-on");
    let before = rig.jobs.get_job(&id).expect("reads");
    let note = waiting("room", Some(JobEventKind::WaitingForCapacity));
    assert_eq!(rig.jobs.note_queue(&id, &[note]), Ok(None));
    assert_eq!(rig.jobs.get_job(&id).expect("reads"), before);
}

/// A job that does not exist is not found; a row changed between the read and the commit
/// makes the write stale, and the other writer's change stands.
#[test]
fn note_queue_is_not_found_or_stale_as_any_write() {
    let rig = rig(&fake());
    let missing = job_id("missing");
    let note = waiting("room", Some(JobEventKind::WaitingForCapacity));
    assert_eq!(
        rig.jobs.note_queue(&missing, std::slice::from_ref(&note)),
        Err(JobStoreError::NotFound {
            id: missing.clone()
        })
    );
    let id = rig.queue("raced");
    let other = rig.other();
    let target = id.clone();
    rig.store.before_next(move || {
        other
            .request_cancel(&target)
            .expect("the other writer cancels");
    });
    assert_eq!(
        rig.jobs.note_queue(&id, &[note]),
        Err(JobStoreError::Stale { id: id.clone() })
    );
    let job = rig.jobs.get_job(&id).expect("reads").expect("kept");
    assert_eq!(job.queue.code(), QueueCode::Unspecified);
}

/// A placement applies its notes in its own transaction: the placed job shows `PLACED` from
/// the placement's time, with the attempt and the placed event naming the host.
#[test]
fn a_placement_applies_its_notes_in_its_own_transaction() {
    let rig = rig(&fake());
    let id = rig.queue("placed");
    rig.jobs
        .note_queue(
            &id,
            &[waiting("room", Some(JobEventKind::WaitingForCapacity))],
        )
        .expect("writes");
    rig.tick();
    let placement = Placement {
        queue: vec![placed_note("host-a")],
        ..Placement::default()
    };
    let (placed, _) = rig
        .jobs
        .place_with(&id, host("host-a"), share(), placement)
        .expect("places");
    let at = placed.placed_at.expect("placed");
    assert_eq!(placed.queue.code(), QueueCode::Placed);
    assert_eq!(placed.queue.since(), Some(at));
    assert_eq!(placed.queue.attempts(), 1);
    assert_eq!(
        placed.queue.events().last(),
        Some(&JobEvent {
            at,
            kind: JobEventKind::Placed,
            detail: "placed on host host-a".to_owned(),
            host: Some(host("host-a")),
        })
    );
    assert_eq!(rig.jobs.get_job(&id).expect("reads"), Some(placed));
}

/// A requeued whole-host job takes its note in the requeue's transaction; a terminal failed
/// prepare takes none, and its placed status and events stay.
#[test]
fn a_requeue_applies_its_note_and_a_terminal_failure_keeps_the_trail() {
    let rig = rig(&fake());
    let requeued = QueueNote {
        code: QueueCode::WaitingForWholeHost,
        detail: "queued again".to_owned(),
        event: Some(JobEventKind::Requeued),
        host: Some(host("host-a")),
        attempt: false,
    };
    let failed = |terminal: bool| FailedPrepare {
        failed_prepares: 1,
        prepare_bars: BTreeMap::new(),
        terminal,
        queue: Some(requeued.clone()),
    };
    let mut outcomes = Vec::new();
    for (text, terminal) in [("again", false), ("over", true)] {
        let id = rig.queue_spec(text, whole_host_spec());
        let placement = Placement {
            prepare_timeout: Some(Duration::from_secs(60)),
            queue: vec![placed_note("host-a")],
            ..Placement::default()
        };
        rig.jobs
            .place_with(&id, host("host-a"), Booked::WholeHost, placement)
            .expect("places");
        rig.tick();
        let (after, _) = rig
            .jobs
            .fail_prepare_with(&id, failed(terminal), &|_| Vec::new())
            .expect("fails the prepare");
        outcomes.push(after);
    }
    let again = &outcomes[0];
    assert_eq!(again.state, JobState::Queued);
    assert_eq!(again.queue.code(), QueueCode::WaitingForWholeHost);
    let kinds: Vec<JobEventKind> = again.queue.events().iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![JobEventKind::Placed, JobEventKind::Requeued]);
    let over = &outcomes[1];
    assert_eq!(
        over.state,
        JobState::FailedToStart(StartFailure::ExclusiveNotReady)
    );
    assert_eq!(over.queue.code(), QueueCode::Placed);
    assert_eq!(over.queue.events().len(), 1);
}
