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
pub(crate) fn classify(base: &str, secret: &str, written: bool, status: &Status) -> JobError {
    let message = clean(status.message(), secret);
    let local = LOCAL
        .iter()
        .any(|prefix| status.message().starts_with(prefix));
    match (status.code(), written, local) {
        (Code::Unauthenticated | Code::PermissionDenied, _, _) => {
            NotSent::Token(code_name(&status.code())).into()
        }
        (_, false, _) => NotSent::Unreached(format!("cannot reach {base}: {message}")).into(),
        (_, true, true)
        | (
            Code::Unknown
            | Code::Internal
            | Code::Unavailable
            | Code::DeadlineExceeded
            | Code::Cancelled
            | Code::DataLoss,
            true,
            false,
        ) => JobError::MayHaveActed(format!(
            "no answer from {base} after the request was sent: {message}"
        )),
        (code, true, false) => JobError::Rejected {
            code,
            reason: reason(status, message, secret),
        },
    }
}

/// The beginnings of the messages of the statuses tonic makes itself when it cannot read an
/// answer (one too large, compressed in a way the client did not offer, or that does not
/// decompress): they come with codes a server could send too, but say nothing of what the
/// server did with the request.
const LOCAL: [&str; 3] = [
    "Error, decoded message length too large",
    "Content is compressed with",
    "Error decompressing",
];

/// The failure of a call that got no complete answer within its limit: before the request
/// was written nothing reached the server; after it, the call may have taken effect.
pub(crate) fn timed_out(base: &str, written: bool, limit: std::time::Duration) -> JobError {
    let seconds = limit.as_secs();
    match written {
        true => JobError::MayHaveActed(format!(
            "no answer from {base} within {seconds}s after the request was sent"
        )),
        false => NotSent::Unreached(format!("cannot reach {base} within {seconds}s")).into(),
    }
}

/// The reason of a definite refusal: the refusal's code and detail when the status carries
/// a refusal (`no-capable-host: <detail>`), the message otherwise.
fn reason(status: &Status, message: String, secret: &str) -> String {
    let refusal = js_proto::refusal::refusal_of(status);
    refusal.map_or(message, |refusal| {
        let parts = [
            refusal.code().as_str().to_owned(),
            clean(&refusal.detail, secret),
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

/// What a server sent, fit to keep in an error: without its control characters (it is
/// printed, never obeyed by the terminal) and with the token, should the server repeat it,
/// replaced by `<token>`.
fn clean(text: &str, secret: &str) -> String {
    let printable: String = text.chars().filter(|ch| !ch.is_control()).collect();
    // The token is never empty: the token file's rules refuse an empty one.
    printable.replace(secret, "<token>")
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
            let error = classify(base, "tok-1", written, &Status::new(code, "why\u{7}"));
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
            let got = classify(base, "tok-1", true, &Status::new(code, "why"));
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
        let rejected =
            |status: Status| classify("https://j.example", "tok-1", true, &status).to_string();
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

    /// Asserts that the token, when a server repeats it in a message or in a refusal's
    /// detail, is kept out of every kind of error: a refusal, a lost answer and a failure
    /// before the write, by their whole text.
    #[test]
    fn a_token_the_server_repeats_is_replaced() {
        use js_proto::refusal::refusal_status;
        use js_proto::v1::RefusalCode;
        let said = |written: bool, status: Status| {
            classify("https://j.example", "tok-1", written, &status).to_string()
        };
        assert_eq!(
            said(true, Status::not_found("no job for tok-1")),
            "no job for <token> (not-found)"
        );
        assert_eq!(
            said(true, Status::internal("lost tok-1")),
            "no answer from https://j.example after the request was sent: lost <token>"
        );
        assert_eq!(
            said(false, Status::unavailable("tok-1?")),
            "cannot reach https://j.example: <token>?"
        );
        let refusal = refusal_status(RefusalCode::NoCapableHost, "tok-1 is not a host");
        assert_eq!(
            said(true, refusal),
            "no-capable-host: <token> is not a host (failed-precondition)"
        );
    }

    /// Asserts that the statuses tonic makes itself when it cannot read an answer (too
    /// large, compressed unasked, not decompressing) are lost answers after the write, while
    /// the same codes from a server are definite refusals; and the kinds of a call that ran
    /// out of time before and after its request was written.
    #[test]
    fn an_answer_the_client_could_not_read_may_have_acted() {
        let kind =
            |status: Status| classify("https://j.example", "tok-1", true, &status).may_have_acted();
        let large = "Error, decoded message length too large: found 9 bytes, the limit is: 4 bytes";
        assert!(kind(Status::out_of_range(large)));
        assert!(kind(Status::unimplemented(
            "Content is compressed with `gzip` which isn't supported"
        )));
        assert!(kind(Status::resource_exhausted(
            "Error decompressing: size limit"
        )));
        assert!(!kind(Status::out_of_range("page token past the end")));
        assert!(!kind(Status::unimplemented("no such method")));
        let limit = std::time::Duration::from_secs(30);
        assert_eq!(
            timed_out("https://j.example", true, limit).to_string(),
            "no answer from https://j.example within 30s after the request was sent"
        );
        assert_eq!(
            format!("{:?}", timed_out("https://j.example", false, limit)),
            "NotSent(Unreached(\"cannot reach https://j.example within 30s\"))"
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
