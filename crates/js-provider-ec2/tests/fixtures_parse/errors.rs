//! The error answers of the API and how each is classed.

use super::*;
use crate::launch::run_request;

/// An error fixture, the status it is served with and the answers it comes to.
struct Failure {
    file: &'static str,
    status: u16,
    error: ApiError,
    launch: Acquired,
}

/// The error fixtures a launch can meet.
fn launch_failures() -> Vec<Failure> {
    let refusal = refused_launch;
    let unknown = || Acquired::Unknown;
    vec![
        Failure {
            file: "error_insufficient_instance_capacity.xml",
            status: 500,
            error: refused("InsufficientInstanceCapacity"),
            launch: refusal(RefusalCode::Capacity, "InsufficientInstanceCapacity"),
        },
        Failure {
            file: "error_spot_max_price_too_low.xml",
            status: 400,
            error: refused("SpotMaxPriceTooLow"),
            launch: refusal(RefusalCode::Capacity, "SpotMaxPriceTooLow"),
        },
        Failure {
            file: "error_instance_limit_exceeded.xml",
            status: 400,
            error: refused("InstanceLimitExceeded"),
            launch: refusal(RefusalCode::Quota, "InstanceLimitExceeded"),
        },
        Failure {
            file: "error_vcpu_limit_exceeded.xml",
            status: 400,
            error: refused("VcpuLimitExceeded"),
            launch: refusal(RefusalCode::Quota, "VcpuLimitExceeded"),
        },
        Failure {
            file: "error_max_spot_instance_count_exceeded.xml",
            status: 400,
            error: refused("MaxSpotInstanceCountExceeded"),
            launch: refusal(RefusalCode::Quota, "MaxSpotInstanceCountExceeded"),
        },
        Failure {
            file: "error_invalid_ami_id_not_found.xml",
            status: 400,
            error: refused("InvalidAMIID.NotFound"),
            launch: refusal(RefusalCode::Shape, "InvalidAMIID.NotFound"),
        },
        Failure {
            file: "error_invalid_parameter_value.xml",
            status: 400,
            error: refused("InvalidParameterValue"),
            launch: refusal(RefusalCode::Shape, "InvalidParameterValue"),
        },
        Failure {
            file: "error_unsupported.xml",
            status: 400,
            error: refused("Unsupported"),
            launch: refusal(RefusalCode::Shape, "Unsupported"),
        },
        Failure {
            file: "error_insufficient_free_addresses_in_subnet.xml",
            status: 400,
            error: refused("InsufficientFreeAddressesInSubnet"),
            launch: refusal(RefusalCode::Shape, "InsufficientFreeAddressesInSubnet"),
        },
        Failure {
            file: "error_unauthorized_operation.xml",
            status: 403,
            error: denied("UnauthorizedOperation"),
            launch: refusal(RefusalCode::Denied, "UnauthorizedOperation"),
        },
        Failure {
            file: "error_auth_failure.xml",
            status: 401,
            error: denied("AuthFailure"),
            launch: refusal(RefusalCode::Denied, "AuthFailure"),
        },
        Failure {
            file: "error_opt_in_required.xml",
            status: 401,
            error: denied("OptInRequired"),
            launch: refusal(RefusalCode::Denied, "OptInRequired"),
        },
        Failure {
            file: "error_blocked.xml",
            status: 400,
            error: denied("Blocked"),
            launch: unknown(),
        },
        Failure {
            file: "error_request_limit_exceeded.xml",
            status: 503,
            error: throttled("RequestLimitExceeded"),
            launch: unknown(),
        },
        Failure {
            file: "error_internal_error.xml",
            status: 500,
            error: refused("InternalError"),
            launch: unknown(),
        },
        Failure {
            file: "error_service_unavailable.xml",
            status: 503,
            error: refused("ServiceUnavailable"),
            launch: unknown(),
        },
        Failure {
            file: "error_unavailable.xml",
            status: 503,
            error: refused("Unavailable"),
            launch: unknown(),
        },
    ]
}

/// Asserts that each error answer a launch can meet is the exact error its code names
/// (throttled, denied or refused, with the code), and that the launch answer is the
/// documented one: a capacity, quota, shape or denied refusal carrying the code, and
/// unknown for a throttle, a transient server error and a code the table does not list.
/// Catches a code classed into the wrong error, a launch mapped to the wrong constant,
/// and a transient failure taken for a refusal.
#[test]
fn every_launch_error_fixture_is_its_error_and_its_launch_answer() {
    for failure in launch_failures() {
        let (api, script) = client(vec![failing(failure.status, failure.file)]);
        let error = api
            .run_instances(&run_request(LaunchPurchase::OnDemand))
            .unwrap_err();
        assert_eq!(error, failure.error, "{}", failure.file);
        assert_eq!(launch_answer(&error), failure.launch, "{}", failure.file);
        assert_eq!(script.sent().len(), 1, "{}", failure.file);
    }
}

/// Asserts that the not-found answer is `Refused` with the code `InvalidInstanceID.NotFound`
/// and is a not-found, from a read by id and from a terminate; and that a launch given the
/// same answer is `Unknown`, since the table lists no refusal for it. Catches a not-found
/// that is not recognised and one that refuses a launch.
#[test]
fn the_not_found_fixture_is_a_not_found_on_a_read_and_a_terminate() {
    let id = InstanceId::new("i-00000000000000001").unwrap();
    let (api, _) = client(vec![
        failing(400, "error_invalid_instance_id_not_found.xml"),
        failing(400, "error_invalid_instance_id_not_found.xml"),
        failing(400, "error_invalid_instance_id_not_found.xml"),
    ]);
    let read = api
        .describe_instances(&DescribeFilter::Ids(vec![id.clone()]))
        .unwrap_err();
    let terminate = api.terminate_instances(&[id]).unwrap_err();
    let launch = api
        .run_instances(&run_request(LaunchPurchase::OnDemand))
        .unwrap_err();
    for error in [&read, &terminate, &launch] {
        assert_eq!(error, &refused("InvalidInstanceID.NotFound"));
        assert!(error.is_not_found());
    }
    assert_eq!(launch_answer(&launch), Acquired::Unknown);
}

/// Asserts that the other EC2 calls class the error fixtures by their code too, whichever
/// call fails: instance types throttled, zone offerings denied, spot prices refused by a
/// transient server error, Capacity Blocks denied, a read by tags refused by an invalid
/// parameter. Catches a call that reads an error answer on a path of its own.
#[test]
fn the_read_calls_class_error_fixtures_by_their_code() {
    let (api, _) = client(vec![
        failing(503, "error_request_limit_exceeded.xml"),
        failing(403, "error_unauthorized_operation.xml"),
        failing(500, "error_internal_error.xml"),
        failing(401, "error_auth_failure.xml"),
        failing(400, "error_invalid_parameter_value.xml"),
    ]);
    assert_eq!(
        api.instance_types(&region()),
        Err(throttled("RequestLimitExceeded"))
    );
    assert_eq!(
        api.zone_offerings(&region()),
        Err(denied("UnauthorizedOperation"))
    );
    assert_eq!(api.spot_prices(&region()), Err(refused("InternalError")));
    assert_eq!(
        api.capacity_block_offerings(&region(), 24),
        Err(denied("AuthFailure"))
    );
    assert_eq!(
        api.describe_instances(&DescribeFilter::Tags(Vec::new())),
        Err(refused("InvalidParameterValue"))
    );
}

/// Asserts the JSON error fixtures: Pricing throttling is `Throttled`, its access denial is
/// `Denied` and its internal error is `Refused`; Service Quotas' too-many-requests is
/// `Throttled`, its access denial `Denied`, and a namespaced `NoSuchResourceException` (the
/// account has no entry for the quota) is no quota rather than an error. Catches the part
/// of `__type` before `#` kept in the code and a missing quota taken for a failure.
#[test]
fn json_error_fixtures_are_classed_by_their_type() {
    let (api, _) = client(vec![
        failing(400, "error_pricing_throttling_exception.json"),
        failing(400, "error_pricing_access_denied_exception.json"),
        failing(500, "error_pricing_internal_error_exception.json"),
    ]);
    assert_eq!(
        api.on_demand_prices(&region()),
        Err(throttled("ThrottlingException"))
    );
    assert_eq!(
        api.on_demand_prices(&region()),
        Err(denied("AccessDeniedException"))
    );
    assert_eq!(
        api.on_demand_prices(&region()),
        Err(refused("InternalErrorException"))
    );
    let (api, _) = client(vec![
        failing(400, "error_quotas_too_many_requests_exception.json"),
        failing(400, "error_quotas_access_denied_exception.json"),
        failing(400, "error_quotas_no_such_resource_exception.json"),
    ]);
    let quota =
        |api: &HttpEc2| api.vcpu_quota(&region(), QuotaFamily::Standard, PurchaseKind::OnDemand);
    assert_eq!(quota(&api), Err(throttled("TooManyRequestsException")));
    assert_eq!(quota(&api), Err(denied("AccessDeniedException")));
    assert_eq!(quota(&api), Ok(None));
}
