//! `SubmitJob` of a job whose layers or mounts come from a node-local provider
//! (`docs/design/mount-providers.md`), over both transports: what is refused and how it is
//! named, what is answered, and what admission asks of the pool's hosts.

use tonic_prost::prost::Message;

use super::*;

/// The provider source `name` with the ref `reference`.
fn source(name: &str, reference: &str) -> v1::ProviderSource {
    v1::ProviderSource {
        provider: name.to_owned(),
        r#ref: reference.to_owned(),
    }
}

/// A layer from the provider `name`, asked for `reference`.
fn layer(name: &str, reference: &str) -> v1::Layer {
    v1::Layer {
        source: Some(v1::layer::Source::Provider(source(name, reference))),
    }
}

/// A mount at `target` from the provider `name`, asked for `reference`.
fn mount(name: &str, reference: &str, target: &str) -> v1::Mount {
    v1::Mount {
        source: Some(v1::mount::Source::Provider(source(name, reference))),
        target: target.to_owned(),
        ..v1::Mount::default()
    }
}

/// A ref that no refusal may repeat.
const SECRET: &str = "ref-that-must-not-leak";

/// A job `id` with one layer from `tierstore` and no mount.
fn layered(id: &str) -> v1::JobSpec {
    v1::JobSpec {
        layers: vec![layer("tierstore", "trees/0123")],
        ..wire(id)
    }
}

/// A job `id` with a layer from `tierstore` and a mount from `cache`.
fn two_providers(id: &str) -> v1::JobSpec {
    v1::JobSpec {
        mounts: vec![mount("cache", "inputs/a", "/cache")],
        ..layered(id)
    }
}

/// `number` as the field of a message holding `body`, length-delimited, as bytes: a field of
/// a source that a release newer than this server added.
fn unknown_field(number: u8, body: &v1::ProviderSource) -> Vec<u8> {
    let body = body.encode_to_vec();
    let mut bytes = vec![(number << 3) | 2, u8::try_from(body.len()).unwrap()];
    bytes.extend(body);
    bytes
}

/// Asserts that a layer and a mount whose source is a member of the `oneof` that this server
/// does not know (the bytes carry it under a field number this server's message lacks, as a
/// server of an earlier release reads a provider source) decode with no source and are refused
/// by name, `field: spec.layers[0]: no source this server knows` and the same for the mount,
/// over both transports, with nothing recorded. Catches a source this server cannot read taken
/// as a host path of nothing, and a refusal that names the list and not the entry.
#[tokio::test]
async fn a_provider_source_is_refused_by_an_earlier_server_by_name() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let later = unknown_field(9, &source("tierstore", "trees/0123"));
    let layer = v1::Layer::decode(later.as_slice()).unwrap();
    let mount = v1::Mount::decode(later.as_slice()).unwrap();
    assert_eq!((layer.source.clone(), mount.source.clone()), (None, None));
    let cases = [
        (
            "field: spec.layers[0]: no source this server knows",
            v1::JobSpec {
                layers: vec![layer],
                ..wire("job-e")
            },
        ),
        (
            "field: spec.mounts[0]: no source this server knows",
            v1::JobSpec {
                mounts: vec![v1::Mount {
                    target: "/in".to_owned(),
                    ..mount
                }],
                ..wire("job-e")
            },
        ),
    ];
    for (text, spec) in cases {
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{text}");
        assert_eq!(status.message(), text);
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        let field = text.strip_prefix("field: ").unwrap();
        assert_eq!(
            (http, body["field"].clone()),
            (StatusCode::BAD_REQUEST, json!(field)),
            "{text}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}

/// Asserts the refusal text of a provider source with a name or a ref this server refuses,
/// over gRPC (the whole message) and JSON (the `field`): the name of the field as the gateway
/// spells it, `provider.provider` or `provider.ref` below the entry, for a layer and for a
/// mount, and the message equal to that text alone, so neither the name nor the ref is
/// repeated. Catches a refusal that names the entry and not its field, one that echoes a
/// value of the request, and a mount that is not held to the layer's rules.
#[tokio::test]
async fn a_refused_provider_source_names_its_field_and_never_repeats_a_value() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let cases: Vec<(&str, Change)> = vec![
        (
            "field: spec.layers[0].provider.provider",
            Box::new(|spec| spec.layers = vec![layer("Not A Name", SECRET)]),
        ),
        (
            "field: spec.layers[1].provider.ref",
            Box::new(|spec| {
                spec.layers = vec![layer("tierstore", "fine"), layer("tierstore", "a\nb")];
            }),
        ),
        (
            "field: spec.mounts[0].provider.provider",
            Box::new(|spec| spec.mounts = vec![mount("UPPER", SECRET, "/in")]),
        ),
        (
            "field: spec.mounts[1].provider.ref",
            Box::new(|spec| {
                spec.mounts = vec![
                    mount("tierstore", "fine", "/in"),
                    mount("tierstore", &"k".repeat(1025), "/out"),
                ];
            }),
        ),
        (
            "field: spec.mounts[0].provider.ref",
            Box::new(|spec| spec.mounts = vec![mount("tierstore", "", "/in")]),
        ),
    ];
    for (text, change) in cases {
        let mut spec = layered("job-p");
        change(&mut spec);
        let status = writes.submit_grpc(&spec).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{text}");
        assert_eq!(status.message(), text);
        let (http, body) = writes.submit(Via::Json, &spec).await.unwrap_err();
        let field = text.strip_prefix("field: ").unwrap();
        assert_eq!(
            (http, body["field"].clone()),
            (StatusCode::BAD_REQUEST, json!(field)),
            "{text}"
        );
    }
    assert_eq!(rows(&writes.jobs), 0);
}

/// Asserts that a job with a provider layer and a provider mount is answered with both as
/// submitted (the provider, the ref, the target, the mode and `subPath`), every default of the
/// mount spelled out, `exec` container inserted for the layer and no capability shown, over
/// both transports, and that a resubmit is the same job (one row). Catches a provider source
/// dropped or changed on the way into the record or out of it, and the capability a submit
/// records leaking into the constraints a client reads.
#[tokio::test]
async fn a_job_with_provider_sources_is_answered_as_submitted() {
    let (_, writes) = over_fake(Fixed::new(true)).await;
    let mut spec = two_providers("job-r");
    spec.mounts[0].mode = v1::MountMode::ReadWrite.into();
    spec.mounts[0].sub_path = "v2".to_owned();
    let made = writes.submit(Via::Grpc, &spec).await.unwrap();
    let shown = made.spec.unwrap();
    assert_eq!(shown.layers, spec.layers);
    let expected = v1::Mount {
        propagation: v1::MountPropagation::Private.into(),
        ..spec.mounts[0].clone()
    };
    assert_eq!(shown.mounts, [expected]);
    let constraints = v1::Constraints {
        exec: v1::ExecMode::Container.into(),
        ..v1::Constraints::default()
    };
    assert_eq!(shown.constraints, Some(constraints));
    for via in Via::BOTH {
        let again = writes.submit(via, &spec).await.unwrap();
        assert_eq!(again.id, "job-r", "{via:?}");
    }
    assert_eq!(rows(&writes.jobs), 1);
}

/// Asserts admission of jobs that name providers over the registry's hosts: while no host
/// reports `mount-provider.tierstore` (one reports nothing of providers, one only `cache`) a job
/// with a `tierstore` layer is refused `no-capable-host` over both transports with nothing
/// recorded, and so is a job naming `tierstore` and `cache` while no host reports both; a plain
/// job is admitted; once a host reports `tierstore` the first is recorded as queued, and once
/// one reports both the second is. Catches a job admitted for a provider no host has, a job
/// that needs two providers admitted on hosts that have one each, and a provider job that
/// refuses a pool that has it.
#[tokio::test]
async fn a_job_naming_a_provider_no_pool_has_is_refused_no_capable_host() {
    let mut rig = clocked_rig("submit_provider", false, NEVER);
    let writes = served(&rig).await;
    let refused = (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "error": "no-capable-host" }),
    );
    rig.register("host-a", true);
    rig.register_layered_rootfs("host-b", "true");
    rig.register_mount_providers("host-c", &["cache"]);
    for via in Via::BOTH {
        for spec in [layered("job-t"), two_providers("job-t")] {
            let answer = writes.submit(via, &spec).await;
            assert_eq!(answer.unwrap_err(), refused, "{via:?}");
        }
    }
    assert_eq!(rows(&rig.jobs), 0);
    let plain = writes.submit(Via::Json, &wire("job-p")).await.unwrap();
    assert_eq!(plain.state(), v1::JobState::Queued);
    rig.register_mount_providers("host-d", &["tierstore"]);
    let one = writes.submit(Via::Json, &layered("job-t")).await.unwrap();
    assert_eq!(one.state(), v1::JobState::Queued);
    let both = two_providers("job-u");
    assert_eq!(writes.submit(Via::Grpc, &both).await.unwrap_err(), refused);
    rig.register_mount_providers("host-e", &["cache", "tierstore"]);
    let made = writes.submit(Via::Grpc, &both).await.unwrap();
    assert_eq!(made.state(), v1::JobState::Queued);
    assert_eq!(rows(&rig.jobs), 3);
}
