//! The live pull probe's logic against a registry in this process: the same function the
//! `live_pull` example runs against a real registry, with a Bearer challenge whose realm
//! is on a second host and blobs that redirect to a third.

mod support;

#[path = "../examples/live_pull/probe.rs"]
mod probe;

use js_oci::{Allow, Digest, Error, Kind, Platform};
use probe::{ProbeError, probe};
use support::{Artifact, Fixture, OTHER, REGISTRY, REPO};

const CONFIG: &str = r#"{"os":"linux","architecture":"amd64"}"#;

fn kind() -> Kind {
    Kind::image(Platform::linux("x86_64").expect("platform"))
}

/// An image whose smallest layer is the second, so choosing it is visible.
fn image() -> Artifact {
    Artifact::image(vec![vec![7u8; 5000], b"tiny layer".to_vec()], CONFIG)
}

fn entry(fx: &Fixture, name: &str, prefix: &str) -> Allow {
    Allow::parse(&format!("{}/{prefix}", fx.at(name)))
        .expect("entry")
        .private()
        .loopback()
}

async fn serving(artifact: &Artifact) -> Fixture {
    let fx = Fixture::start().await;
    fx.publish(artifact);
    fx.behave(|b| b.token = Some("tok".to_owned()));
    fx
}

/// Proves the probe's report is true: the realm named in the challenge (on another host)
/// is asked for a token with the service and scope the challenge gave, the token is
/// used on every registry request, the blob redirect to a second host is followed
/// without `Authorization`, only the smallest layer is downloaded, and the manifest,
/// config and layer are reported verified with the sink's own hash equal to the digest.
#[tokio::test]
async fn the_probe_reports_a_token_a_redirect_and_verified_digests() {
    let artifact = image();
    let fx = serving(&artifact).await;
    let (port, digest) = (fx.port, Digest::of(b"tiny layer"));
    fx.behave(|b| {
        b.realm = Some(format!("https://{OTHER}:{port}/token"));
        b.redirect_blobs = Some(format!("https://{OTHER}:{port}/cdn/{{digest}}"));
    });
    let client = fx.client_with(vec![entry(&fx, REGISTRY, "team"), entry(&fx, OTHER, "cdn")]);
    let lines = probe(
        &client,
        fx.location(REPO),
        &artifact.digest,
        &kind(),
        1 << 20,
    )
    .await
    .expect("probe");

    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with(&format!(
        "target registry={REGISTRY}:{port} repository={REPO} auth=anonymous"
    )));
    assert!(lines[1].starts_with(&format!(
        "manifest digest={} bytes={} layers=2 verified=yes ms=",
        artifact.digest,
        artifact.manifest.len()
    )));
    assert!(!lines[1].contains("token="), "{}", lines[1]);
    assert!(lines[2].starts_with(&format!(
        "config digest={} bytes={} verified=yes",
        Digest::of(CONFIG.as_bytes()),
        CONFIG.len()
    )));
    assert!(lines[3].starts_with(&format!(
        "layer digest={digest} media=application/vnd.oci.image.layer.v1.tar+gzip bytes=10 sink_sha256={digest} verified=yes"
    )));

    let (registry, other) = (fx.at(REGISTRY), fx.at(OTHER));
    let log = fx.requests();
    let token =
        format!("{other} GET /token?service=fake-registry&scope=repository:team/bundle:pull []");
    assert!(log.contains(&token), "{log:?}");
    let blob = format!("{registry} GET /v2/{REPO}/blobs/{digest} [Bearer tok]");
    let cdn = format!("{other} GET /cdn/{digest} []");
    assert!(log.contains(&blob) && log.contains(&cdn), "{log:?}");
    let big = Digest::of(&artifact.layers[0]);
    assert!(
        log.iter().all(|r| !r.contains(big.as_str())),
        "only the smallest layer is fetched: {log:?}"
    );
}

/// Proves a redirect to a host the policy does not let near loopback fails the probe with
/// js-oci's address error (naming the host), before any request reaches that host.
#[tokio::test]
async fn a_refused_redirect_host_fails_the_probe() {
    let artifact = image();
    let fx = serving(&artifact).await;
    let port = fx.port;
    fx.behave(|b| b.redirect_blobs = Some(format!("https://{OTHER}:{port}/cdn/{{digest}}")));
    let client = fx.client_with(vec![entry(&fx, REGISTRY, "team")]);
    let err = probe(
        &client,
        fx.location(REPO),
        &artifact.digest,
        &kind(),
        1 << 20,
    )
    .await
    .expect_err("refused");
    assert!(
        matches!(&err, ProbeError::Pull(Error::Address { .. })),
        "{err}"
    );
    assert!(err.to_string().contains(OTHER), "{err}");
    assert!(
        fx.requests().iter().all(|r| !r.contains("/cdn/")),
        "{:?}",
        fx.requests()
    );
}

/// Proves a pinned digest the registry does not hold fails with the registry's status
/// (the shape of a missing or private package), and a smallest layer over the limit is
/// refused before it is downloaded.
#[tokio::test]
async fn a_missing_manifest_and_an_oversize_layer_fail_with_their_reason() {
    let artifact = image();
    let fx = serving(&artifact).await;
    let client = fx.client_with(vec![entry(&fx, REGISTRY, "team")]);
    let missing = Digest::of(b"not published");
    let err = probe(&client, fx.location(REPO), &missing, &kind(), 1 << 20)
        .await
        .expect_err("missing");
    assert!(
        matches!(&err, ProbeError::Pull(Error::Status { status: 404, .. })),
        "{err}"
    );

    let err = probe(&client, fx.location(REPO), &artifact.digest, &kind(), 5)
        .await
        .expect_err("too large");
    assert!(
        matches!(err, ProbeError::LayerTooLarge { size: 10, max: 5 }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "the smallest layer is 10 bytes, over --max-layer-bytes 5"
    );
    let layer = Digest::of(b"tiny layer");
    assert!(
        fx.requests().iter().all(|r| !r.contains(layer.as_str())),
        "{:?}",
        fx.requests()
    );
}

/// Proves a location the policy does not admit is refused as `NotAllowed` before any
/// request is made.
#[tokio::test]
async fn a_location_outside_the_policy_is_refused_before_any_request() {
    let artifact = image();
    let fx = serving(&artifact).await;
    let client = fx.client_with(vec![entry(&fx, REGISTRY, "team")]);
    let err = probe(
        &client,
        fx.location("elsewhere/bundle"),
        &artifact.digest,
        &kind(),
        1 << 20,
    )
    .await
    .expect_err("not allowed");
    assert!(
        matches!(&err, ProbeError::Pull(Error::NotAllowed { .. })),
        "{err}"
    );
    assert!(fx.requests().is_empty(), "{:?}", fx.requests());
}

/// Proves a manifest whose config blob the registry does not hold fails the probe with
/// the registry's status, before any layer is requested.
#[tokio::test]
async fn an_unpublished_config_blob_fails_the_probe() {
    let artifact = image();
    let fx = Fixture::start().await;
    fx.publish_manifest_as(&artifact.digest, &artifact.manifest);
    let client = fx.client_with(vec![entry(&fx, REGISTRY, "team")]);
    let err = probe(
        &client,
        fx.location(REPO),
        &artifact.digest,
        &kind(),
        1 << 20,
    )
    .await
    .expect_err("config missing");
    assert!(
        matches!(&err, ProbeError::Pull(Error::Status { status: 404, .. })),
        "{err}"
    );
    let layer = Digest::of(b"tiny layer");
    assert!(
        fx.requests().iter().all(|r| !r.contains(layer.as_str())),
        "{:?}",
        fx.requests()
    );
}
