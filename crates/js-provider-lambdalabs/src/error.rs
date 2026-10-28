//! What a call can fail with. Lambda's own text (`message`, `suggestion`) is never kept:
//! it may echo the request, and the documentation says to match on `code`.

use std::time::Duration;

/// The longest code or request id kept, in characters.
const KEPT: usize = 80;

/// An error `code` of the API: the documented ones by name, any other as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// `global/invalid-api-key`: the key is invalid, expired or deleted (401).
    InvalidApiKey,
    /// `global/forbidden`: the key may not do this (403).
    Forbidden,
    /// `global/account-inactive`: the account is not verified or has no payment method.
    AccountInactive,
    /// `global/invalid-address`: the billing address is invalid (403).
    InvalidAddress,
    /// `global/object-does-not-exist`: the named instance, type, region or key does not
    /// exist. On a by-id read this is the one answer that means the instance is gone.
    ObjectDoesNotExist,
    /// `global/not-found`: does not exist, or the caller may not see it. Not evidence of
    /// absence.
    NotFound,
    /// `global/rate-limited` (429).
    RateLimited,
    /// `global/internal-error` (500).
    InternalError,
    /// `global/invalid-parameters` (400).
    InvalidParameters,
    /// `instance-operations/launch/insufficient-capacity`.
    InsufficientCapacity,
    /// `global/quota-exceeded`.
    QuotaExceeded,
    /// `instance-operations/launch/file-system-in-wrong-region`.
    FileSystemInWrongRegion,
    /// A `provider/...` code (an upstream failure, such as `provider/internal-unavailable`).
    /// It can be a gateway failure after the request landed, so it never reads as a
    /// definite refusal.
    Provider(String),
    /// Any other code, printable ASCII only and shortened.
    Other(String),
    /// The answer carried no readable `error.code`.
    Unparsed,
}

impl ErrorCode {
    /// The code a body carried, as one of the above.
    pub fn parse(code: &str) -> Self {
        match code {
            "global/invalid-api-key" => Self::InvalidApiKey,
            "global/forbidden" => Self::Forbidden,
            "global/account-inactive" => Self::AccountInactive,
            "global/invalid-address" => Self::InvalidAddress,
            "global/object-does-not-exist" => Self::ObjectDoesNotExist,
            "global/not-found" => Self::NotFound,
            "global/rate-limited" => Self::RateLimited,
            "global/internal-error" => Self::InternalError,
            "global/invalid-parameters" => Self::InvalidParameters,
            "instance-operations/launch/insufficient-capacity" => Self::InsufficientCapacity,
            "global/quota-exceeded" => Self::QuotaExceeded,
            "instance-operations/launch/file-system-in-wrong-region" => {
                Self::FileSystemInWrongRegion
            }
            other if other.starts_with("provider/") => Self::Provider(plain(other, KEPT)),
            other => Self::Other(plain(other, KEPT)),
        }
    }

    /// The code as Lambda spells it (`unparsed` when there was none).
    pub fn as_str(&self) -> &str {
        match self {
            Self::InvalidApiKey => "global/invalid-api-key",
            Self::Forbidden => "global/forbidden",
            Self::AccountInactive => "global/account-inactive",
            Self::InvalidAddress => "global/invalid-address",
            Self::ObjectDoesNotExist => "global/object-does-not-exist",
            Self::NotFound => "global/not-found",
            Self::RateLimited => "global/rate-limited",
            Self::InternalError => "global/internal-error",
            Self::InvalidParameters => "global/invalid-parameters",
            Self::InsufficientCapacity => "instance-operations/launch/insufficient-capacity",
            Self::QuotaExceeded => "global/quota-exceeded",
            Self::FileSystemInWrongRegion => {
                "instance-operations/launch/file-system-in-wrong-region"
            }
            Self::Provider(code) | Self::Other(code) => code,
            Self::Unparsed => "unparsed",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(self.as_str())
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// The API answered with an error status.
    #[error("the API answered {status} {code}")]
    Api {
        /// The HTTP status.
        status: u16,
        /// The error `code` of the body.
        code: ErrorCode,
        /// The `request_id` of the body, which support can find a call by.
        request_id: Option<String>,
    },
    /// The API answered 429.
    #[error("the API is rate limiting this client")]
    RateLimited {
        /// The wait a `Retry-After` header of whole seconds asked for.
        retry_after: Option<Duration>,
        /// The `request_id` of the body.
        request_id: Option<String>,
    },
    /// The API could not be reached, or the answer could not be read to its end in time.
    #[error("cannot talk to the API: {0}")]
    Transport(String),
    /// The API answered success with a body that is not what the documentation says.
    #[error("the answer cannot be read: {0}")]
    Malformed(String),
    /// A list answer said more pages follow, so what it holds is not everything.
    #[error("the list answer is not complete: it carries a page token")]
    Incomplete,
}

impl ApiError {
    /// Whether this is the API saying, for a read by id, that the instance does not exist:
    /// status 404 with `global/object-does-not-exist`. `global/not-found` may also mean
    /// "not permitted", and every other failure says nothing about the instance.
    ///
    /// Meaningful for [`LambdaApi::get_instance`](crate::LambdaApi::get_instance) only:
    /// the same error from a list or the instance types would not mean that an instance
    /// is gone, and must not be read as such.
    pub fn is_absent(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: 404,
                code: ErrorCode::ObjectDoesNotExist,
                ..
            }
        )
    }
}

/// `text` with every character that is not printable ASCII dropped, cut to `max`
/// characters.
pub(crate) fn plain(text: &str, max: usize) -> String {
    text.chars()
        .filter(char::is_ascii_graphic)
        .take(max)
        .collect()
}

/// `text` cut to `max` characters, for an error that quotes a parser.
pub(crate) fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
