//! The bound of a job's events, the time its code last changed, and its attempts.

use std::time::Duration;

use js_state::StoreTime;
use js_types::{HostId, JobEventKind, QueueCode, QueueNote};

use super::{JobEvent, MAX_JOB_EVENTS, QueueTrail};

fn at(millis: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(millis))
}

fn note(code: QueueCode, detail: &str, event: Option<JobEventKind>) -> QueueNote {
    QueueNote {
        code,
        detail: detail.to_owned(),
        event,
        host: None,
        attempt: false,
    }
}

fn refused(n: usize) -> QueueNote {
    QueueNote {
        attempt: true,
        ..note(
            QueueCode::FailingOver,
            &format!("refusal {n}"),
            Some(JobEventKind::FailingOver),
        )
    }
}

/// A trail nothing was recorded on shows nothing: no code, detail, time, attempt or event.
#[test]
fn a_new_trail_shows_nothing() {
    let trail = QueueTrail::default();
    assert_eq!(trail.code(), QueueCode::Unspecified);
    assert_eq!(trail.detail(), "");
    assert_eq!(trail.since(), None);
    assert_eq!(trail.attempts(), 0);
    assert!(trail.events().is_empty());
    assert_eq!(trail.dropped_events(), 0);
}

/// Appending past the bound keeps the newest events in order and counts each one dropped.
#[test]
fn appending_past_the_bound_keeps_the_newest_in_order_and_counts_the_dropped() {
    let mut trail = QueueTrail::default();
    let total = MAX_JOB_EVENTS + 5;
    for n in 0..total {
        assert!(trail.apply(&refused(n), at(n as u64)));
    }
    let kept: Vec<String> = trail.events().iter().map(|e| e.detail.clone()).collect();
    let newest: Vec<String> = (5..total).map(|n| format!("refusal {n}")).collect();
    assert_eq!(kept, newest);
    assert_eq!(trail.events()[0].at, at(5));
    assert_eq!(trail.dropped_events(), 5);
    assert_eq!(trail.attempts(), total as u32);
}

/// Up to the bound nothing is dropped.
#[test]
fn appending_up_to_the_bound_drops_nothing() {
    let mut trail = QueueTrail::default();
    for n in 0..MAX_JOB_EVENTS {
        trail.apply(&refused(n), at(n as u64));
    }
    assert_eq!(trail.events().len(), MAX_JOB_EVENTS);
    assert_eq!(trail.dropped_events(), 0);
}

/// A change of detail alone leaves the time the code changed as it was, appends no event, and
/// is still a change to write.
#[test]
fn a_detail_only_change_leaves_since_unchanged() {
    let mut trail = QueueTrail::default();
    let first = note(
        QueueCode::WaitingForWholeHost,
        "host a is being emptied for the job",
        Some(JobEventKind::WaitingForWholeHost),
    );
    assert!(trail.apply(&first, at(100)));
    let moved = note(
        QueueCode::WaitingForWholeHost,
        "host b is being emptied for the job",
        None,
    );
    assert!(trail.apply(&moved, at(900)));
    assert_eq!(trail.since(), Some(at(100)));
    assert_eq!(trail.detail(), "host b is being emptied for the job");
    assert_eq!(trail.events().len(), 1);
}

/// A note that changes nothing (same code, same detail, no event, no attempt) says so, so the
/// caller writes nothing.
#[test]
fn the_same_note_again_changes_nothing() {
    let mut trail = QueueTrail::default();
    let waiting = note(
        QueueCode::WaitingForCapacity,
        "no open host can run the job",
        Some(JobEventKind::WaitingForCapacity),
    );
    assert!(trail.apply(&waiting, at(1)));
    let again = QueueNote {
        event: None,
        ..waiting
    };
    let before = trail.clone();
    assert!(!trail.apply(&again, at(2)));
    assert_eq!(trail, before);
}

/// A change of code moves `since` to the time of the note, and an event keeps its host.
#[test]
fn a_code_change_moves_since_and_an_event_keeps_its_host() {
    let mut trail = QueueTrail::default();
    trail.apply(&refused(0), at(10));
    let host = HostId::new("h-1").unwrap();
    let placed = QueueNote {
        code: QueueCode::Placed,
        detail: "placed on host h-1".to_owned(),
        event: Some(JobEventKind::Placed),
        host: Some(host.clone()),
        attempt: true,
    };
    assert!(trail.apply(&placed, at(20)));
    assert_eq!(trail.code(), QueueCode::Placed);
    assert_eq!(trail.since(), Some(at(20)));
    assert_eq!(trail.attempts(), 2);
    assert_eq!(
        trail.events().last(),
        Some(&JobEvent {
            at: at(20),
            kind: JobEventKind::Placed,
            detail: "placed on host h-1".to_owned(),
            host: Some(host),
        })
    );
}

/// The counts saturate rather than wrap.
#[test]
fn attempts_and_dropped_events_saturate() {
    let mut trail = QueueTrail::of_row(
        (QueueCode::FailingOver, String::new(), Some(at(0))),
        u32::MAX,
        Vec::new(),
        u32::MAX,
    );
    for n in 0..=MAX_JOB_EVENTS {
        trail.apply(&refused(n), at(1));
    }
    assert_eq!(trail.attempts(), u32::MAX);
    assert_eq!(trail.dropped_events(), u32::MAX);
}

/// A trail read with more events than the bound (a later build's) is trimmed to the newest at
/// the next append, each one dropped counted.
#[test]
fn a_trail_over_the_bound_is_trimmed_at_the_next_append() {
    let events: Vec<JobEvent> = (0..MAX_JOB_EVENTS + 3)
        .map(|n| JobEvent {
            at: at(n as u64),
            kind: JobEventKind::FailingOver,
            detail: format!("old {n}"),
            host: None,
        })
        .collect();
    let mut trail = QueueTrail::of_row(
        (QueueCode::FailingOver, String::new(), Some(at(0))),
        0,
        events,
        1,
    );
    trail.apply(&refused(99), at(500));
    assert_eq!(trail.events().len(), MAX_JOB_EVENTS);
    assert_eq!(trail.dropped_events(), 5);
    assert_eq!(trail.events()[0].detail, "old 4");
    assert_eq!(
        trail.events().last().map(|e| e.detail.as_str()),
        Some("refusal 99")
    );
}

/// A trail tells whether notes would change it without changing it: a repeat of its own
/// status would not, a new detail, an event or an attempt would.
#[test]
fn changed_by_tells_whether_notes_would_change_the_trail() {
    let mut trail = QueueTrail::default();
    let first = note(
        QueueCode::WaitingForCapacity,
        "room",
        Some(JobEventKind::WaitingForCapacity),
    );
    assert!(trail.changed_by(std::slice::from_ref(&first)));
    trail.apply(&first, at(7));
    let before = trail.clone();
    let same = note(QueueCode::WaitingForCapacity, "room", None);
    assert!(!trail.changed_by(std::slice::from_ref(&same)));
    assert!(!trail.changed_by(&[]));
    assert!(trail.changed_by(&[same, note(QueueCode::WaitingForCapacity, "other", None)]));
    assert!(trail.changed_by(&[refused(1)]));
    assert_eq!(trail, before, "nothing was applied");
}

/// Applying several notes says whether the trail ended other than it was: a note undone by the
/// next one is no change, and nothing to apply is none.
#[test]
fn apply_all_compares_the_trail_before_and_after() {
    let mut trail = QueueTrail::default();
    let first = note(
        QueueCode::WaitingForCapacity,
        "room",
        Some(JobEventKind::WaitingForCapacity),
    );
    assert!(trail.apply_all(std::slice::from_ref(&first), at(1)));
    let other = note(QueueCode::WaitingForCapacity, "other", None);
    let back = note(QueueCode::WaitingForCapacity, "room", None);
    assert!(!trail.apply_all(&[other, back], at(2)));
    assert!(!trail.apply_all(&[], at(3)));
}
