//! The delivery of a job's completion callback as the store keeps it
//! (`docs/design/job-retry-and-completion-callback.md`, section 5.5): a due call one instance
//! has claimed ([`CallbackClaim`], what [`JobStore::claim_callbacks`] hands out and
//! [`JobStore::answer_callback`] takes back), and where a job's delivery stands
//! ([`CallbackStatus`], what [`JobStore::callback_status`] reads).
//!
//! [`JobStore::claim_callbacks`]: crate::JobStore::claim_callbacks
//! [`JobStore::answer_callback`]: crate::JobStore::answer_callback
//! [`JobStore::callback_status`]: crate::JobStore::callback_status

use js_state::{StoreTime, Version};
use js_types::{FailureClass, SigningKey};

use crate::ids::JobId;

/// A due callback that one instance has claimed: what it sends, and the claim it answers. Only
/// the store makes one, and only an answer of the claim that made it counts.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The version of the callback's row the claim holds.
    pub(crate) version: Version,
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
