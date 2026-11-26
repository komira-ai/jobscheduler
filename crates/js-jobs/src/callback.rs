//! The completion callback of a job as the store keeps it
//! (`docs/design/job-retry-and-completion-callback.md`, sections 5.2 and 5.5): what a job asks
//! for ([`JobCallback`], part of its spec), a due call one instance has claimed
//! ([`CallbackClaim`], what [`JobStore::claim_callbacks`] hands out and
//! [`JobStore::answer_callback`] takes back), and where a job's delivery stands
//! ([`CallbackStatus`], what [`JobStore::callback_status`] reads).
//!
//! The delivery itself is the row `callback/<job id>` ([`CallbackRecord`]), the one place the
//! signing key is held. It is written armed in the commit that records the job, made due in the
//! commit that ends it, and erases its key when the delivery reaches a final state (delivered,
//! expired or abandoned). The job row holds the key's SHA-256 alone ([`CallbackKey::Digest`]).
//!
//! [`JobStore::claim_callbacks`]: crate::JobStore::claim_callbacks
//! [`JobStore::answer_callback`]: crate::JobStore::answer_callback
//! [`JobStore::callback_status`]: crate::JobStore::callback_status

use std::fmt;
use std::time::Duration;

use js_state::StoreTime;
use js_types::{CallbackAnswer, FailureClass, SigningKey};
use sha2::{Digest as _, Sha256};

use crate::codec::Carried;
use crate::ids::{InstanceId, JobId};

/// How long the server keeps trying a job's callback when the job names no deadline: a day
/// from the job's end, cut to its retention end.
pub(crate) const DEFAULT_CALLBACK_DEADLINE: Duration = Duration::from_secs(86_400);

/// How long a claim of a due callback holds it ([`JobStore::claim_callbacks`]): a claim not
/// answered by then lapses, and the callback is handed out again.
///
/// [`JobStore::claim_callbacks`]: crate::JobStore::claim_callbacks
pub(crate) const CALLBACK_CLAIM_LEASE: Duration = Duration::from_secs(30);

/// The wait after a callback's first failed try. Each further failure doubles it, up to
/// [`LONGEST_CALLBACK_WAIT`].
pub(crate) const FIRST_CALLBACK_WAIT: Duration = Duration::from_secs(5);

/// The longest wait between two tries of a callback.
pub(crate) const LONGEST_CALLBACK_WAIT: Duration = Duration::from_secs(600);

/// The completion callback a job asks for: where the server POSTs once the job reaches its
/// final state, the key it signs each call with, and how long it keeps trying. It is part of the
/// job's spec, so a resubmit with another URL, key or deadline is another job. Its `Debug`
/// shows the deadline alone: neither the URL, whose path or query may carry the receiver's own
/// reference, nor anything of the key.
#[derive(Clone, PartialEq, Eq)]
pub struct JobCallback {
    /// Where the call goes, as the submitter gave it.
    pub url: String,
    /// The key, as submitted, or its digest, as stored.
    pub key: CallbackKey,
    /// How long the server keeps trying, in seconds from the job's end; `None`:
    /// [`DEFAULT_CALLBACK_DEADLINE`]. Either is cut to the job's retention end.
    pub deadline_seconds: Option<u32>,
}

impl fmt::Debug for JobCallback {
    /// `JobCallback { deadline_seconds: …, .. }`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobCallback")
            .field("deadline_seconds", &self.deadline_seconds)
            .finish_non_exhaustive()
    }
}

impl JobCallback {
    /// Whether `self` and `other` ask for the same callback: the same URL and deadline, and
    /// keys with the same SHA-256, whichever form each side holds.
    pub(crate) fn same_as(&self, other: &JobCallback) -> bool {
        (self.url == other.url)
            & (self.deadline_seconds == other.deadline_seconds)
            & (self.key.digest() == other.key.digest())
    }

    /// This callback with its key replaced by the key's digest: what the job row holds.
    #[must_use]
    pub(crate) fn digested(&self) -> JobCallback {
        JobCallback {
            key: CallbackKey::Digest(self.key.digest()),
            ..self.clone()
        }
    }
}

/// The deadline from a job's end that `seconds` asks for: [`DEFAULT_CALLBACK_DEADLINE`] when it
/// asks for none.
fn deadline_of(seconds: Option<u32>) -> Duration {
    seconds.map_or(DEFAULT_CALLBACK_DEADLINE, |secs| {
        Duration::from_secs(u64::from(secs))
    })
}

/// A callback's key in the form a job holds it.
#[derive(Clone, PartialEq, Eq)]
pub enum CallbackKey {
    /// The key as submitted: what [`JobStore::insert_queued`] takes for a new job, and writes
    /// into the job's `callback/` row alone.
    ///
    /// [`JobStore::insert_queued`]: crate::JobStore::insert_queued
    Key(SigningKey),
    /// The SHA-256 of the key: what the job row holds, so every job a read returns.
    Digest(KeyDigest),
}

impl fmt::Debug for CallbackKey {
    /// `Key(..)` or `Digest(..)`, nothing of either.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CallbackKey::Key(_) => "Key(..)",
            CallbackKey::Digest(_) => "Digest(..)",
        })
    }
}

impl CallbackKey {
    /// The SHA-256 of the key, computed from the key or as stored.
    #[must_use]
    pub fn digest(&self) -> KeyDigest {
        match self {
            CallbackKey::Key(key) => KeyDigest(Sha256::digest(key.as_bytes()).into()),
            CallbackKey::Digest(digest) => *digest,
        }
    }
}

/// The SHA-256 of a callback's signing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDigest(pub(crate) [u8; 32]);

impl KeyDigest {
    /// The 32 bytes of the digest.
    #[must_use]
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A due callback that one instance has claimed: what it sends, and the claim it answers. Only
/// the store makes one, and only an answer of the claim that made it counts. Its `Debug`
/// shows the job and the `webhook-id` alone: neither the key nor the URL, whose path or query may
/// carry the receiver's own reference.
#[derive(Clone, PartialEq, Eq)]
pub struct CallbackClaim {
    /// The job the callback is for.
    pub job: JobId,
    /// Where the call goes, as the submitter gave it.
    pub url: String,
    /// The key the call is signed with.
    pub key: SigningKey,
    /// The call's `webhook-id`: minted at submit, the same on every try, and holding nothing
    /// of the job.
    pub webhook_id: String,
    /// The instance that holds the claim, and the try it is: the callback's row holds the
    /// claim while it records both.
    pub(crate) held: Held,
}

/// Which claim of a callback's row is which: the instance that made it and the number of the
/// try it made. Every claim counts a try, so no two claims of one row are the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    /// The instance that claimed the row.
    pub(crate) by: InstanceId,
    /// The row's try count the claim wrote.
    pub(crate) tries: u32,
}

impl fmt::Debug for CallbackClaim {
    /// `CallbackClaim { job: …, webhook_id: …, .. }`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallbackClaim")
            .field("job", &self.job)
            .field("webhook_id", &self.webhook_id)
            .finish_non_exhaustive()
    }
}

/// Where the delivery of a job's callback stands, as the job API's `CallbackState` tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackState {
    /// The job has not ended, so nothing is due.
    Armed,
    /// The job ended and no try has been answered 2xx or 410 yet.
    Pending,
    /// The receiver answered 2xx.
    Delivered,
    /// The delivery deadline passed first.
    Expired,
    /// The receiver answered 410.
    Abandoned,
}

/// Where the delivery of a job's callback stands, as `GetJob` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackStatus {
    /// Where it stands.
    pub state: CallbackState,
    /// How many calls were tried.
    pub tries: u32,
    /// When the last call was tried, on the store's clock.
    pub last_try_at: Option<StoreTime>,
    /// The class of the last failed try, if a try failed.
    pub last_failure: Option<FailureClass>,
    /// When the receiver answered 2xx, on the store's clock.
    pub delivered_at: Option<StoreTime>,
    /// Until when the server keeps trying, on the store's clock; `None` while the job has not
    /// ended.
    pub deadline_at: Option<StoreTime>,
}

/// The row `callback/<job id>`: the delivery of one job's callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallbackRecord {
    /// The job the callback is for.
    pub(crate) job: JobId,
    /// Where the call goes.
    pub(crate) url: String,
    /// The key each call is signed with, until the delivery reaches a final state.
    pub(crate) key: Option<SigningKey>,
    /// The call's `webhook-id`, minted at submit.
    pub(crate) webhook_id: String,
    /// The deadline the job asked for, in seconds from its end.
    pub(crate) deadline_seconds: Option<u32>,
    /// Where the delivery stands.
    pub(crate) state: CallbackState,
    /// How many calls were tried (claimed).
    pub(crate) tries: u32,
    /// When the next call is due, while it is pending.
    pub(crate) next_try_at: Option<StoreTime>,
    /// Until when the server keeps trying, once the job has ended.
    pub(crate) deadline_at: Option<StoreTime>,
    /// When the last call was tried.
    pub(crate) last_try_at: Option<StoreTime>,
    /// The class of the last failed try.
    pub(crate) last_failure: Option<FailureClass>,
    /// When the receiver answered 2xx.
    pub(crate) delivered_at: Option<StoreTime>,
    /// The instance whose claim holds the row, while one does.
    pub(crate) claimed_by: Option<InstanceId>,
    /// Until when that claim holds it.
    pub(crate) claim_until: Option<StoreTime>,
    /// The fields of a later build, carried into the next write.
    pub(crate) carried: Carried,
}

/// The wait before the try after the `tries`-th failed one: [`FIRST_CALLBACK_WAIT`], doubling
/// with each try, never above [`LONGEST_CALLBACK_WAIT`]. The exponent is capped, so no count of
/// tries overflows it.
pub(crate) fn wait_after(tries: u32) -> Duration {
    let doublings = tries.saturating_sub(1).min(16);
    FIRST_CALLBACK_WAIT
        .saturating_mul(1 << doublings)
        .min(LONGEST_CALLBACK_WAIT)
}

impl CallbackRecord {
    /// The armed row of `callback`, for the job `job`, whose key is `key`, with the minted
    /// `webhook_id`: nothing is due until the job ends.
    pub(crate) fn armed(
        job: JobId,
        callback: &JobCallback,
        key: SigningKey,
        webhook_id: String,
    ) -> CallbackRecord {
        CallbackRecord {
            job,
            url: callback.url.clone(),
            key: Some(key),
            webhook_id,
            deadline_seconds: callback.deadline_seconds,
            state: CallbackState::Armed,
            tries: 0,
            next_try_at: None,
            deadline_at: None,
            last_try_at: None,
            last_failure: None,
            delivered_at: None,
            claimed_by: None,
            claim_until: None,
            carried: Carried::default(),
        }
    }

    /// This row made due by its job's end at `now`: the first call due at once, and the
    /// deadline the job's end plus the one asked for, cut to `retention_ends`. `None` when it is
    /// not armed: a job ends once, so only its first end makes the row due.
    pub(crate) fn due(&self, now: StoreTime, retention_ends: StoreTime) -> Option<CallbackRecord> {
        let deadline = now
            .saturating_add(deadline_of(self.deadline_seconds))
            .min(retention_ends);
        (self.state == CallbackState::Armed).then(|| CallbackRecord {
            state: CallbackState::Pending,
            next_try_at: Some(now),
            deadline_at: Some(deadline),
            ..self.clone()
        })
    }

    /// When the row may next be claimed: the end of the claim that holds it, else its next
    /// try. `None` when it is not pending.
    pub(crate) fn due_at(&self) -> Option<StoreTime> {
        (self.state == CallbackState::Pending).then(|| {
            self.claim_until
                .or(self.next_try_at)
                .unwrap_or(StoreTime::ZERO)
        })
    }

    /// Whether a claim at `now` may take the row: it is pending and due, and no claim holds it.
    pub(crate) fn claimable(&self, now: StoreTime) -> bool {
        self.due_at().is_some_and(|at| at <= now)
    }

    /// Whether the row's deadline is at or before `now`.
    fn past_deadline(&self, now: StoreTime) -> bool {
        self.deadline_at.is_some_and(|deadline| deadline <= now)
    }

    /// This row with the delivery at the final state `state`: its key erased, nothing more
    /// due and no claim held.
    fn finished(self, state: CallbackState) -> CallbackRecord {
        CallbackRecord {
            state,
            key: None,
            next_try_at: None,
            claimed_by: None,
            claim_until: None,
            ..self
        }
    }

    /// What a claim by `by` at `now` makes of this claimable row: expired when its deadline
    /// has passed (nothing is sent), else held by `by` for [`CALLBACK_CLAIM_LEASE`], with one
    /// more try counted at `now`. The claim it makes, when it makes one, is returned beside.
    pub(crate) fn claimed(
        self,
        by: &InstanceId,
        now: StoreTime,
    ) -> (CallbackRecord, Option<CallbackClaim>) {
        if self.past_deadline(now) {
            return (self.finished(CallbackState::Expired), None);
        }
        let Some(key) = self.key.clone() else {
            // A pending row always holds its key; one without it has nothing to sign with.
            return (self.finished(CallbackState::Expired), None);
        };
        let tries = self.tries.saturating_add(1);
        let claim = CallbackClaim {
            job: self.job.clone(),
            url: self.url.clone(),
            key,
            webhook_id: self.webhook_id.clone(),
            held: Held {
                by: by.clone(),
                tries,
            },
        };
        let held = CallbackRecord {
            tries,
            last_try_at: Some(now),
            claimed_by: Some(by.clone()),
            claim_until: Some(now.saturating_add(CALLBACK_CLAIM_LEASE)),
            ..self
        };
        (held, Some(claim))
    }

    /// Whether `held` is the claim this row holds now.
    pub(crate) fn holds(&self, held: &Held) -> bool {
        (self.state == CallbackState::Pending)
            & (self.claimed_by.as_ref() == Some(&held.by))
            & (self.tries == held.tries)
    }

    /// This row with `answer` to the try its claim made, recorded at `now`: delivered on a 2xx,
    /// abandoned on a 410, and on a failure its class kept and the next try due after
    /// [`wait_after`] the tries so far, but no later than the deadline. A failure at or past the
    /// deadline expires it.
    pub(crate) fn answered(self, answer: CallbackAnswer, now: StoreTime) -> CallbackRecord {
        match answer {
            CallbackAnswer::Delivered => CallbackRecord {
                delivered_at: Some(now),
                ..self.finished(CallbackState::Delivered)
            },
            CallbackAnswer::Abandoned => self.finished(CallbackState::Abandoned),
            CallbackAnswer::Failed(class) => {
                let failed = CallbackRecord {
                    last_failure: Some(class),
                    ..self
                };
                if failed.past_deadline(now) {
                    return failed.finished(CallbackState::Expired);
                }
                let next = now.saturating_add(wait_after(failed.tries));
                CallbackRecord {
                    next_try_at: Some(failed.deadline_at.map_or(next, |end| next.min(end))),
                    claimed_by: None,
                    claim_until: None,
                    ..failed
                }
            }
        }
    }

    /// This row with its deadline raised to `floor` (a boot plus its grace), when it is pending
    /// and its deadline is earlier: an outage of the server expires no callback. `None` when
    /// nothing changes.
    pub(crate) fn raised_at_boot(&self, floor: StoreTime) -> Option<CallbackRecord> {
        let raise = self.deadline_at.is_some_and(|deadline| deadline < floor);
        ((self.state == CallbackState::Pending) & raise).then(|| CallbackRecord {
            deadline_at: Some(floor),
            ..self.clone()
        })
    }

    /// Where the delivery stands, as [`CallbackStatus`] tells it.
    pub(crate) fn status(&self) -> CallbackStatus {
        CallbackStatus {
            state: self.state,
            tries: self.tries,
            last_try_at: self.last_try_at,
            last_failure: self.last_failure,
            delivered_at: self.delivered_at,
            deadline_at: self.deadline_at,
        }
    }
}

#[cfg(test)]
#[path = "callback_tests.rs"]
mod tests;
