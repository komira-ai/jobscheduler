//! The agent's own declarations of the mount providers' messages (`js_agent::provider::wire`)
//! against the messages `js-proto` generates from `provider.proto`: the same bytes each way,
//! the same paths and the same mode numbers. The agent does not link `js-proto`, so this is
//! what holds its copy to the protocol.

use std::fmt::Debug;

use js_agent::provider::wire;
use js_proto::provider::v1 as pb;
use js_proto::provider::v1::mount_provider_server::SERVICE_NAME;
use prost::Message;

/// `ours` and `theirs` encode to the same bytes, and each decodes from the other's.
fn same<A, B>(name: &str, ours: &A, theirs: &B)
where
    A: Message + Default + PartialEq + Debug,
    B: Message + Default + PartialEq + Debug,
{
    let bytes = ours.encode_to_vec();
    assert_eq!(
        bytes,
        theirs.encode_to_vec(),
        "{name} encodes as the proto's"
    );
    assert_eq!(
        &B::decode(bytes.as_slice()).unwrap(),
        theirs,
        "{name} read by the proto's"
    );
    let back = A::decode(theirs.encode_to_vec().as_slice()).unwrap();
    assert_eq!(&back, ours, "{name} read from the proto's");
}

fn source() -> (wire::Source, pb::Source) {
    let ours = wire::Source {
        agent: "host-1".to_owned(),
        run: "js.job-1".to_owned(),
        index: 2,
        attempt: 3,
    };
    let theirs = pb::Source {
        agent: "host-1".to_owned(),
        run: "js.job-1".to_owned(),
        index: 2,
        attempt: 3,
    };
    (ours, theirs)
}

/// Asserts that every request and response the agent writes or reads has the bytes of the
/// proto's message of the same name, filled (an attempt and a kept attempt included) and
/// empty, and that each reads the other's.
/// Catches a field number, type or name of the agent's copy that drifts from
/// `provider.proto`, which a provider built from the proto would read as another field.
#[test]
fn the_agents_messages_are_the_protos_to_the_byte() {
    let (ours, theirs) = source();
    same("Source", &ours, &theirs);
    let modes = [
        (wire::MODE_READ_ONLY, pb::MountMode::ReadOnly),
        (wire::MODE_READ_WRITE, pb::MountMode::ReadWrite),
        (wire::MODE_COPY_ON_WRITE, pb::MountMode::CopyOnWrite),
    ];
    for (mode, protocol) in modes {
        assert_eq!(mode, protocol as i32);
        let ours = wire::PrepareRequest {
            source: Some(source().0),
            r#ref: "toolchains/clang-18".to_owned(),
            mode,
        };
        let theirs = pb::PrepareRequest {
            source: Some(source().1),
            r#ref: "toolchains/clang-18".to_owned(),
            mode: protocol as i32,
        };
        same("PrepareRequest", &ours, &theirs);
    }
    same(
        "PrepareResponse",
        &wire::PrepareResponse {},
        &pb::PrepareResponse {},
    );
    same(
        "MountRequest",
        &wire::MountRequest {
            source: Some(source().0),
        },
        &pb::MountRequest {
            source: Some(source().1),
        },
    );
    same(
        "MountResponse",
        &wire::MountResponse {
            path: "/srv/tier/job-1/2".to_owned(),
        },
        &pb::MountResponse {
            path: "/srv/tier/job-1/2".to_owned(),
        },
    );
    same(
        "ReleaseRequest",
        &wire::ReleaseRequest {
            source: Some(source().0),
        },
        &pb::ReleaseRequest {
            source: Some(source().1),
        },
    );
    same(
        "ReleaseResponse",
        &wire::ReleaseResponse {},
        &pb::ReleaseResponse {},
    );
    let keep = vec!["js.job-1".to_owned(), "js.job-2".to_owned()];
    let kept = (
        wire::KeptAttempt {
            run: "js.job-1".to_owned(),
            attempt: 3,
        },
        pb::KeptAttempt {
            run: "js.job-1".to_owned(),
            attempt: 3,
        },
    );
    same("KeptAttempt", &kept.0, &kept.1);
    same(
        "SweepRequest",
        &wire::SweepRequest {
            agent: "host-1".to_owned(),
            keep: keep.clone(),
            keep_attempts: vec![kept.0],
        },
        &pb::SweepRequest {
            agent: "host-1".to_owned(),
            keep,
            keep_attempts: vec![kept.1],
        },
    );
    same(
        "SweepResponse",
        &wire::SweepResponse { released: 5 },
        &pb::SweepResponse { released: 5 },
    );
    same(
        "an empty MountRequest",
        &wire::MountRequest::default(),
        &pb::MountRequest::default(),
    );
}

/// Asserts that the paths the agent calls are the service's methods as the proto names them.
/// Catches a call sent to a path a provider built from the proto does not serve, which it
/// would answer `UNIMPLEMENTED`.
#[test]
fn the_agent_calls_the_protos_paths() {
    let paths = [
        (wire::PREPARE, "Prepare"),
        (wire::MOUNT, "Mount"),
        (wire::RELEASE, "Release"),
        (wire::SWEEP, "Sweep"),
    ];
    for (path, method) in paths {
        assert_eq!(path, format!("/{SERVICE_NAME}/{method}"));
    }
}
