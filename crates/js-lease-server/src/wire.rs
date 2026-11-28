//! The error body the routes answer with.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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

    /// Reads the JSON an error renders as.
    async fn rendered(err: ApiError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    /// Asserts that a bad request renders as the 400 `{"error": "bad-request", "reason": ...}`
    /// with its reason exactly as given. Catches a reason that is dropped or rendered under
    /// another key.
    #[tokio::test]
    async fn a_bad_request_renders_its_reason() {
        let (status, json) = rendered(ApiError::bad_request("to is before from")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            json,
            serde_json::json!({"error": "bad-request", "reason": "to is before from"})
        );
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
