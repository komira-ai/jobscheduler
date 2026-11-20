//! `SubmitJob` of a job that names an Xcode build (`Constraints.xcode`), over both
//! transports through the mounted handler: what the answer shows, what the tables record, and
//! how a malformed build is refused.

use super::*;

/// The job `id` with the Xcode build `build`.
fn with_xcode(id: &str, build: &str) -> v1::JobSpec {
    v1::JobSpec {
        constraints: Some(v1::Constraints {
            xcode: build.to_owned(),
            ..v1::Constraints::default()
        }),
        ..wire(id)
    }
}

/// Asserts that a job submitted with `constraints.xcode` over gRPC and over JSON is answered
/// with the build in its constraints and recorded with the capability `xcode` of that build
/// (what placement and the start read), one row each. Catches a build the handler drops on its
/// way into the record or out of it, and one recorded under another key or not at all.
#[tokio::test]
async fn a_submitted_xcode_build_is_answered_and_recorded_as_the_xcode_capability() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    for (via, named) in [(Via::Grpc, "xcode-g"), (Via::Json, "xcode-j")] {
        let spec = with_xcode(named, "16C5032a");
        let made = writes.submit(via, &spec).await.unwrap();
        let shown = made.spec.unwrap().constraints.unwrap();
        assert_eq!(shown.xcode, "16C5032a", "{via:?}");
        let stored = writes.jobs.get_job(&id(named)).unwrap().unwrap();
        let capabilities = stored.spec.constraints.capabilities;
        assert_eq!(
            capabilities,
            BTreeMap::from([("xcode".to_owned(), "16C5032a".to_owned())]),
            "{via:?}"
        );
    }
    assert_eq!(rows(&writes.jobs), 2);
}

/// Asserts that a job whose `constraints.xcode` is not a build id (a comma, as a joined list
/// would have, and 33 bytes) is refused over both transports naming
/// `spec.constraints.xcode`, and that nothing is recorded. Catches a malformed build that
/// reaches the tables, where it could never match a host's builds.
#[tokio::test]
async fn a_malformed_xcode_build_is_refused_by_the_handler_naming_its_field() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let long = "A".repeat(33);
    for build in ["16C5032a,16E140", long.as_str()] {
        let spec = with_xcode("xcode-bad", build);
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{build}");
        assert_eq!(status.message(), "field: spec.constraints.xcode", "{build}");
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        assert_eq!(
            (http, body["field"].clone()),
            (StatusCode::BAD_REQUEST, json!("spec.constraints.xcode")),
            "{build}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}
