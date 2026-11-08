//! What a pass records of why a queued job waits (`docs/design/queue-status.md`, section 2):
//! [`note`] turns the [`Outcome`] a pass reached for one job, and the code the job shows now,
//! into the [`QueueNote`] the job store applies to the job's record, or nothing.
//!
//! Pass 1 (the placement walk over the open hosts) reaches [`Outcome::Waiting`],
//! [`Outcome::WholeHost`] and [`Outcome::Placed`]; pass 2 (the acquire of a machine, with its
//! failover) reaches [`Outcome::Refused`] and [`Outcome::Launched`]; a failed whole-host prepare
//! reaches [`Outcome::Requeued`]. A pass-2 reason outlives pass 1's "nothing is free": a job
//! that shows [`QueueCode::QuotaBackoff`], [`QueueCode::FailingOver`] or
//! [`QueueCode::WaitingForMachine`] keeps it through
//! [`Outcome::Waiting`], and through an [`Outcome::WholeHost`] with no host being emptied for
//! it, until it is placed, a host is emptied or a machine is launched for it, or another
//! refusal comes. An event is appended on every change of code, and on every attempt and requeue even
//! when the code stays; a change of the detail alone appends none.
//!
//! Nothing here reads a clock: the store stamps a note with its own when it applies it.

use js_elastic::RefusalCode;
use js_types::{HostId, JobEventKind, QueueCode, QueueNote};

use crate::failover::Refused;

/// The outcome a pass reached for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Pass 1 placed the job nowhere. `capable`: some open host could run it, but none has
    /// room now; otherwise no open host can run it at all.
    Waiting {
        /// Whether some open host could run the job.
        capable: bool,
    },
    /// Pass 1 did not commit the job, which books a whole host. `emptying`: the host being
    /// emptied for it (its mark), when there is one.
    WholeHost {
        /// The host the job's mark holds.
        emptying: Option<HostId>,
    },
    /// Pass 2: an acquire made for the job's request was refused, as
    /// [`crate::failover::AcquireMemory::answered`] recorded it.
    Refused(Refused),
    /// Pass 2 launched `machine` for the job's request.
    Launched {
        /// The machine's name.
        machine: String,
    },
    /// The job was placed on `host`.
    Placed {
        /// The host.
        host: HostId,
    },
    /// A failed whole-host prepare on `host` sent the placed job back to the queue.
    Requeued {
        /// The host whose prepare failed.
        host: HostId,
    },
}

/// What `outcome` records on a job that shows `current` now; `None` when it records nothing:
/// a job pass 2 explains is told nothing is free (no room, or no whole host and none being
/// emptied for it), or a refusal other than for capacity or quota (which
/// [`crate::failover::AcquireMemory::answered`] never records).
#[must_use]
pub fn note(outcome: &Outcome, current: QueueCode) -> Option<QueueNote> {
    use JobEventKind as Kind;
    let waiting = |detail: &str| {
        (
            QueueCode::WaitingForCapacity,
            Kind::WaitingForCapacity,
            detail.to_owned(),
        )
    };
    let (code, kind, detail) = match outcome {
        Outcome::Waiting { .. } | Outcome::WholeHost { emptying: None }
            if matches!(
                current,
                QueueCode::QuotaBackoff | QueueCode::FailingOver | QueueCode::WaitingForMachine
            ) =>
        {
            return None;
        }
        Outcome::Waiting { capable: true } => {
            waiting("no open host that can run the job has room for it now")
        }
        Outcome::Waiting { capable: false } => waiting("no open host can run the job"),
        Outcome::Launched { machine } => (
            QueueCode::WaitingForMachine,
            Kind::WaitingForMachine,
            format!("machine {machine} was launched for the job; it waits for the machine to join"),
        ),
        Outcome::WholeHost { emptying } => (
            QueueCode::WaitingForWholeHost,
            Kind::WaitingForWholeHost,
            emptying.as_ref().map_or_else(
                || "no whole host is free".to_owned(),
                |host| format!("host {host} is being emptied for the job"),
            ),
        ),
        Outcome::Refused(refused) => return refusal(refused),
        Outcome::Placed { host } => {
            return Some(QueueNote {
                code: QueueCode::Placed,
                detail: format!("placed on host {host}"),
                event: Some(Kind::Placed),
                host: Some(host.clone()),
                attempt: true,
            });
        }
        Outcome::Requeued { host } => {
            return Some(QueueNote {
                code: QueueCode::WaitingForWholeHost,
                detail: format!(
                    "the whole-host prepare on host {host} failed; the job is queued again"
                ),
                event: Some(Kind::Requeued),
                host: Some(host.clone()),
                attempt: false,
            });
        }
    };
    Some(QueueNote {
        code,
        detail,
        event: (code != current).then_some(kind),
        host: None,
        attempt: false,
    })
}

/// The notes `outcomes`, in order, record on a job that shows `current` now: each outcome is
/// decided against the code the notes before it left ([`note`]), so a sequence of outcomes
/// in one pass reads as they came.
#[must_use]
pub fn notes<'a>(
    current: QueueCode,
    outcomes: impl IntoIterator<Item = &'a Outcome>,
) -> Vec<QueueNote> {
    let mut code = current;
    let mut notes = Vec::new();
    for outcome in outcomes {
        if let Some(noted) = note(outcome, code) {
            code = noted.code;
            notes.push(noted);
        }
    }
    notes
}

/// The note of `refused`, an attempt that always appends its event: `capacity` fails over,
/// `quota` backs off; any other code records nothing.
fn refusal(refused: &Refused) -> Option<QueueNote> {
    let zone = refused
        .zone
        .as_ref()
        .map_or_else(String::new, |zone| format!(" in zone {zone}"));
    let at = format!("{} refused offer {}{zone}", refused.provider, refused.offer);
    let (code, kind, detail) = match refused.code {
        RefusalCode::Capacity => (
            QueueCode::FailingOver,
            JobEventKind::FailingOver,
            format!("{at} for capacity; the next zone, offer or provider is asked"),
        ),
        RefusalCode::Quota => {
            let until = refused
                .passed_over
                .as_ref()
                .map_or_else(String::new, |(_, until)| {
                    format!("; it is passed over until {} ms", until.millis())
                });
            (
                QueueCode::QuotaBackoff,
                JobEventKind::QuotaBackoff,
                format!("{at} for quota{until}"),
            )
        }
        RefusalCode::Shape | RefusalCode::Denied => return None,
    };
    Some(QueueNote {
        code,
        detail,
        event: Some(kind),
        host: None,
        attempt: true,
    })
}

#[cfg(test)]
#[path = "queue_status_tests.rs"]
mod tests;
