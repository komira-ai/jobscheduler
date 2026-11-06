//! What a call or the settings can fail with. A call's error is matched on the reason the
//! cloud's error answer gives, never on its message text, which is never kept: it may echo
//! the request.

use std::fmt;

use crate::api::Method;

/// The longest reason kept, in characters.
const KEPT: usize = 80;

/// An error reason as the cloud's error answer gives it (`rateLimitExceeded`), visible
/// ASCII characters only (no space) and at most 80 characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorReason(String);

impl ErrorReason {
    /// The reason `reason`, with every character that is not visible ASCII (a space, a
    /// control character, anything beyond ASCII) dropped, and cut to 80 characters.
    pub fn new(reason: &str) -> Self {
        Self(
            reason
                .chars()
                .filter(char::is_ascii_graphic)
                .take(KEPT)
                .collect(),
        )
    }

    /// The reason as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorReason {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The reasons that ask the caller to slow down: `rateLimitExceeded` (Compute Engine API
/// rate quotas: "If your project exceeds a rate quota, you receive a 403 error with the
/// reason rateLimitExceeded") and `RATE_LIMIT_EXCEEDED` (Cloud Quotas troubleshooting:
/// Compute Engine rate quotas return "403 RATE_LIMIT_EXCEEDED").
const THROTTLED: [&str; 2] = ["rateLimitExceeded", "RATE_LIMIT_EXCEEDED"];

/// The reasons that say the credential may not make the call: `forbidden` (Compute Engine
/// troubleshooting VM creation: a missing `compute.regions.get` permission answers
/// `forbidden`) and `PERMISSION_DENIED` (the canonical error code of AIP-193, which the
/// Cloud Billing API answers with).
const DENIED: [&str; 2] = ["forbidden", "PERMISSION_DENIED"];

/// Why a call failed. A failure is never an empty answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The cloud asked the caller to slow down.
    #[error("throttled ({reason})")]
    Throttled {
        /// The cloud's error reason.
        reason: ErrorReason,
    },
    /// The credential may not make the call.
    #[error("denied ({reason})")]
    Denied {
        /// The cloud's error reason.
        reason: ErrorReason,
    },
    /// The cloud answered with another error reason.
    #[error("refused ({reason})")]
    Refused {
        /// The cloud's error reason.
        reason: ErrorReason,
    },
    /// The cloud could not be reached, or its answer could not be read to its end. The
    /// text is the client's own words, never the cloud's.
    #[error("cannot talk to the cloud: {0}")]
    Transport(String),
    /// The answer was not what the documentation says, or a value in it could not be read
    /// (a price whose units are not a number). The text says what, in the crate's own
    /// words.
    #[error("the answer cannot be read: {0}")]
    Unreadable(String),
    /// A list answer could not be had whole: a page walk that ended early, or an
    /// aggregated list that named a scope it could not reach.
    #[error("the answer is not complete")]
    Incomplete,
}

impl ApiError {
    /// The error of an answer that carried the error reason `reason`:
    /// [`ApiError::Throttled`] or [`ApiError::Denied`] for the reasons documented as such,
    /// else [`ApiError::Refused`].
    pub fn answered(reason: &str) -> Self {
        let reason = ErrorReason::new(reason);
        let is = |set: &[&str]| set.contains(&reason.as_str());
        match (is(&THROTTLED), is(&DENIED)) {
            (true, _) => Self::Throttled { reason },
            (false, true) => Self::Denied { reason },
            (false, false) => Self::Refused { reason },
        }
    }

    /// The cloud's error reason, for an error that carries one.
    pub fn reason(&self) -> Option<&ErrorReason> {
        match self {
            Self::Throttled { reason } | Self::Denied { reason } | Self::Refused { reason } => {
                Some(reason)
            }
            Self::Transport(_) | Self::Unreadable(_) | Self::Incomplete => None,
        }
    }
}

/// A call of the catalog that failed, named by its API method.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{method} failed: {error}")]
pub struct CallError {
    /// The call.
    pub method: Method,
    /// How it failed.
    pub error: ApiError,
}

/// Why the settings of a catalog are refused. Each names the configuration key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// The project is not a project id.
    #[error(
        "project: not a project id (6 to 30 lower-case letters, digits and '-', starting \
         with a letter and not ending with '-')"
    )]
    Project,
    /// The zone list is empty.
    #[error("zones: lists no zone")]
    NoZone,
    /// A zone is not a zone name.
    #[error("zones: {0:?} is not a zone name (a region name, '-' and a zone letter)")]
    Zone(String),
    /// A zone listed twice.
    #[error("zones: {0:?} is listed twice")]
    ZoneTwice(String),
    /// The purchase list is empty.
    #[error("purchase: lists no purchase option")]
    NoPurchase,
    /// A purchase option this provider does not sell.
    #[error("purchase: {0:?} is not one of on-demand, spot")]
    Purchase(String),
    /// A purchase option listed twice.
    #[error("purchase: {0:?} is listed twice")]
    PurchaseTwice(String),
}
