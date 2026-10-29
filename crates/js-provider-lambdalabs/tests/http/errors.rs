//! What the client makes of error answers and of success answers it cannot read.

use std::time::Duration;

use js_provider_lambdalabs::{ApiError, ErrorCode, LambdaApi};

use crate::reads::client;
use crate::support::{Reply, Server, fixture};

const LIST: &str = "GET /api/v1/instances";

fn list_error(reply: Reply) -> ApiError {
    let server = Server::plain();
    server.on(LIST, reply);
    client(&server, "key-one").list_instances().unwrap_err()
}

fn api_error(status: u16, file: &str) -> ApiError {
    list_error(Reply::json(status, &fixture(file)))
}

#[test]
fn documented_error_codes_map_to_their_names() {
    let cases = [
        (401, "error_invalid_api_key.json", ErrorCode::InvalidApiKey),
        (
            403,
            "error_account_inactive.json",
            ErrorCode::AccountInactive,
        ),
        (403, "error_forbidden.json", ErrorCode::Forbidden),
        (404, "error_not_found.json", ErrorCode::NotFound),
        (500, "error_internal.json", ErrorCode::InternalError),
    ];
    for (status, file, code) in cases {
        let id = file.trim_start_matches("error_").trim_end_matches(".json");
        assert_eq!(
            api_error(status, file),
            ApiError::Api {
                status,
                code,
                request_id: Some(format!("req-{id}")),
            },
            "{file}"
        );
    }
}

#[test]
fn a_provider_code_stays_a_provider_code() {
    let err = api_error(502, "error_provider_unavailable.json");
    assert_eq!(
        err,
        ApiError::Api {
            status: 502,
            code: ErrorCode::Provider("provider/internal-unavailable".to_owned()),
            request_id: Some("req-provider_unavailable".to_owned()),
        }
    );
}

#[test]
fn the_message_and_suggestion_are_not_kept() {
    for file in [
        "error_invalid_api_key.json",
        "error_provider_unavailable.json",
    ] {
        let err = api_error(400, file);
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("CANARY"), "{shown}");
    }
}

#[test]
fn a_code_that_is_not_known_is_kept_as_printable_text_and_shortened() {
    let long = "x".repeat(200);
    let body = format!(
        r#"{{"error": {{"code": "weird/\u0007code {long}", "request_id": "id\u0001{long}"}}}}"#
    );
    let ApiError::Api {
        code, request_id, ..
    } = list_error(Reply::json(418, &body))
    else {
        panic!("not an API error");
    };
    let ErrorCode::Other(text) = code else {
        panic!("not an unknown code");
    };
    assert!(text.starts_with("weird/code") && text.len() == 80, "{text}");
    assert_eq!(request_id.unwrap().len(), 80);
}

#[test]
fn a_body_with_no_code_is_unparsed() {
    for body in ["", "{}", r#"{"error": {"message": "no code"}}"#, "[1]"] {
        let err = list_error(Reply::json(503, body));
        assert_eq!(
            err,
            ApiError::Api {
                status: 503,
                code: ErrorCode::Unparsed,
                request_id: None,
            },
            "{body}"
        );
    }
}

#[test]
fn a_redirect_is_not_followed() {
    let server = Server::plain();
    server.on(
        LIST,
        Reply::json(302, "").with_header("location", "/api/v1/elsewhere"),
    );
    server.on("GET /api/v1/elsewhere", Reply::json(200, r#"{"data": []}"#));
    let err = client(&server, "key-one").list_instances().unwrap_err();
    assert!(matches!(
        err,
        ApiError::Api {
            status: 302,
            code: ErrorCode::Unparsed,
            ..
        }
    ));
    assert_eq!(server.seen().len(), 1);
}

#[test]
fn rate_limited_carries_whole_second_retry_after_and_nothing_else() {
    let body = fixture("error_rate_limited.json");
    let cases = [
        (Some("7"), Some(Duration::from_secs(7))),
        (Some(" 12 "), Some(Duration::from_secs(12))),
        (Some("Wed, 21 Oct 2026 07:28:00 GMT"), None),
        (Some("soon"), None),
        (None, None),
    ];
    for (header, expected) in cases {
        let reply = match header {
            Some(value) => Reply::json(429, &body).with_header("retry-after", value),
            None => Reply::json(429, &body),
        };
        assert_eq!(
            list_error(reply),
            ApiError::RateLimited {
                retry_after: expected,
                request_id: Some("req-rate_limited".to_owned()),
            },
            "{header:?}"
        );
    }
}

#[test]
fn a_429_without_a_readable_body_is_still_rate_limited() {
    assert_eq!(
        list_error(Reply::json(429, "slow down")),
        ApiError::RateLimited {
            retry_after: None,
            request_id: None
        }
    );
}

fn malformed(body: &str) -> String {
    match list_error(Reply::json(200, body)) {
        ApiError::Malformed(why) => why,
        other => panic!("not malformed: {other:?}"),
    }
}

#[test]
fn a_success_that_is_not_the_documented_shape_is_malformed() {
    malformed("");
    malformed("not json");
    malformed("null");
    malformed(r#"{"data": {}}"#);
    malformed(r#"{"data": [{"id": "x"}]}"#);
    malformed(r#"{"nothing": []}"#);
}

#[test]
fn the_malformed_message_quotes_no_part_of_the_answer() {
    let body = format!(r#"{{"data": "{}"}}"#, "y".repeat(5000));
    let why = malformed(&body);
    assert!(
        why.starts_with("unexpected answer shape (line 1, column "),
        "{why}"
    );
    assert!(!why.contains('y'), "{why}");
}

#[test]
fn an_instance_id_that_could_alter_a_path_is_malformed() {
    for bad in ["../x", "a b", "", "a/b", "é", &"a".repeat(65)] {
        let body = format!(
            r#"{{"data": [{{"id": "{bad}", "status": "active", "region": {{"name": "r"}}, "instance_type": {{"name": "t"}}}}]}}"#
        );
        assert!(malformed(&body).contains("id"), "{bad}");
    }
}

#[test]
fn a_type_with_a_negative_price_is_malformed() {
    let server = Server::plain();
    server.on(
        "GET /api/v1/instance-types",
        Reply::json(
            200,
            r#"{"data": {"t": {"instance_type": {"name": "t", "price_cents_per_hour": -1,
                "specs": {"vcpus": 1, "memory_gib": 1, "storage_gib": 1, "gpus": 1}}}}}"#,
        ),
    );
    let err = client(&server, "key-one").instance_types().unwrap_err();
    assert!(matches!(err, ApiError::Malformed(_)), "{err:?}");
}

#[test]
fn the_error_codes_spell_themselves_as_lambda_does() {
    let all = [
        "global/invalid-api-key",
        "global/forbidden",
        "global/account-inactive",
        "global/invalid-address",
        "global/object-does-not-exist",
        "global/not-found",
        "global/rate-limited",
        "global/internal-error",
        "global/invalid-parameters",
        "instance-operations/launch/insufficient-capacity",
        "global/quota-exceeded",
        "instance-operations/launch/file-system-in-wrong-region",
        "provider/internal-unavailable",
        "something/else",
    ];
    for text in all {
        let code = ErrorCode::parse(text);
        assert_eq!(code.as_str(), text);
        assert_eq!(code.to_string(), text);
    }
    assert_eq!(
        ErrorCode::parse("global/invalid-address"),
        ErrorCode::InvalidAddress
    );
    assert_eq!(
        ErrorCode::parse("global/invalid-parameters"),
        ErrorCode::InvalidParameters
    );
    assert_eq!(
        ErrorCode::parse("instance-operations/launch/insufficient-capacity"),
        ErrorCode::InsufficientCapacity
    );
    assert_eq!(
        ErrorCode::parse("global/quota-exceeded"),
        ErrorCode::QuotaExceeded
    );
    assert_eq!(
        ErrorCode::parse("instance-operations/launch/file-system-in-wrong-region"),
        ErrorCode::FileSystemInWrongRegion
    );
    assert_eq!(
        ErrorCode::parse("global/rate-limited"),
        ErrorCode::RateLimited
    );
    assert_eq!(ErrorCode::Unparsed.as_str(), "unparsed");
}
