//! Deleting a host's device after release: idempotent, only that host's devices, bounded
//! retries, and typed failures.

use js_lease_server::network::{ApiFailure, FORGET_ATTEMPTS, NetworkError, NetworkMode};

use crate::fake::{Fake, Reply};
use crate::support::{clock, host, mode};

const DEVICES: &str = "GET /api/v2/tailnet/-/devices";
const DELETE_D1: &str = "DELETE /api/v2/device/d1";

/// Asserts that `forget` lists the devices, deletes the one whose hostname is the host's
/// and no other, with the access token; and that a second `forget` after the list no
/// longer holds the device succeeds and deletes nothing.
#[test]
fn forget_deletes_the_hosts_device_and_is_idempotent() {
    let host = host();
    let fake = Fake::tailscale(host.as_str());
    let (_, shared) = clock();
    let mode = mode(&fake, shared);

    mode.forget(&host).unwrap();
    assert_eq!(fake.count(DELETE_D1), 1);
    assert_eq!(fake.count("DELETE /api/v2/device/d9"), 0);
    let delete = fake
        .seen()
        .into_iter()
        .find(|s| s.method == "DELETE")
        .unwrap();
    assert!(
        delete
            .header("authorization")
            .unwrap()
            .starts_with("Bearer ")
    );

    fake.set(
        DEVICES,
        Reply::json(200, r#"{"devices":[{"id":"d9","hostname":"other"}]}"#),
    );
    mode.forget(&host).unwrap();
    assert_eq!(fake.count(DELETE_D1), 1);

    fake.set(DEVICES, Reply::json(200, r#"{"devices":[]}"#));
    mode.forget(&host).unwrap();
    assert_eq!(fake.count(DELETE_D1), 1);
}

/// Asserts that a device already deleted (a 404 on the delete) is not an error, and that
/// every device with the host's name is deleted, a device without a hostname is skipped.
#[test]
fn a_device_already_gone_is_not_an_error_and_every_match_is_deleted() {
    let host = host();
    let fake = Fake::tailscale(host.as_str());
    fake.set(
        DEVICES,
        Reply::json(
            200,
            &format!(
                r#"{{"devices":[{{"id":"d1","hostname":"{host}"}},{{"id":"d2","hostname":"{host}"}},{{"id":"d3"}}]}}"#
            ),
        ),
    );
    fake.set(DELETE_D1, Reply::json(404, "{}"));
    fake.set("DELETE /api/v2/device/d2", Reply::json(200, ""));
    let (_, shared) = clock();
    mode(&fake, shared).forget(&host).unwrap();
    assert_eq!(fake.count(DELETE_D1), 1);
    assert_eq!(fake.count("DELETE /api/v2/device/d2"), 1);
    assert_eq!(fake.count("DELETE /api/v2/device/d3"), 0);
}

/// Asserts that a failure that may pass is tried again up to the attempt limit and not
/// beyond, that one that cannot pass is tried once, and that an attempt after failures
/// that succeeds ends the tries.
#[test]
fn forget_retries_a_transient_failure_up_to_the_limit() {
    let host = host();

    // Two server errors, then success: three attempts, no error.
    let fake = Fake::tailscale(host.as_str());
    fake.set(DELETE_D1, Reply::json(503, "{}"));
    fake.on(DELETE_D1, Reply::json(503, "{}"));
    fake.on(DELETE_D1, Reply::json(200, ""));
    let (_, shared) = clock();
    mode(&fake, shared).forget(&host).unwrap();
    assert_eq!(fake.count(DELETE_D1), 3);

    // Always a server error: exactly the limit, then the error.
    let fake = Fake::tailscale(host.as_str());
    fake.set(DELETE_D1, Reply::json(503, "{}"));
    let (_, shared) = clock();
    let err = mode(&fake, shared).forget(&host).unwrap_err();
    assert_eq!(
        err,
        NetworkError::DeviceDelete(ApiFailure::Unavailable(503))
    );
    assert_eq!(fake.count(DELETE_D1), FORGET_ATTEMPTS);

    // A refusal is final.
    let fake = Fake::tailscale(host.as_str());
    fake.set(DELETE_D1, Reply::json(403, "{}"));
    let (_, shared) = clock();
    let err = mode(&fake, shared).forget(&host).unwrap_err();
    assert_eq!(err, NetworkError::DeviceDelete(ApiFailure::Rejected(403)));
    assert_eq!(fake.count(DELETE_D1), 1);
}

/// Asserts that the device list's failures are typed `DeviceDelete` (refused, server
/// error, malformed body, a device id that cannot stand in a path), that no delete is
/// sent after them, and that no token is a `Credentials` failure tried once.
#[test]
fn forget_failures_are_typed() {
    let host = host();
    let cases = [
        (Reply::json(403, "{}"), ApiFailure::Rejected(403)),
        (Reply::json(500, "{}"), ApiFailure::Unavailable(500)),
        (Reply::json(200, "nope"), ApiFailure::Malformed),
        (
            Reply::json(200, r#"{"devices":[{"hostname":"x"}]}"#),
            ApiFailure::Malformed,
        ),
        (
            Reply::json(
                200,
                &format!(r#"{{"devices":[{{"id":"../keys","hostname":"{host}"}}]}}"#),
            ),
            ApiFailure::Malformed,
        ),
    ];
    for (reply, failure) in cases {
        let fake = Fake::tailscale(host.as_str());
        fake.set(DEVICES, reply);
        let (_, shared) = clock();
        let err = mode(&fake, shared).forget(&host).unwrap_err();
        assert_eq!(
            err,
            NetworkError::DeviceDelete(failure.clone()),
            "{failure}"
        );
        assert_eq!(
            fake.seen().iter().filter(|s| s.method == "DELETE").count(),
            0
        );
    }

    let fake = Fake::tailscale(host.as_str());
    fake.set("POST /api/v2/oauth/token", Reply::json(401, "{}"));
    let (_, shared) = clock();
    let err = mode(&fake, shared).forget(&host).unwrap_err();
    assert_eq!(err, NetworkError::Credentials(ApiFailure::Rejected(401)));
    assert_eq!(fake.count("POST /api/v2/oauth/token"), 1);
}
