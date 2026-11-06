//! The answers that are not a call's response: the error table of the crate rustdoc, one
//! [`Answer`] per row, and the JSON body each is written as.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use js_proto::refusal::refusal_of;
use js_proto::v1::RefusalCode;
use serde_json::{Map, Value};
use tonic::{Code, Status};

use crate::annotation::Verb;

/// The field a wall-time refusal names: the job's wall limit, by its JSON name.
const WALL_LIMIT_FIELD: &str = "spec.wallLimitSeconds";

/// The `Content-Type` of every body the gateway writes.
const JSON: &str = "application/json";

/// An answer that is not a call's response: a status and `{"error": "<code>"}`, with
/// `"field"` (the first offending field, by its JSON name) and `"reason"` (a fixed text
/// that never repeats a value of the request) on a 400. A 405 also carries an `Allow`
/// header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    status: StatusCode,
    code: &'static str,
    field: Option<String>,
    reason: Option<&'static str>,
    allow: Vec<Verb>,
}

impl Answer {
    fn coded(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            field: None,
            reason: None,
            allow: Vec::new(),
        }
    }

    /// A 400 of `code` naming `field`, with `reason`.
    fn refused(code: &'static str, field: Option<String>, reason: &'static str) -> Self {
        Self {
            field,
            reason: Some(reason),
            ..Self::coded(StatusCode::BAD_REQUEST, code)
        }
    }

    /// The 400 `invalid-argument` of a request the gateway cannot turn into the call's
    /// request message: `field` names the first offending field, if one is to blame.
    pub(crate) fn invalid(field: Option<String>, reason: &'static str) -> Self {
        Self::refused("invalid-argument", field, reason)
    }

    /// The 500 `internal`: a failure whose cause stays out of the answer.
    pub(crate) fn internal() -> Self {
        Self::coded(StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }

    /// The 404 of a path under a resource of the gateway that no route matches.
    pub(crate) fn not_found() -> Self {
        Self::coded(StatusCode::NOT_FOUND, "not-found")
    }

    /// The 405 of a path that routes match for other methods only, `allow`.
    pub(crate) fn method_not_allowed(allow: Vec<Verb>) -> Self {
        Self {
            allow,
            ..Self::coded(StatusCode::METHOD_NOT_ALLOWED, "method-not-allowed")
        }
    }

    /// The 413 of a body over [`crate::BODY_LIMIT`].
    pub(crate) fn too_large() -> Self {
        Self::coded(StatusCode::PAYLOAD_TOO_LARGE, "request-too-large")
    }

    /// The answer to a refused submit of kind `code`: the refusal rows of the error table.
    /// The value that is no refusal, which is also how a code this version does not know
    /// reads, is a 500 `internal`.
    #[must_use]
    pub fn of_refusal(code: RefusalCode) -> Self {
        match code {
            RefusalCode::Unspecified => Self::internal(),
            RefusalCode::JobIdConflict => Self::coded(StatusCode::CONFLICT, "job-id-conflict"),
            RefusalCode::NoCapableHost => {
                Self::coded(StatusCode::UNPROCESSABLE_ENTITY, "no-capable-host")
            }
            RefusalCode::WallTimeNotPositive => Self::refused(
                "wall-time-not-positive",
                Some(WALL_LIMIT_FIELD.to_owned()),
                "the job has no wall limit, or one of 0",
            ),
            RefusalCode::WallTimeAboveCeiling => Self::refused(
                "wall-time-above-ceiling",
                Some(WALL_LIMIT_FIELD.to_owned()),
                "the job's wall limit is above the service's ceiling",
            ),
        }
    }

    /// The answer to a call that answered `status`: by its refusal when it carries one
    /// ([`Answer::of_refusal`]), else by its gRPC code and its message, which the service
    /// sets to the code of the error table. The message is never written to the answer;
    /// an `INVALID_ARGUMENT` message `field: <name>` gives the answer's `"field"`, and
    /// `OUT_OF_RANGE` with `field: fromOffset` is a 400 naming that field.
    #[must_use]
    pub fn of_status(status: &Status) -> Self {
        if let Some(refusal) = refusal_of(status) {
            return Self::of_refusal(refusal.code());
        }
        let message = status.message();
        match status.code() {
            Code::NotFound => match message {
                "unknown-job" => Self::coded(StatusCode::NOT_FOUND, "unknown-job"),
                "expired" => Self::coded(StatusCode::GONE, "expired"),
                _ => Self::internal(),
            },
            Code::FailedPrecondition => match message {
                "not-finished" => Self::coded(StatusCode::CONFLICT, "not-finished"),
                "no-log-sink" => Self::coded(StatusCode::CONFLICT, "no-log-sink"),
                "no-live-logs" => Self::coded(StatusCode::CONFLICT, "no-live-logs"),
                _ => Self::internal(),
            },
            Code::Internal => match message {
                "state-write-failed" => {
                    Self::coded(StatusCode::INTERNAL_SERVER_ERROR, "state-write-failed")
                }
                _ => Self::internal(),
            },
            Code::PermissionDenied => match message {
                "operator-only" => Self::coded(StatusCode::FORBIDDEN, "operator-only"),
                _ => Self::internal(),
            },
            Code::Unimplemented => Self::coded(StatusCode::NOT_IMPLEMENTED, "unimplemented"),
            Code::InvalidArgument => Self::invalid(
                message.strip_prefix("field: ").map(str::to_owned),
                "the service refused an argument of the request",
            ),
            Code::OutOfRange => match message {
                "field: fromOffset" => Self::refused(
                    "out-of-range",
                    Some("fromOffset".to_owned()),
                    "the offset is past the end of the stream",
                ),
                _ => Self::internal(),
            },
            Code::ResourceExhausted => match message {
                "too-many-follows" => {
                    Self::coded(StatusCode::TOO_MANY_REQUESTS, "too-many-follows")
                }
                _ => Self::internal(),
            },
            Code::Unavailable => match message {
                "log-store-unavailable" => {
                    Self::coded(StatusCode::SERVICE_UNAVAILABLE, "log-store-unavailable")
                }
                _ => Self::internal(),
            },
            Code::Ok
            | Code::Cancelled
            | Code::Unknown
            | Code::DeadlineExceeded
            | Code::AlreadyExists
            | Code::Aborted
            | Code::DataLoss
            | Code::Unauthenticated => Self::internal(),
        }
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The code the body's `"error"` holds.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The field the body's `"field"` names, if any.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// The body: `{"error": code}`, with `"field"` and `"reason"` when the answer has them.
    #[must_use]
    pub fn body(&self) -> Value {
        let mut body = Map::new();
        body.insert("error".to_owned(), Value::from(self.code));
        if let Some(field) = &self.field {
            body.insert("field".to_owned(), Value::from(field.as_str()));
        }
        if let Some(reason) = self.reason {
            body.insert("reason".to_owned(), Value::from(reason));
        }
        Value::Object(body)
    }
}

impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        let text = self.body().to_string();
        if self.allow.is_empty() {
            return (self.status, [(header::CONTENT_TYPE, JSON)], text).into_response();
        }
        let allow: Vec<&str> = self.allow.iter().map(|verb| verb.as_str()).collect();
        let allow = allow.join(", ");
        (
            self.status,
            [
                (header::CONTENT_TYPE, JSON),
                (header::ALLOW, allow.as_str()),
            ],
            text,
        )
            .into_response()
    }
}
