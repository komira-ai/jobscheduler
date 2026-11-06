//! What a store call can fail with.
//!
//! A failure is told apart by the transport's kind, the HTTP status and the S3 error code
//! (`<Code>` of the answer), never by a message text. No error carries a secret, a session
//! token, a signature, a URL or its query, or the store's message: only statuses, codes made of
//! ASCII letters and digits, and fixed texts.

use crate::http::TransportError;

/// The S3 error codes that mean the credential is no longer valid, so a new one is the remedy:
/// S3's (`ExpiredToken`, `RequestExpired`, `InvalidToken`), and MinIO's, an S3-compatible store
/// with STS: `InvalidAccessKeyId` for a temporary key past its expiry (it forgets the key) and
/// `InvalidTokenId` for a session token it did not issue or no longer takes.
const EXPIRY_CODES: [&str; 5] = [
    "ExpiredToken",
    "RequestExpired",
    "InvalidToken",
    "InvalidAccessKeyId",
    "InvalidTokenId",
];

/// The longest S3 error code kept.
const MAX_CODE_LEN: usize = 64;

/// Why a store call failed. The client never retries: the caller owns retry and backoff.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The store could not be reached or asked to be called later: a transport failure, a
    /// timeout, a 5xx, a 429 or `SlowDown`. Retrying later may succeed.
    #[error("the store is unavailable: {0}")]
    Unavailable(Unavailable),
    /// The store refused the credential's request (403 other than an expiry): the credential
    /// may not do this. Retrying with the same credential does not help.
    #[error("the store refused the request (status {status}, code `{code}`)")]
    Denied {
        /// The status.
        status: u16,
        /// The S3 error code, or empty.
        code: String,
    },
    /// The credential has expired, by the client's clock before any request or by the store's
    /// answer (`ExpiredToken`, `RequestExpired`, `InvalidToken`, `InvalidAccessKeyId`,
    /// `InvalidTokenId`: the store no longer takes it): ask for a new one.
    #[error("the credential has expired (code `{code}`)")]
    Expired {
        /// The S3 error code, or empty when the client's own clock said so.
        code: String,
    },
    /// The bucket does not exist: a 404 of a put or a list, or a 404 of a get or a delete whose
    /// code is not `NoSuchKey`.
    #[error("the store has no such bucket or object (code `{code}`)")]
    NotFound {
        /// The S3 error code, or empty.
        code: String,
    },
    /// A body is longer than one put may carry ([`crate::layout::MAX_PUT_BYTES`]); nothing was
    /// sent.
    #[error("the body is {len} bytes, more than the {max} one put carries")]
    TooLarge {
        /// The body's length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A call's argument is outside what the client sends; nothing was sent.
    #[error("the call is refused before sending: {0}")]
    Invalid(&'static str),
    /// A ranged read starts past the object's end (416).
    #[error("the range starts past the object's end")]
    OutOfRange,
    /// The store's answer is not one the client understands, or is too long to read.
    #[error("the store's answer is malformed: {0}")]
    Malformed(&'static str),
}

/// What made the store unavailable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    /// No answer came.
    #[error("{0}")]
    Transport(TransportError),
    /// The store answered that it cannot serve now.
    #[error("status {status}, code `{code}`")]
    Status {
        /// The status.
        status: u16,
        /// The S3 error code, or empty.
        code: String,
    },
}

/// The error a failed exchange is.
pub(crate) fn transport(error: TransportError) -> StoreError {
    match error {
        TransportError::TooLarge => StoreError::Malformed("the answer is longer than is read"),
        other => StoreError::Unavailable(Unavailable::Transport(other)),
    }
}

/// The error an answer that is not a success is, from its status and S3 error code.
pub(crate) fn refusal(status: u16, body: &[u8]) -> StoreError {
    let code = error_code(body);
    if EXPIRY_CODES.contains(&code.as_str()) {
        return StoreError::Expired { code };
    }
    match (status, code.as_str()) {
        (_, "SlowDown") | (429 | 500..=599, _) => {
            StoreError::Unavailable(Unavailable::Status { status, code })
        }
        (403, _) => StoreError::Denied { status, code },
        (404, _) => StoreError::NotFound { code },
        _ => StoreError::Malformed("the store answered a status the client does not expect"),
    }
}

/// Whether a 404's body says the key is missing (`NoSuchKey`, or no code at all), rather than
/// the bucket (`NoSuchBucket`) or anything else.
pub(crate) fn missing_key(body: &[u8]) -> bool {
    matches!(error_code(body).as_str(), "NoSuchKey" | "")
}

/// The `<Code>` of an S3 error body when it is 1 to [`MAX_CODE_LEN`] ASCII letters and digits,
/// and otherwise empty.
fn error_code(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let code = text
        .split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map_or("", |(code, _)| code);
    let kept = code.len() <= MAX_CODE_LEN && code.bytes().all(|b| b.is_ascii_alphanumeric());
    match kept {
        true => code.to_owned(),
        false => String::new(),
    }
}
