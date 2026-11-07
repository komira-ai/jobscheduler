//! The error body the routes answer with, and the parse of a JSON request body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::error::Category;

/// Parses a request body as `T`. The 400 says which kind of fault it is and where the
/// parser stopped; it never repeats the body or a value or field name in it, since the
/// parser's own message does.
pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| {
        let fault = match err.classify() {
            Category::Data => "the body is JSON, but not what this route takes",
            Category::Syntax | Category::Eof | Category::Io => "the body is not valid JSON",
        };
        ApiError::bad_request(format!(
            "{fault} (line {}, column {})",
            err.line(),
            err.column()
        ))
    })
}

/// An error answer: a status and `{"error": "<code>"}`, with a `reason` for a 400.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    reason: Option<String>,
}

impl ApiError {
    pub(crate) fn coded(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            reason: None,
        }
    }

    /// A 400 with a short reason.
    pub(crate) fn bad_request(reason: impl ToString) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad-request",
            reason: Some(reason.to_string()),
        }
    }

    /// The 404 of an unknown route.
    pub(crate) fn no_route() -> Self {
        Self::coded(StatusCode::NOT_FOUND, "not-found")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = match self.reason {
            None => serde_json::json!({ "error": self.code }),
            Some(reason) => serde_json::json!({ "error": self.code, "reason": reason }),
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// A body shape with one required field, to parse against.
    #[derive(Debug, Deserialize)]
    struct Shape {
        name: String,
    }

    /// Reads the JSON an error renders as.
    async fn rendered(err: ApiError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    /// Asserts that a body which is not JSON, one cut short, and one of another shape are
    /// each a 400 whose reason names the kind of fault and the position, and that a body
    /// of the right shape parses.
    #[tokio::test]
    async fn a_bad_body_is_a_400_naming_the_kind_of_fault_and_the_position() {
        let shape = parse::<Shape>(br#"{"name":"a"}"#).expect("parses");
        assert_eq!(shape.name, "a");
        let cases: [(&[u8], &str); 3] = [
            (b"nope", "the body is not valid JSON (line 1, column 2)"),
            (
                b"{\"name\":",
                "the body is not valid JSON (line 1, column 8)",
            ),
            (
                br#"{"name":3}"#,
                "the body is JSON, but not what this route takes (line 1, column 9)",
            ),
        ];
        for (body, reason) in cases {
            let err = parse::<Shape>(body).expect_err("refused");
            let (status, json) = rendered(err).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                json,
                serde_json::json!({"error": "bad-request", "reason": reason})
            );
        }
    }

    /// Asserts that a coded error renders as `{"error": code}` with its status, and an
    /// unknown route as the 404 `not-found`.
    #[tokio::test]
    async fn a_coded_error_renders_its_status_and_code_alone() {
        let (status, json) = rendered(ApiError::coded(StatusCode::CONFLICT, "busy")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json, serde_json::json!({"error": "busy"}));
        let (status, json) = rendered(ApiError::no_route()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json, serde_json::json!({"error": "not-found"}));
    }
}
