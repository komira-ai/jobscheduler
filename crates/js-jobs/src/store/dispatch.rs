//! The dispatch queue (`docs/design/multi-instance.md` section 0.2): the messages
//! `dispatch/<host>/<seq>` that tell a host's agent to start or stop one allocation, handed out
//! by any server and deleted when the agent acknowledges them.
//!
//! - **A start** is written in the transaction that books the host for a job that starts on its
//!   placement ([`JobStore::place`], [`JobStore::place_with`], [`JobStore::place_claimed`]), and,
//!   for a whole-host job, which starts only once its host is ready, in the transaction that moves
//!   it from placed to starting ([`JobStore::transition`]). A placement that does not commit
//!   writes none.
//! - **A stop** is written in the transaction that asks for a placed job's cancel
//!   ([`JobStore::request_cancel`]) or that ends a job by the server's own decision (a loss, a
//!   start or queue timeout, a failed prepare, a cancel of a job its host no longer holds): every
//!   end but the ones the host reported, which the host has already acted on. It is written only
//!   when the start may have reached the host: the start was handed out, or no start is queued
//!   (it was acknowledged, or an earlier release placed the job and queued none) for a job whose
//!   start was due. A start never handed out is deleted instead, and nothing is sent.
//! - **Every end deletes the job's start**, so no start outlives its job, and the retention sweep
//!   deletes the rest of a job's messages with its row ([`JobStore::tick`]).
//! - **A claim** ([`JobStore::claim_dispatch`]) takes each due message of a host in its own
//!   claimed transaction (`FOR UPDATE SKIP LOCKED` on Postgres), so two servers never hand out
//!   one message at once, and holds it for a lease on the store's clock; a message not
//!   acknowledged by then is handed out again. Delivery is at least once: the agent's start is
//!   idempotent on the job id, fenced by the allocation id.
//! - **An acknowledgement** ([`JobStore::ack_dispatch`]) names allocations and deletes the
//!   messages of them that were handed out; one already gone, or never handed out, is left as it
//!   is.
//!
//! A row this build cannot read (a later build's, or one that does not parse) is left as it is
//! and logged: no claim hands it out, no acknowledgement deletes it, and no end counts it.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use js_agent_wire::JobStartSpec;
use js_state::{Claim, Cond, Store, StoreTime, Txn, TxnRead, Version, Versioned};
use js_types::HostId;

use super::{JobStore, same_key};
use crate::codec::DecodeError;
use crate::codec::dispatch_fields::{decode_dispatch, encode_dispatch};
use crate::error::JobStoreError;
use crate::ids::{AllocationId, IdSource};
use crate::keys;
use crate::record::{
    AllocationRecord, AllocationStep, Booked, DispatchKind, DispatchMessage, JobRecord, JobState,
    Needs, StartFailure,
};
use crate::result::JobEnd;

/// A message of the queue as read: its key, its version and the message.
type Queued = (String, Version, DispatchMessage);

/// The key of `message`.
fn message_key(message: &DispatchMessage) -> String {
    keys::dispatch(
        &message.host,
        message.queued_at,
        message.allocation_id,
        message.kind,
    )
}

/// The message the row `key` holds: a row under another message's key is
/// [`DecodeError::WrongKey`].
fn message_at(key: &str, value: &[u8]) -> Result<DispatchMessage, DecodeError> {
    decode_dispatch(value).and_then(|message| same_key(key, message_key(&message), message))
}

/// The messages of `rows`, in their order; a row this build cannot read is left out and logged.
fn readable(rows: Vec<(String, Versioned)>) -> Vec<Queued> {
    let mut read = Vec::with_capacity(rows.len());
    for (key, row) in rows {
        match message_at(&key, &row.value) {
            Ok(message) => read.push((key, row.version, message)),
            Err(why) => {
                tracing::warn!(%key, %why, "a dispatch message this build cannot read is left as it is")
            }
        }
    }
    read
}

/// `txn` with a put of `message` under its key, on `cond`.
fn put_message(txn: Txn, message: &DispatchMessage, cond: Cond) -> Result<Txn, JobStoreError> {
    Ok(txn.put(message_key(message), encode_dispatch(message)?, cond))
}

/// Whether `end` is one the host reported, so the host has already acted on it and is sent no
/// stop: an exit, a stop at the wall limit or for memory (the agent enforces both), a failed
/// input, and the agent's own refusal of the start.
pub(super) fn told_by_host(end: &JobEnd) -> bool {
    matches!(
        end,
        JobEnd::Exited { .. }
            | JobEnd::WallLimit { .. }
            | JobEnd::Memory { .. }
            | JobEnd::InputFailed { .. }
            | JobEnd::Refused { .. }
            | JobEnd::FailedToStart(StartFailure::AgentRefused | StartFailure::InputUnavailable)
    )
}

/// The Unix time in milliseconds of `at`, saturating, as a start's placement time is sent.
fn millis(at: StoreTime) -> u64 {
    u64::try_from(at.as_duration().as_millis()).unwrap_or(u64::MAX)
}

/// `txn` with the start of `allocation` queued at `now`, when its job starts on its placement
/// (a whole-host job starts once its host is ready, [`JobStore::transition`]). `body` is the
/// start the agent is sent, given its job id, allocation id and placement time here.
pub(super) fn with_start(
    txn: Txn,
    allocation: &AllocationRecord,
    body: Option<JobStartSpec>,
    now: StoreTime,
) -> Result<Txn, JobStoreError> {
    if allocation.booked == Booked::WholeHost {
        return Ok(txn);
    }
    let start = body.map(|body| JobStartSpec {
        job_id: allocation.job.to_string(),
        allocation_id: allocation.allocation_id.to_string(),
        placed_at_ms: Some(millis(now)),
        ..body
    });
    let message = DispatchMessage {
        start,
        ..DispatchMessage::new(DispatchKind::Start, allocation, now)
    };
    put_message(txn, &message, Cond::Absent)
}

/// The decision of one claim of the message `key` for `claimer`, on what `read` shows at `now`:
/// the transaction that holds it until `until`, and the message as it then stands; `None` when
/// the message is gone, cannot be read, or is held by another claim.
fn decide_claim(
    read: &mut dyn TxnRead,
    key: &str,
    claimer: &str,
    (now, until): (StoreTime, StoreTime),
) -> Result<Option<(Txn, DispatchMessage)>, JobStoreError> {
    let Some(row) = read.get(key)? else {
        return Ok(None);
    };
    let message = message_at(key, &row.value).ok();
    let Some(message) = message.filter(|message| message.is_due(now)) else {
        return Ok(None);
    };
    let claimed = DispatchMessage {
        claimed_by: Some(claimer.to_owned()),
        claim_until: Some(until),
        ..message
    };
    let txn = put_message(Txn::new(), &claimed, Cond::Version(row.version))?;
    Ok(Some((txn, claimed)))
}

impl<S: Store, I: IdSource> JobStore<S, I> {
    /// Every message queued for `host` that this build can read, in the order the agent takes
    /// them, whether handed out or not.
    ///
    /// # Errors
    /// [`JobStoreError::Store`].
    pub fn dispatch_queue(&self, host: &HostId) -> Result<Vec<DispatchMessage>, JobStoreError> {
        let queued = self.queue_of(host)?;
        Ok(queued.into_iter().map(|(_, _, message)| message).collect())
    }

    /// Hands out the messages of `host` that are due when the store's clock is read: each one
    /// never handed out, or whose last hand-out's hold ended before that reading
    /// ([`DispatchMessage::is_due`]). Each is claimed in a transaction of its own on its locked
    /// row (`FOR UPDATE SKIP LOCKED` on Postgres) and read again there, so a message another
    /// server holds or has just claimed is left to it, and never returned to two servers at once.
    /// A claimed message takes `claimer` as [`DispatchMessage::claimed_by`] and the reading plus
    /// `lease` as [`DispatchMessage::claim_until`]. Returns the messages it claimed, as written,
    /// in queue order; none when none is due.
    ///
    /// # Errors
    /// [`JobStoreError::Store`], also when the store's clock cannot be read; what was claimed
    /// before the failure stays claimed until its lease ends.
    pub fn claim_dispatch(
        &self,
        host: &HostId,
        claimer: &str,
        lease: Duration,
    ) -> Result<Vec<DispatchMessage>, JobStoreError> {
        let now = self.store.now()?;
        let held = (now, now.saturating_add(lease));
        let mut claimed = Vec::new();
        for (key, _, message) in self.queue_of(host)? {
            if message.is_due(now) {
                claimed.extend(self.claim_one(&key, claimer, held)?);
            }
        }
        Ok(claimed)
    }

    /// One claim of [`JobStore::claim_dispatch`]: the message `key` as claimed, or `None` when
    /// it was not.
    fn claim_one(
        &self,
        key: &str,
        claimer: &str,
        held: (StoreTime, StoreTime),
    ) -> Result<Option<DispatchMessage>, JobStoreError> {
        let mut failed = None;
        let mut taken = None;
        let claim = [key.to_owned()];
        let outcome =
            self.store.claim_decide(
                &claim,
                &mut |read| match decide_claim(read, key, claimer, held) {
                    Ok(decided) => decided.map(|(txn, message)| {
                        taken = Some(message);
                        txn
                    }),
                    Err(error) => {
                        failed = Some(error);
                        None
                    }
                },
            );
        self.writes.fetch_add(1, Ordering::SeqCst);
        match (outcome?, taken) {
            (Claim::Committed(_), Some(message)) => Ok(Some(message)),
            _ => failed.map_or(Ok(None), Err),
        }
    }

    /// The agent of `host` acknowledges the messages of `allocations`: every message of the host
    /// for one of them that was handed out ([`DispatchMessage::claimed_by`]) is deleted, in one
    /// transaction, and their number returned. An allocation with no such message (acknowledged
    /// already, never queued, or another host's) changes nothing, and neither does a message not
    /// yet handed out, which the agent has not seen: a stop queued after its start was handed out
    /// stays queued when the agent acknowledges the start.
    ///
    /// # Errors
    /// [`JobStoreError::Store`]; nothing is deleted then.
    pub fn ack_dispatch(
        &self,
        host: &HostId,
        allocations: &[AllocationId],
    ) -> Result<usize, JobStoreError> {
        let named: BTreeSet<AllocationId> = allocations.iter().copied().collect();
        let mut txn = Txn::new();
        let mut acked = 0;
        for (key, _, message) in self.queue_of(host)? {
            let seen = message.claimed_by.is_some() & named.contains(&message.allocation_id);
            if seen {
                // A message's key is never used again, so whatever claim it holds now, it is
                // the message the agent saw: the delete needs no precondition.
                txn = txn.delete(key, Cond::Any);
                acked += 1;
            }
        }
        if acked > 0 {
            // A delete with no precondition has nothing to conflict on.
            let _committed = self.write(txn)?;
        }
        Ok(acked)
    }

    /// The readable messages of `host`, in queue order.
    fn queue_of(&self, host: &HostId) -> Result<Vec<Queued>, JobStoreError> {
        Ok(readable(
            self.store.scan_prefix(&keys::dispatch_prefix(host))?,
        ))
    }

    /// `txn`, which moves `job` (as read) to `next` at `now`, with the start of its allocation
    /// queued when the move is the one a whole-host job's start waits for, placed to starting,
    /// and the allocation is booked and its cancel not asked for; the allocation's row, at the
    /// version read, is then a precondition. Any other move is `txn` as it is.
    pub(super) fn start_on_move(
        &self,
        txn: Txn,
        job: &JobRecord,
        next: JobState,
        now: StoreTime,
    ) -> Result<Txn, JobStoreError> {
        let whole = (job.spec.needs == Needs::WholeHost)
            & (job.state == JobState::Placed)
            & (next == JobState::Starting)
            & !job.cancel_requested;
        let read = if whole {
            self.read_allocation(&job.id)?
        } else {
            None
        };
        let booked = read.filter(|(allocation, _)| allocation.step == AllocationStep::Booked);
        let Some((allocation, version)) = booked else {
            return Ok(txn);
        };
        let txn = txn.check(keys::allocation(&job.id), Cond::Version(version));
        let message = DispatchMessage::new(DispatchKind::Start, &allocation, now);
        put_message(txn, &message, Cond::Absent)
    }

    /// `txn`, which ends `job` (as read before it) or asks for its cancel, with the queue of
    /// `allocation`'s host brought in line when the allocation is live: its starts deleted, each
    /// at the version read (a claim in between makes the commit stale), and, when `stop` (the
    /// end is the server's decision, not the host's report), a stop queued at `now` if the
    /// start may have reached the host and no stop is queued yet. The start may have reached it
    /// when it was handed out, or when none is queued for a job whose start was due (a
    /// whole-host job's start is not due until it is starting) and whose cancel, which would
    /// have decided this already, was not asked for before.
    pub(super) fn queue_stop(
        &self,
        txn: Txn,
        job: &JobRecord,
        allocation: Option<&AllocationRecord>,
        stop: bool,
        now: StoreTime,
    ) -> Result<Txn, JobStoreError> {
        let live = allocation.filter(|allocation| allocation.step != AllocationStep::Freed);
        let Some(allocation) = live else {
            return Ok(txn);
        };
        let mut txn = txn;
        let (mut started, mut handed_out, mut stopped) = (false, false, false);
        let queued = self.queue_of(&allocation.host)?.into_iter();
        let own =
            queued.filter(|(_, _, message)| message.allocation_id == allocation.allocation_id);
        for (key, version, message) in own {
            match message.kind {
                DispatchKind::Start => {
                    started = true;
                    handed_out |= message.claimed_by.is_some();
                    txn = txn.delete(key, Cond::Version(version));
                }
                DispatchKind::Stop => stopped = true,
            }
        }
        let due = !((job.spec.needs == Needs::WholeHost) & (job.state == JobState::Placed));
        let reached = handed_out | (!started & due & !job.cancel_requested);
        if !(stop & reached & !stopped) {
            return Ok(txn);
        }
        let message = DispatchMessage::new(DispatchKind::Stop, allocation, now);
        put_message(txn, &message, Cond::Absent)
    }

    /// `txn` with every readable message of the jobs of `jobs` deleted, whatever claim it holds:
    /// the messages of jobs whose rows the retention sweep deletes. A job's messages are on the
    /// host of its last placement, the only one that can hold any.
    pub(super) fn drop_messages(&self, txn: Txn, jobs: &[JobRecord]) -> Result<Txn, JobStoreError> {
        let hosts: BTreeSet<&HostId> = jobs.iter().filter_map(|job| job.host.as_ref()).collect();
        let ids: BTreeSet<&crate::ids::JobId> = jobs.iter().map(|job| &job.id).collect();
        let mut txn = txn;
        for host in hosts {
            let queued = self.queue_of(host)?.into_iter();
            for (key, _, _) in queued.filter(|(_, _, message)| ids.contains(&message.job)) {
                txn = txn.delete(key, Cond::Any);
            }
        }
        Ok(txn)
    }
}

#[cfg(test)]
mod tests {
    use js_state::{StoreError, TxnRead, Versioned};

    use super::*;
    use crate::result::Output;

    /// A view whose every read fails.
    struct Failing;

    impl TxnRead for Failing {
        fn get(&mut self, _key: &str) -> Result<Option<Versioned>, StoreError> {
            Err(StoreError::Failed("the read failed".to_owned()))
        }

        fn scan_prefix(&mut self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
            Err(StoreError::Failed("the scan failed".to_owned()))
        }
    }

    // Catches a claim that takes a failed read for an absent message (and so answers nothing
    // instead of the failure): the decision answers the store's error.
    #[test]
    fn a_claim_decided_on_a_failed_read_is_the_failure() {
        let at = StoreTime::ZERO;
        let decided = decide_claim(&mut Failing, "dispatch/host-a/x", "server-1", (at, at));
        assert_eq!(
            decided.map(drop),
            Err(JobStoreError::Store(StoreError::Failed(
                "the read failed".to_owned()
            )))
        );
        assert!(Failing.scan_prefix("dispatch/").is_err());
    }

    // Catches a stop sent for an end the host reported (it has already acted on it) and none
    // sent for an end the server decided.
    #[test]
    fn only_the_servers_own_ends_send_a_stop() {
        let output = Output::default();
        let reported = [
            JobEnd::Exited {
                exit_code: 0,
                output: output.clone(),
            },
            JobEnd::WallLimit {
                exit_code: None,
                output: output.clone(),
            },
            JobEnd::Memory {
                exit_code: None,
                output: output.clone(),
            },
            JobEnd::InputFailed {
                exit_code: None,
                input_failure: "tree-gone".to_owned(),
                output: output.clone(),
            },
            JobEnd::Refused {
                failure: StartFailure::AgentRefused,
                detail: "image-pull".to_owned(),
            },
            JobEnd::FailedToStart(StartFailure::AgentRefused),
            JobEnd::FailedToStart(StartFailure::InputUnavailable),
        ];
        for end in &reported {
            assert!(told_by_host(end), "{end:?}");
        }
        let decided = [
            JobEnd::Lost,
            JobEnd::Cancelled {
                exit_code: None,
                output,
            },
            JobEnd::FailedToStart(StartFailure::NoCapacity),
            JobEnd::FailedToStart(StartFailure::StartTimeout),
            JobEnd::FailedToStart(StartFailure::ExclusiveNotReady),
        ];
        for end in &decided {
            assert!(!told_by_host(end), "{end:?}");
        }
    }

    // Catches a placement time sent in another unit or wrapped past the end of `u64`.
    #[test]
    fn a_start_is_sent_its_placement_time_in_milliseconds() {
        let at = StoreTime::from_duration(Duration::from_millis(1_234));
        assert_eq!(millis(at), 1_234);
        let far = StoreTime::from_duration(Duration::from_secs(u64::MAX));
        assert_eq!(millis(far), u64::MAX);
    }
}
