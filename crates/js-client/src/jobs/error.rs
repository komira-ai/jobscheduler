//! Why a call of the job service failed, said so that the caller knows whether the call may
//! have taken effect. This is the one part of the job client that is not generated: it is
//! about the transport, not about any call.

use std::fmt::Display;

use tonic::{Code, Status};

use crate::error::ClientError;

/// A failed call, in one of three kinds. [`JobError::may_have_acted`] is the one question a
/// caller asks before it tries a call again. The text never holds the token.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// Nothing reached the server's handling of the call: a flag, the token file or the
    /// origin rule failed; the connection or the TLS handshake failed before the first byte
    /// of the request was written; or the server refused the token.
    #[error(transparent)]
    NotSent(#[from] NotSent),
    /// The server answered with a definite status: nothing happened that the answer does not
    /// describe.
    #[error("{reason} ({})", code_name(.code))]
    Rejected {
        /// The status code.
        code: Code,
        /// The refusal's code (and detail) when the status carries a refusal, the status
        /// message otherwise; without control characters.
        reason: String,
    },
    /// The request was written and no definite answer was read: the connection was reset or
    /// timed out, or the answer was `UNAVAILABLE`, `UNKNOWN`, `INTERNAL`, `DEADLINE_EXCEEDED`,
    /// `CANCELLED` or `DATA_LOSS`, or a body that does not decode (which reads as
    /// `INTERNAL`). The call may have taken effect.
    #[error("{0}")]
    MayHaveActed(String),
}

/// Why nothing reached the server's handling of a call.
#[derive(Debug, thiserror::Error)]
pub enum NotSent {
    /// A flag, or a file it names, is unusable.
    #[error("{flag}: {why}")]
    Flag {
        /// The flag, with its leading dashes.
        flag: &'static str,
        /// What is wrong with it.
        why: String,
    },
    /// The connection or the TLS handshake failed before the request was written.
    #[error("{0}")]
    Unreached(String),
    /// The server answered `UNAUTHENTICATED` or `PERMISSION_DENIED` (the code's name).
    #[error("the server refused the token ({0})")]
    Token(String),
}

impl JobError {
    /// Whether the call may have taken effect: the request was written and no definite
    /// answer was read. Only then does a retry need the call to be idempotent.
    #[must_use]
    pub fn may_have_acted(&self) -> bool {
        matches!(self, Self::MayHaveActed(_))
    }

    /// A [`NotSent::Flag`] failure naming `flag`.
    pub(crate) fn flag(flag: &'static str, why: impl Display) -> Self {
        Self::NotSent(NotSent::Flag {
            flag,
            why: why.to_string(),
        })
    }
}

impl From<ClientError> for JobError {
    fn from(err: ClientError) -> Self {
        let not_sent = match err {
            ClientError::Flag { flag, why } => NotSent::Flag { flag, why },
            ClientError::Server(why) => NotSent::Unreached(why),
        };
        Self::NotSent(not_sent)
    }
}

/// The kind of failure `status` is, for a call to `base` whose request was `written` (its
/// first byte handed to the connection) or not. Before the write every status is
/// [`JobError::NotSent`]: the same `UNAVAILABLE` is `NotSent` when the connection failed
/// before the request and [`JobError::MayHaveActed`] after it, so the record of the write
/// decides, not the code alone.
pub(crate) fn classify(base: &str, written: bool, status: &Status) -> JobError {
    let message = printable(status.message());
    match (status.code(), written) {
        (Code::Unauthenticated | Code::PermissionDenied, _) => {
            NotSent::Token(code_name(&status.code())).into()
        }
        (_, false) => NotSent::Unreached(format!("cannot reach {base}: {message}")).into(),
        (
            Code::Unknown
            | Code::Internal
            | Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Cancelled
            | Code::DataLoss,
            true,
        ) => JobError::MayHaveActed(format!(
            "no answer from {base} after the request was sent: {message}"
        )),
        (code, true) => JobError::Rejected {
            code,
            reason: reason(status, message),
        },
    }
}

/// The reason of a definite refusal: the refusal's code and detail when the status carries
/// a refusal (`no-capable-host: <detail>`), the message otherwise.
fn reason(status: &Status, message: String) -> String {
    let refusal = js_proto::refusal::refusal_of(status);
    refusal.map_or(message, |refusal| {
        let parts = [
            refusal.code().as_str().to_owned(),
            printable(&refusal.detail),
        ];
        let parts: Vec<String> = parts.into_iter().filter(|part| !part.is_empty()).collect();
        parts.join(": ")
    })
}

/// The name of a status code in kebab case: `failed-precondition`.
pub(crate) fn code_name(code: &Code) -> String {
    let name = format!("{code:?}");
    let dashed = name.chars().enumerate().flat_map(|(at, ch)| {
        let dash = ((at > 0) & ch.is_uppercase()).then_some('-');
        dash.into_iter().chain(ch.to_lowercase())
    });
    dashed.collect()
}

/// `text` without its control characters: what a server sends is printed, never obeyed by
/// the terminal.
fn printable(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the kind of every class of status before and after the write: a token
    /// refusal is `NotSent` either way, anything before the write is `NotSent`, the codes of
    /// a lost answer after it `MayHaveActed`, and every other code after it `Rejected` with
    /// the message as its reason. It catches a classification by the code alone.
    #[test]
    fn statuses_classify_by_the_write_and_the_code() {
        let base = "https://jobs.example";
        let kind = |written: bool, code: Code| {
            let error = classify(base, written, &Status::new(code, "why\u{7}"));
            format!("{error:?}")
        };
        assert_eq!(
            kind(false, Code::Unauthenticated),
            r#"NotSent(Token("unauthenticated"))"#
        );
        assert_eq!(
            kind(true, Code::PermissionDenied),
            r#"NotSent(Token("permission-denied"))"#
        );
        assert_eq!(
            kind(false, Code::NotFound),
            r#"NotSent(Unreached("cannot reach https://jobs.example: why"))"#
        );
        for code in [
            Code::Unknown,
            Code::Internal,
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Cancelled,
            Code::DataLoss,
        ] {
            assert_eq!(
                kind(true, code),
                r#"MayHaveActed("no answer from https://jobs.example after the request was sent: why")"#,
                "{code:?}"
            );
        }
        for code in [
            Code::InvalidArgument,
            Code::NotFound,
            Code::FailedPrecondition,
            Code::AlreadyExists,
        ] {
            let got = classify(base, true, &Status::new(code, "why"));
            assert_eq!(
                format!("{got:?}"),
                format!("Rejected {{ code: {code:?}, reason: \"why\" }}")
            );
        }
    }

    /// Asserts the reason of a refused submit: the refusal's code and detail, the code alone
    /// when the detail is empty, and without control characters. It catches a refusal read
    /// as its status message.
    #[test]
    fn a_refusal_is_its_code_and_detail() {
        use js_proto::refusal::refusal_status;
        use js_proto::v1::RefusalCode;
        let rejected = |status: Status| classify("https://j.example", true, &status).to_string();
        let status = refusal_status(RefusalCode::NoCapableHost, "no host has\u{1b} 64 GPUs");
        assert_eq!(
            rejected(status),
            "no-capable-host: no host has 64 GPUs (failed-precondition)"
        );
        let status = refusal_status(RefusalCode::WallTimeNotPositive, "");
        assert_eq!(
            rejected(status),
            "wall-time-not-positive (invalid-argument)"
        );
    }

    /// Asserts the text and the kind of every error: the flag, the unreached server, the
    /// refused token, a refusal and a lost answer; and that only the last may have acted.
    #[test]
    fn every_error_prints_its_text_and_says_whether_it_may_have_acted() {
        let cases = [
            (
                JobError::flag("--server", "is wrong"),
                "--server: is wrong",
                false,
            ),
            (ClientError::Server("down".to_owned()).into(), "down", false),
            (
                ClientError::Flag {
                    flag: "--token-file",
                    why: "bad".to_owned(),
                }
                .into(),
                "--token-file: bad",
                false,
            ),
            (
                NotSent::Token("unauthenticated".to_owned()).into(),
                "the server refused the token (unauthenticated)",
                false,
            ),
            (
                JobError::Rejected {
                    code: Code::NotFound,
                    reason: "no job j-1".to_owned(),
                },
                "no job j-1 (not-found)",
                false,
            ),
            (JobError::MayHaveActed("lost".to_owned()), "lost", true),
        ];
        for (error, text, acted) in cases {
            assert_eq!(
                (error.to_string(), error.may_have_acted()),
                (text.to_owned(), acted)
            );
        }
    }
}
