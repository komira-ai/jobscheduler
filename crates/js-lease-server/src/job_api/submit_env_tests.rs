//! `SubmitJob` of a job with an environment (`spec.env`), over both transports through the
//! mounted handler: an environment a start could not carry is refused as the wire refuses it,
//! and one it could carry is recorded as given.

use super::*;

/// The job `id` with the one variable `name` set to `value`.
fn with_env(id: &str, name: &str, value: &str) -> v1::JobSpec {
    v1::JobSpec {
        env: BTreeMap::from([(name.to_owned(), value.to_owned())]),
        ..wire(id)
    }
}

/// Asserts that a job whose environment names a variable the agent owns or that steers the
/// loader, the agent or a shell (`HOME`, `TMPDIR`, `LD_PRELOAD`, `JS_X`, `DYLD_` and `IFS`),
/// or breaks another rule of the wire's environment check (a name that starts with a digit, a
/// value with a NUL), is refused over both transports `INVALID_ARGUMENT` naming `spec.env`
/// with the wire's code `invalid-env` (400 with that field over JSON), never echoing the name
/// or the value, and that nothing is recorded. Catches a job admitted with an environment its
/// start can never carry, which would wait placed and never be sent to its host.
#[tokio::test]
async fn an_environment_the_wire_refuses_is_refused_naming_spec_env() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let refused = [
        ("HOME", "/scratch"),
        ("TMPDIR", "/scratch"),
        ("LD_PRELOAD", "/scratch/hook.so"),
        ("JS_X", "1"),
        ("DYLD_INSERT_LIBRARIES", "/scratch/hook.dylib"),
        ("IFS", ","),
        ("1BAD", "1"),
        ("MODE", "fa\0st"),
    ];
    for (name, value) in refused {
        let spec = with_env("env-bad", name, value);
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{name}");
        assert_eq!(status.message(), "field: spec.env: invalid-env", "{name}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body["field"].clone()),
            (StatusCode::BAD_REQUEST, json!("spec.env: invalid-env")),
            "{name}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}

/// Asserts that names beside the reserved ones (`LD` without the `_` of the prefix, `home` in
/// lower case, `MY_HOME`, which only contains a reserved name) and an empty environment are
/// accepted over both transports and recorded as given. Catches a check that refuses by a
/// looser match than the wire's (case-folded, a substring, a prefix without its `_`).
#[tokio::test]
async fn an_environment_the_wire_carries_is_accepted_and_recorded_as_given() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let cases = [
        (Via::Grpc, "env-ld", "LD", "1"),
        (Via::Json, "env-home", "home", "/x"),
        (Via::Grpc, "env-mine", "MY_HOME", "/x"),
    ];
    for (via, named, name, value) in cases {
        writes
            .submit(via, &with_env(named, name, value))
            .await
            .unwrap();
        let stored = writes.jobs.get_job(&id(named)).unwrap().unwrap();
        let wanted = BTreeMap::from([(name.to_owned(), value.to_owned())]);
        assert_eq!(stored.spec.env, wanted, "{via:?} {name}");
    }
    writes.submit(Via::Json, &wire("env-none")).await.unwrap();
    let stored = writes.jobs.get_job(&id("env-none")).unwrap().unwrap();
    assert!(stored.spec.env.is_empty());
    assert_eq!(rows(&writes.jobs), 4);
}
