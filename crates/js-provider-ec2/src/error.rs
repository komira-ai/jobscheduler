//! What a call or the settings can fail with. A call's error is matched on the cloud's
//! error code, never on its message text, which is never kept: it may echo the request.

use std::fmt;

use crate::api::Action;

/// The longest error code kept, in characters.
const KEPT: usize = 80;

/// An error code as the cloud's answer gives it (`RequestLimitExceeded`), printable ASCII
/// only and at most 80 characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorCode(String);

impl ErrorCode {
    /// The code `code`, with every character that is not printable ASCII dropped and cut to
    /// 80 characters.
    pub fn new(code: &str) -> Self {
        Self(
            code.chars()
                .filter(char::is_ascii_graphic)
                .take(KEPT)
                .collect(),
        )
    }

    /// The code as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The codes that ask the caller to slow down: `RequestLimitExceeded` (EC2 API, common
/// client errors), `ThrottlingException` (Pricing API `GetProducts`) and
/// `TooManyRequestsException` (Service Quotas `GetServiceQuota`).
const THROTTLED: [&str; 3] = [
    "RequestLimitExceeded",
    "ThrottlingException",
    "TooManyRequestsException",
];

/// The codes that say the credential may not make the call: `AuthFailure`,
/// `UnauthorizedOperation`, `OptInRequired` and `Blocked` (EC2 API, common client errors),
/// and `AccessDeniedException` (Pricing API `GetProducts` and Service Quotas
/// `GetServiceQuota`).
const DENIED: [&str; 5] = [
    "AuthFailure",
    "UnauthorizedOperation",
    "OptInRequired",
    "Blocked",
    "AccessDeniedException",
];

/// Why a call failed. A failure is never an empty answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The cloud asked the caller to slow down.
    #[error("throttled ({code})")]
    Throttled {
        /// The cloud's error code.
        code: ErrorCode,
    },
    /// The credential may not make the call.
    #[error("denied ({code})")]
    Denied {
        /// The cloud's error code.
        code: ErrorCode,
    },
    /// The cloud answered with another error code.
    #[error("refused ({code})")]
    Refused {
        /// The cloud's error code.
        code: ErrorCode,
    },
    /// The cloud could not be reached, or its answer could not be read to its end. The
    /// text is the client's own words, never the cloud's.
    #[error("cannot talk to the cloud: {0}")]
    Transport(String),
    /// The answer was not what the documentation says, or a value in it could not be read
    /// (a price that is not a decimal). The text says what, in the crate's own words.
    #[error("the answer cannot be read: {0}")]
    Unreadable(String),
    /// A list answer could not be had whole (a page walk that ended early).
    #[error("the answer is not complete")]
    Incomplete,
}

impl ApiError {
    /// The error of an answer that carried the error code `code`: [`ApiError::Throttled`]
    /// or [`ApiError::Denied`] for the codes documented as such, else
    /// [`ApiError::Refused`].
    pub fn answered(code: &str) -> Self {
        let code = ErrorCode::new(code);
        let is = |set: &[&str]| set.contains(&code.as_str());
        match (is(&THROTTLED), is(&DENIED)) {
            (true, _) => Self::Throttled { code },
            (false, true) => Self::Denied { code },
            (false, false) => Self::Refused { code },
        }
    }

    /// The cloud's error code, for an error that carries one.
    pub fn code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Throttled { code } | Self::Denied { code } | Self::Refused { code } => Some(code),
            Self::Transport(_) | Self::Unreadable(_) | Self::Incomplete => None,
        }
    }
}

/// A call of the catalog that failed, named by its action.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{action} failed: {error}")]
pub struct CallError {
    /// The call.
    pub action: Action,
    /// How it failed.
    pub error: ApiError,
}

/// Why the settings of a catalog are refused. Each names the configuration key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// The region is not a region code.
    #[error("region: not a region code (lower-case letters, digits and '-', 1 to 32)")]
    Region,
    /// The purchase list is empty.
    #[error("purchase: lists no purchase option")]
    NoPurchase,
    /// A purchase option this provider does not sell.
    #[error("purchase: {0:?} is not one of on-demand, spot, capacity-block")]
    Purchase(String),
    /// A purchase option listed twice.
    #[error("purchase: {0:?} is listed twice")]
    PurchaseTwice(String),
    /// `capacity-block` is listed without a block length, or the length is zero.
    #[error("capacity_block_hours: capacity-block needs a length of at least one hour")]
    BlockHours,
}
