//! The small value types and the error text.

use std::time::Duration;

use js_provider_lambdalabs::{ApiError, ErrorCode, InstanceId, Status};

#[test]
fn an_instance_id_is_a_path_safe_token() {
    for good in ["a", "A1_-z", &"a".repeat(64)] {
        let id = InstanceId::new(good).unwrap();
        assert_eq!((id.as_str(), id.to_string().as_str()), (good, good));
    }
    for bad in ["", "a b", "a/b", "..", "a%2fb", "a?b", &"a".repeat(65)] {
        let err = InstanceId::new(bad).unwrap_err();
        assert!(err.to_string().contains("instance id"), "{bad:?}");
    }
}

#[test]
fn every_documented_status_has_a_variant_and_any_other_is_unknown() {
    let documented = [
        ("booting", Status::Booting),
        ("active", Status::Active),
        ("unhealthy", Status::Unhealthy),
        ("terminated", Status::Terminated),
        ("terminating", Status::Terminating),
        ("preempted", Status::Preempted),
    ];
    for (text, status) in documented {
        assert_eq!(Status::parse(text), status);
    }
    assert_eq!(
        Status::parse("Active"),
        Status::Unknown("Active".to_owned())
    );
    assert_eq!(
        Status::parse("a b\u{7}c"),
        Status::Unknown("abc".to_owned())
    );
    let Status::Unknown(long) = Status::parse(&"s".repeat(100)) else {
        panic!("not unknown");
    };
    assert_eq!(long.len(), 40);
}

#[test]
fn errors_read_as_sentences_without_lambdas_own_text() {
    let api = ApiError::Api {
        status: 403,
        code: ErrorCode::AccountInactive,
        request_id: Some("r1".to_owned()),
    };
    assert_eq!(
        api.to_string(),
        "the API answered 403 global/account-inactive"
    );
    assert!(!api.is_absent());
    let limited = ApiError::RateLimited {
        retry_after: Some(Duration::from_secs(3)),
        request_id: None,
    };
    assert!(!limited.is_absent());
    assert!(limited.to_string().contains("rate limiting"));
    assert!(
        ApiError::Transport("t".to_owned())
            .to_string()
            .contains("cannot talk")
    );
    assert!(
        ApiError::Malformed("m".to_owned())
            .to_string()
            .contains("cannot be read")
    );
    assert!(ApiError::Incomplete.to_string().contains("not complete"));
    for other in [
        ApiError::Incomplete,
        ApiError::Transport(String::new()),
        ApiError::Malformed(String::new()),
        ApiError::Api {
            status: 404,
            code: ErrorCode::NotFound,
            request_id: None,
        },
        ApiError::Api {
            status: 500,
            code: ErrorCode::ObjectDoesNotExist,
            request_id: None,
        },
    ] {
        assert!(!other.is_absent(), "{other:?}");
    }
}
