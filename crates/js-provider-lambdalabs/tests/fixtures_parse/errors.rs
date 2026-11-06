//! Error answers: the exact `ApiError`, and the launch answer the provider gives for each.

use crate::*;

// ------------------------------------------------------------------------- errors

/// Every error fixture, with the status the test serves it under, the code the client
/// reads and the request id the file carries. The status is not part of a file.
fn api_errors() -> Vec<(&'static str, u16, ErrorCode, &'static str)> {
    vec![
        (
            "error_invalid_api_key.json",
            401,
            ErrorCode::InvalidApiKey,
            "req-invalid_api_key",
        ),
        (
            "error_account_inactive.json",
            403,
            ErrorCode::AccountInactive,
            "req-account_inactive",
        ),
        (
            "error_forbidden.json",
            403,
            ErrorCode::Forbidden,
            "req-forbidden",
        ),
        (
            "error_invalid_address.json",
            400,
            ErrorCode::InvalidAddress,
            "req-invalid_address",
        ),
        (
            "error_object_does_not_exist.json",
            404,
            ErrorCode::ObjectDoesNotExist,
            "req-object_does_not_exist",
        ),
        (
            "error_not_found.json",
            404,
            ErrorCode::NotFound,
            "req-not_found",
        ),
        (
            "error_invalid_parameters.json",
            400,
            ErrorCode::InvalidParameters,
            "req-invalid_parameters",
        ),
        (
            "error_file_system_in_wrong_region.json",
            400,
            ErrorCode::FileSystemInWrongRegion,
            "req-file_system_in_wrong_region",
        ),
        (
            "error_insufficient_capacity.json",
            400,
            ErrorCode::InsufficientCapacity,
            "req-capacity",
        ),
        (
            "error_quota_exceeded.json",
            400,
            ErrorCode::QuotaExceeded,
            "req-quota",
        ),
        (
            "error_internal.json",
            500,
            ErrorCode::InternalError,
            "req-internal",
        ),
        (
            "error_provider_unavailable.json",
            502,
            ErrorCode::Provider("provider/internal-unavailable".to_owned()),
            "req-provider_unavailable",
        ),
    ]
}

#[test]
fn every_error_fixture_is_read_as_its_exact_variant_status_code_and_request_id() {
    for (file, status, code, request_id) in api_errors() {
        let server = Server::plain();
        server.on(LIST, Reply::json(status, &fixture(file)));
        assert_eq!(
            connect(&server).list_instances().unwrap_err(),
            ApiError::Api {
                status,
                code,
                request_id: Some(request_id.to_owned()),
            },
            "{file}"
        );
    }
}

#[test]
fn the_same_error_body_gives_the_same_error_on_each_of_the_nine_calls() {
    let routes = [
        TYPES.to_owned(),
        LIST.to_owned(),
        one(FIRST),
        LAUNCH.to_owned(),
        TERMINATE.to_owned(),
        RULESETS.to_owned(),
        MAKE_RULESET.to_owned(),
        KEYS.to_owned(),
        MAKE_KEY.to_owned(),
    ];
    let server = Server::plain();
    for route in &routes {
        server.on(
            route,
            Reply::json(400, &fixture("error_insufficient_capacity.json")),
        );
    }
    let api = connect(&server);
    let errors = [
        api.instance_types().map(|_| ()).unwrap_err(),
        api.list_instances().map(|_| ()).unwrap_err(),
        api.get_instance(&id(FIRST)).map(|_| ()).unwrap_err(),
        api.launch(&request()).map(|_| ()).unwrap_err(),
        api.terminate(&id(FIRST)).map(|_| ()).unwrap_err(),
        api.list_firewall_rulesets().map(|_| ()).unwrap_err(),
        api.create_firewall_ruleset("region-a", "n")
            .map(|_| ())
            .unwrap_err(),
        api.list_ssh_keys().map(|_| ()).unwrap_err(),
        api.create_ssh_key("n").map(|_| ()).unwrap_err(),
    ];
    let expected = ApiError::Api {
        status: 400,
        code: ErrorCode::InsufficientCapacity,
        request_id: Some("req-capacity".to_owned()),
    };
    for error in errors {
        assert_eq!(error, expected);
    }
    assert_eq!(server.seen().len(), 9);
}

#[test]
fn a_429_is_rate_limited_with_the_retry_after_the_header_carries() {
    let body = fixture("error_rate_limited.json");
    let server = Server::plain();
    server.on(
        LIST,
        Reply::json(429, &body).with_header("Retry-After", "30"),
    );
    server.on(LIST, Reply::json(429, &body));
    let api = connect(&server);
    let limited = |retry_after| ApiError::RateLimited {
        retry_after,
        request_id: Some("req-rate_limited".to_owned()),
    };
    assert_eq!(
        api.list_instances().unwrap_err(),
        limited(Some(Duration::from_secs(30)))
    );
    assert_eq!(api.list_instances().unwrap_err(), limited(None));
}

#[test]
fn only_the_documented_404_reads_as_an_absent_instance() {
    for (file, status, code, _) in api_errors() {
        let server = Server::plain();
        server.on(&one(FIRST), Reply::json(status, &fixture(file)));
        let err = connect(&server).get_instance(&id(FIRST)).unwrap_err();
        let absent = code == ErrorCode::ObjectDoesNotExist && status == 404;
        assert_eq!(err.is_absent(), absent, "{file}");
    }
}

/// Launches once through a provider on the real client, where the account holds its
/// no-inbound ruleset and no instance, and the launch is answered `launch`.
fn launched_into(launch: Reply) -> (Acquired, usize) {
    let server = Server::plain();
    server.on(
        RULESETS,
        Reply::json(200, &fixture("firewall_rulesets.json")),
    );
    server.on(LIST, Reply::json(200, &fixture("instances_empty.json")));
    server.on(LAUNCH, launch);
    let provider = provider(&server, &rows(&["gpu_1x_h100_sxm5"]), 2);
    let answer = provider.acquire(&ask());
    let posts = server.seen().iter().filter(|s| s.method == "POST").count();
    (answer, posts)
}

fn ask() -> AcquireRequest {
    let acquire = acquire("acq-1");
    AcquireRequest {
        name: HostName::derive(&scheduler(), &acquire),
        acquire,
        pool: "pool-a".to_owned(),
        lifetime: None,
        offer: Offer {
            id: "gpu_1x_h100_sxm5".to_owned(),
            zone: Some("region-a".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 100 << 30,
            accelerator: Some(accelerator_of(
                &Model::parse("nvidia-h100-80gb-hbm3").unwrap(),
                1,
                None,
            )),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
        },
        labels: Vec::new(),
        payload: BootPayload::new(String::new()),
    }
}

fn refused(code: RefusalCode, api_code: &str) -> Acquired {
    Acquired::Refused(Refusal {
        code,
        reason: Reason::Api(api_code.to_owned()),
    })
}

#[test]
fn each_error_fixture_becomes_the_launch_answer_the_provider_gives_for_it() {
    use RefusalCode::{Capacity, Denied, Quota, Shape};
    let cases = [
        (
            "error_invalid_api_key.json",
            401,
            refused(Denied, "global/invalid-api-key"),
        ),
        (
            "error_account_inactive.json",
            403,
            refused(Denied, "global/account-inactive"),
        ),
        (
            "error_forbidden.json",
            403,
            refused(Denied, "global/forbidden"),
        ),
        (
            "error_invalid_address.json",
            400,
            refused(Denied, "global/invalid-address"),
        ),
        (
            "error_invalid_parameters.json",
            400,
            refused(Shape, "global/invalid-parameters"),
        ),
        (
            "error_object_does_not_exist.json",
            404,
            refused(Shape, "global/object-does-not-exist"),
        ),
        (
            "error_file_system_in_wrong_region.json",
            400,
            refused(
                Shape,
                "instance-operations/launch/file-system-in-wrong-region",
            ),
        ),
        (
            "error_insufficient_capacity.json",
            400,
            refused(Capacity, "instance-operations/launch/insufficient-capacity"),
        ),
        (
            "error_quota_exceeded.json",
            400,
            refused(Quota, "global/quota-exceeded"),
        ),
        // No definite answer: the launch may or may not have landed.
        ("error_not_found.json", 404, Acquired::Unknown),
        ("error_rate_limited.json", 429, Acquired::Unknown),
        ("error_internal.json", 500, Acquired::Unknown),
        ("error_provider_unavailable.json", 502, Acquired::Unknown),
        // A familiar code behind a server error may be a gateway speaking after the
        // launch landed.
        ("error_insufficient_capacity.json", 503, Acquired::Unknown),
        ("error_quota_exceeded.json", 500, Acquired::Unknown),
    ];
    for (file, status, want) in cases {
        let (answer, posts) = launched_into(Reply::json(status, &fixture(file)));
        assert_eq!(answer, want, "{status} {file}");
        assert_eq!(posts, 1, "{status} {file}: one launch, never a second try");
    }
}

#[test]
fn a_429_with_a_retry_after_is_still_an_unknown_launch() {
    let reply =
        Reply::json(429, &fixture("error_rate_limited.json")).with_header("Retry-After", "30");
    assert_eq!(launched_into(reply), (Acquired::Unknown, 1));
}

#[test]
fn a_launch_answer_fixture_is_accepted_with_the_instance_it_lists() {
    let (answer, posts) = launched_into(Reply::json(200, &fixture("launch.json")));
    let Acquired::Accepted { held, .. } = answer else {
        panic!("not accepted: {answer:?}");
    };
    assert_eq!(held.id.as_str(), LAUNCHED);
    assert_eq!(posts, 1);
}
