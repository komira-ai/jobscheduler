//! What a job's record keeps of why it waits (`docs/design/queue-status.md`): its queue code,
//! with the detail for a person and when the code last changed, how many placement attempts it
//! had, and its newest events, at most [`MAX_JOB_EVENTS`] of them with a count of those
//! dropped ([`QueueTrail`]).
//!
//! The engine decides each [`QueueNote`] (`js_sched::queue_status`); the store applies it
//! ([`QueueTrail::apply`]) at its own clock's time, in the transaction that writes the job.

use js_state::StoreTime;
use js_types::{HostId, JobEventKind, QueueCode, QueueNote};

/// The most events a job's record keeps: appending past it drops the oldest.
pub const MAX_JOB_EVENTS: usize = 32;

/// One event of a job's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    /// When the store recorded it.
    pub at: StoreTime,
    /// What happened.
    pub kind: JobEventKind,
    /// Text for a person saying more.
    pub detail: String,
    /// The host it names: the one the job was placed on, or sent back from.
    pub host: Option<HostId>,
}

/// Why a job waits, and its history: see the module documentation. The default is a job that
/// nothing has been recorded of, which is also what a row written before these fields reads as.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueTrail {
    code: QueueCode,
    detail: String,
    since: Option<StoreTime>,
    attempts: u32,
    events: Vec<JobEvent>,
    dropped_events: u32,
}

impl QueueTrail {
    /// The code the job shows: [`QueueCode::Unspecified`] while nothing was recorded.
    #[must_use]
    pub fn code(&self) -> QueueCode {
        self.code
    }

    /// The detail of the code, for a person; empty while nothing was recorded.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// When the code last changed; `None` while nothing was recorded. A change of the detail
    /// alone does not move it.
    #[must_use]
    pub fn since(&self) -> Option<StoreTime> {
        self.since
    }

    /// How many placement attempts the job had: placements and refused acquires.
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The newest events, oldest first.
    #[must_use]
    pub fn events(&self) -> &[JobEvent] {
        &self.events
    }

    /// How many older events were dropped to keep at most [`MAX_JOB_EVENTS`].
    #[must_use]
    pub fn dropped_events(&self) -> u32 {
        self.dropped_events
    }

    /// Applies `note` at `at`: the code and detail become the note's, and `since` becomes `at`
    /// only when the code changed; an attempt is counted (saturating); the note's event is
    /// appended, the oldest dropped and counted (saturating) while more than
    /// [`MAX_JOB_EVENTS`] are kept. Whether anything changed, so a caller writes nothing when
    /// it did not.
    pub fn apply(&mut self, note: &QueueNote, at: StoreTime) -> bool {
        let before = self.clone();
        if note.code != self.code {
            self.code = note.code;
            self.since = Some(at);
        }
        self.detail.clone_from(&note.detail);
        if note.attempt {
            self.attempts = self.attempts.saturating_add(1);
        }
        if let Some(kind) = note.event {
            self.events.push(JobEvent {
                at,
                kind,
                detail: note.detail.clone(),
                host: note.host.clone(),
            });
            let over = self.events.len().saturating_sub(MAX_JOB_EVENTS);
            self.events.drain(..over);
            let over = u32::try_from(over).unwrap_or(u32::MAX);
            self.dropped_events = self.dropped_events.saturating_add(over);
        }
        *self != before
    }

    /// Applies `notes` in order at `at` ([`QueueTrail::apply`]), and whether the trail is now
    /// other than it was.
    pub fn apply_all(&mut self, notes: &[QueueNote], at: StoreTime) -> bool {
        let before = self.clone();
        for note in notes {
            self.apply(note, at);
        }
        *self != before
    }

    /// Whether applying `notes` in order would change this trail: what a pass checks against
    /// the job it read, so a job whose status stands is not read or written again.
    #[must_use]
    pub fn changed_by(&self, notes: &[QueueNote]) -> bool {
        let at = self.since.unwrap_or_default();
        self.clone().apply_all(notes, at)
    }

    /// A trail as a row holds it.
    pub(crate) fn of_row(
        (code, detail, since): (QueueCode, String, Option<StoreTime>),
        attempts: u32,
        events: Vec<JobEvent>,
        dropped_events: u32,
    ) -> Self {
        Self {
            code,
            detail,
            since,
            attempts,
            events,
            dropped_events,
        }
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
