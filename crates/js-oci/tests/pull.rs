//! The pull core against a registry in this process, over TLS: what it verifies, what it
//! refuses, where it may connect, and how it behaves on the wire.

mod support;

use std::time::Duration;

use async_trait::async_trait;
use js_oci::{Allow, BlobSink, Client, Digest, Error, Kind, Location, Platform, Policy, Timeouts};
use js_testkit::dir::TestDir;
use support::{Artifact, Behaviour, Fixture, OTHER, REGISTRY, REPO};

fn layers() -> Vec<Vec<u8>> {
    vec![b"first layer".to_vec(), vec![7u8; 5000]]
}

fn bundle() -> Artifact {
    Artifact::bundle(layers())
}

async fn started(artifact: &Artifact) -> Fixture {
    let fx = Fixture::start().await;
    fx.publish(artifact);
    fx
}

/// A sink that records what it is told and can be made to fail.
#[derive(Default)]
struct Recording {
    bytes: Vec<u8>,
    finished: bool,
    fail_write: bool,
    fail_finish: bool,
}

#[async_trait]
impl BlobSink for Recording {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if self.fail_write {
            return Err(std::io::Error::other("disk full"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        if self.fail_finish {
            return Err(std::io::Error::other("flush failed"));
        }
        self.finished = true;
        Ok(())
    }
}

type Change = Box<dyn Fn(&mut Behaviour)>;

fn change(f: impl Fn(&mut Behaviour) + 'static) -> Change {
    Box::new(f)
}

/// Fetches the manifest of `artifact` and then its first layer into a recording sink.
async fn first_layer(fx: &Fixture, artifact: &Artifact, sink: &mut Recording) -> Result<(), Error> {
    let client = fx.client();
    let repo = client.repository(fx.location(REPO))?;
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await?;
    repo.fetch_blob(&manifest.layers()[0], sink).await
}

/// The error of fetching the manifest of `artifact` with `kind`.
async fn manifest_error(fx: &Fixture, artifact: &Artifact, kind: &Kind) -> Error {
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    repo.fetch_manifest(&artifact.digest, kind)
        .await
        .expect_err("refused")
}

/// Proves a pin is honoured and every blob arrives whole: the manifest is read, the config
/// comes back byte for byte for the right platform, and each layer lands in its sink
/// (memory, or a file that is flushed) with the sink finished, whether the registry sends
/// plain bodies or chunked ones with a trailer, and with layers fetched concurrently.
#[tokio::test]
async fn a_bundle_is_pulled_whole_and_each_sink_is_finished() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let kind = Kind::bundle();
    assert!(format!("{repo:?}").contains("Repository"));
    assert!(format!("{client:?}").contains("Client"));
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    assert_eq!(*manifest.digest(), artifact.digest);
    assert_eq!(
        manifest.artifact_type(),
        Some("application/vnd.komira.js.bundle.v1")
    );
    assert_eq!(manifest.layers().len(), 2);
    assert_eq!(
        repo.fetch_config(&manifest, &kind).await.expect("config"),
        artifact.config
    );
    let (mut a, mut b) = (Recording::default(), Recording::default());
    let (first, second) = tokio::join!(
        repo.fetch_blob(&manifest.layers()[0], &mut a),
        repo.fetch_blob(&manifest.layers()[1], &mut b)
    );
    first.expect("first layer");
    second.expect("second layer");
    assert_eq!((a.bytes, a.finished), (layers()[0].clone(), true));
    assert_eq!((b.bytes, b.finished), (layers()[1].clone(), true));

    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-oci-file-sink");
    let path = dir.join("layer");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    repo.fetch_blob(&manifest.layers()[1], &mut file)
        .await
        .expect("to a file");
    assert_eq!(std::fs::read(&path).expect("read"), layers()[1]);

    fx.behave(|b| b.trailers = true);
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("chunked");
    let mut c = Recording::default();
    repo.fetch_blob(&manifest.layers()[1], &mut c)
        .await
        .expect("chunked blob");
    assert_eq!(c.bytes, layers()[1]);
}

/// Proves a registry on a literal address is reached when listed, with the certificate
/// checked against the address.
#[tokio::test]
async fn a_registry_on_a_literal_address_is_reached() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let at = fx.at("127.0.0.1");
    let client = fx.client_for(&[&format!("{at}/team")], true);
    let location = Location::parse(&format!("{at}/{REPO}")).expect("location");
    let repo = client.repository(location).expect("repository");
    repo.fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
}

/// Proves an image is pulled as images are: Docker-compatible kind, linux platform, and
/// a config that states nothing accepted, with the layers' types allowed by the kind.
#[tokio::test]
async fn an_image_is_pulled_for_the_platform_the_caller_chose() {
    let kind = Kind::image(Platform::linux("x86_64").expect("platform"));
    for config in [r#"{"os":"linux","architecture":"amd64"}"#, "{}"] {
        let artifact = Artifact::image(layers(), config);
        let fx = started(&artifact).await;
        let client = fx.client();
        let repo = client.repository(fx.location(REPO)).expect("repository");
        let manifest = repo
            .fetch_manifest(&artifact.digest, &kind)
            .await
            .expect("manifest");
        repo.fetch_config(&manifest, &kind).await.expect(config);
    }
    let artifact = Artifact::image(layers(), r#"{"os":"linux","architecture":"arm64"}"#);
    let fx = started(&artifact).await;
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let err = repo
        .fetch_config(&manifest, &kind)
        .await
        .expect_err("arm64");
    assert!(matches!(err, Error::Platform { .. }), "{err}");
    // And a bundle is not an image of this kind: its layer types are not taken.
    let artifact = bundle();
    fx.publish(&artifact);
    let err = manifest_error(&fx, &artifact, &kind).await;
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
}

/// Proves a manifest is believed only under its own digest: another manifest served under
/// the pin is refused.
#[tokio::test]
async fn a_manifest_that_does_not_hash_to_its_pin_is_refused() {
    let artifact = bundle();
    let other = Artifact::bundle(vec![b"other".to_vec()]);
    let fx = started(&artifact).await;
    fx.publish_manifest_as(&artifact.digest, &other.manifest);
    let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
    assert!(matches!(err, Error::ManifestDigest { .. }), "{err}");
}

/// Proves an index digest is refused as an index (not read as an image), though its bytes
/// hash to the pin, and that the request asked for index types so a registry answers
/// with the index rather than a 404.
#[tokio::test]
async fn an_image_index_is_refused() {
    let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
    let digest = Digest::of(index);
    let fx = Fixture::start().await;
    fx.publish_manifest_as(&digest, index);
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo
        .fetch_manifest(&digest, &Kind::bundle())
        .await
        .expect_err("index");
    assert!(matches!(err, Error::Index(_)), "{err}");
}

/// Proves the artifact type is checked: an image manifest (no artifact type) and a
/// manifest of another type are not a bundle.
#[tokio::test]
async fn a_manifest_of_another_artifact_type_is_not_a_bundle() {
    let fx = Fixture::start().await;
    let image = Artifact::image(layers(), "{}");
    fx.publish(&image);
    let err = manifest_error(&fx, &image, &Kind::bundle()).await;
    assert!(
        matches!(err, Error::ArtifactType { have: None, .. }),
        "{err}"
    );
    let other = Artifact::build(
        "application/vnd.oci.image.manifest.v1+json",
        Some("application/vnd.example.other"),
        "application/vnd.komira.js.bundle.config.v1+json",
        b"{}".to_vec(),
        "application/vnd.komira.js.bundle.layer.v1.tar+zstd",
        layers(),
    );
    fx.publish(&other);
    let err = manifest_error(&fx, &other, &Kind::bundle()).await;
    assert!(
        matches!(&err, Error::ArtifactType { have: Some(t), .. } if t == "application/vnd.example.other"),
        "{err}"
    );
}

/// Proves the size limits hold before the bytes are believed: a manifest over its limit,
/// a config declared over its limit (refused without fetching it), and a config body
/// longer than the manifest declares.
#[tokio::test]
async fn oversize_manifests_and_configs_are_refused() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let mut kind = Kind::bundle();
    kind.limits.manifest = 10;
    let err = manifest_error(&fx, &artifact, &kind).await;
    assert!(matches!(err, Error::TooLarge { limit: 10, .. }), "{err}");

    let big_config = format!(
        r#"{{"os":"darwin","architecture":"arm64","pad":"{}"}}"#,
        "x".repeat(2000)
    );
    let big = Artifact::bundle_with_config(layers(), &big_config);
    fx.publish(&big);
    let mut kind = Kind::bundle();
    kind.limits.config = 1000;
    let before = fx.requests().len();
    let err = manifest_error(&fx, &big, &kind).await;
    assert!(matches!(err, Error::TooLarge { limit: 1000, .. }), "{err}");
    assert_eq!(
        fx.requests().len(),
        before + 1,
        "only the manifest was asked for"
    );

    fx.publish_blob_as(
        &Digest::of(&artifact.config),
        &b"x".repeat(artifact.config.len() + 1),
    );
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let kind = Kind::bundle();
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let err = repo
        .fetch_config(&manifest, &kind)
        .await
        .expect_err("long config");
    assert!(matches!(err, Error::BlobSize { .. }), "{err}");
}

/// Proves a blob is whole or refused: bytes of the right length that hash differently,
/// a short body and a long body each fail with their own error, and the sink is never
/// told the blob is whole.
#[tokio::test]
async fn a_blob_with_the_wrong_digest_or_size_is_refused_and_never_finished() {
    let artifact = bundle();
    let digest = Digest::of(&artifact.layers[0]);
    let right = artifact.layers[0].len();
    for (name, bytes) in [
        ("same length, other bytes", b"z".repeat(right)),
        ("short", artifact.layers[0][..right - 1].to_vec()),
        ("long", [artifact.layers[0].clone(), b"!".to_vec()].concat()),
    ] {
        let fx = started(&artifact).await;
        fx.publish_blob_as(&digest, &bytes);
        let mut sink = Recording::default();
        let err = first_layer(&fx, &artifact, &mut sink)
            .await
            .expect_err(name);
        match name {
            "same length, other bytes" => assert!(matches!(err, Error::BlobDigest { .. }), "{err}"),
            "short" => assert!(
                matches!(err, Error::BlobSize { declared, got, .. } if got + 1 == declared),
                "{err}"
            ),
            _ => assert!(
                matches!(err, Error::BlobSize { declared, got, .. } if got == declared + 1),
                "{err}"
            ),
        }
        assert!(!sink.finished, "{name}");
    }
    let fx = started(&artifact).await;
    fx.publish_blob_as(
        &Digest::of(&artifact.config),
        &b"z".repeat(artifact.config.len()),
    );
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    let err = repo
        .fetch_config(&manifest, &Kind::bundle())
        .await
        .expect_err("config");
    assert!(matches!(err, Error::BlobDigest { .. }), "{err}");
}

/// Proves a failing sink fails the pull: a write error and a flush error are both
/// reported as the sink's.
#[tokio::test]
async fn a_sink_that_fails_fails_the_pull() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let mut write = Recording {
        fail_write: true,
        ..Recording::default()
    };
    let err = first_layer(&fx, &artifact, &mut write)
        .await
        .expect_err("write");
    assert!(matches!(err, Error::Sink(_)), "{err}");
    let mut finish = Recording {
        fail_finish: true,
        ..Recording::default()
    };
    let err = first_layer(&fx, &artifact, &mut finish)
        .await
        .expect_err("finish");
    assert!(matches!(err, Error::Sink(_)), "{err}");
    assert_eq!(finish.bytes, layers()[0], "the bytes had all arrived");
}

/// Proves the platform is checked on the config of a bundle: darwin/arm64 only, and a
/// config that states less is refused, as is a config that is not JSON.
#[tokio::test]
async fn a_bundle_for_another_platform_or_none_is_refused() {
    for config in [
        r#"{"os":"linux","architecture":"arm64"}"#,
        r#"{"os":"darwin","architecture":"amd64"}"#,
        r#"{"os":"darwin"}"#,
        "{}",
    ] {
        let artifact = Artifact::bundle_with_config(layers(), config);
        let fx = started(&artifact).await;
        let client = fx.client();
        let repo = client.repository(fx.location(REPO)).expect("repository");
        let kind = Kind::bundle();
        let manifest = repo
            .fetch_manifest(&artifact.digest, &kind)
            .await
            .expect("manifest");
        let err = repo.fetch_config(&manifest, &kind).await.expect_err(config);
        assert!(matches!(err, Error::Platform { .. }), "{config}: {err}");
    }
    let artifact = Artifact::bundle_with_config(layers(), "not json");
    let fx = started(&artifact).await;
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    let err = repo
        .fetch_config(&manifest, &Kind::bundle())
        .await
        .expect_err("not json");
    assert!(matches!(err, Error::Malformed(_)), "{err}");
}

/// Proves a pin may name only a listed registry and namespace.
#[tokio::test]
async fn a_pin_outside_the_allow_list_is_not_pulled() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let client = fx.client();
    for name in [
        fx.location("other/bundle"),
        fx.location("team-evil/bundle"),
        Location::parse(&format!("{}/{REPO}", fx.at(OTHER))).expect("location"),
        Location::parse("ghcr.io/team/bundle").expect("location"),
    ] {
        let err = client.repository(name).expect_err("not allowed");
        assert!(matches!(err, Error::NotAllowed { .. }), "{err}");
    }
    assert!(fx.requests().is_empty(), "nothing was asked for");
}

/// Proves a token is asked for as the challenge says, and used: the first request is
/// anonymous, the token request names service and scope (and keeps a query the realm
/// had), the retry carries the token, and an `access_token` answer works too.
#[tokio::test]
async fn a_bearer_challenge_is_answered_once_and_the_token_is_used() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    fx.behave(|b| b.token = Some("tok".to_owned()));
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let kind = Kind::bundle();
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let host = fx.at(REGISTRY);
    let digest = &artifact.digest;
    assert_eq!(
        fx.requests(),
        [
            format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{host} GET /token?service=fake-registry&scope=repository:team/bundle:pull []"),
            format!("{host} GET /v2/{REPO}/manifests/{digest} [Bearer tok]"),
        ]
    );
    let mut sink = Recording::default();
    repo.fetch_blob(&manifest.layers()[0], &mut sink)
        .await
        .expect("blob");
    assert_eq!(
        fx.requests().len(),
        4,
        "the token is kept for the next request"
    );

    let variants: Vec<(&str, Change)> = vec![
        (
            "access_token",
            change(|b| b.token_answer = Some(r#"{"access_token":"tok"}"#.to_owned())),
        ),
        ("no scope", change(|b| b.challenge_no_scope = true)),
        ("no service", change(|b| b.challenge_no_service = true)),
        ("realm with a query", change(|b| b.realm_query = true)),
    ];
    for (name, apply) in variants {
        let fx = started(&artifact).await;
        fx.behave(|b| b.token = Some("tok".to_owned()));
        fx.behave(apply);
        let client = fx.client();
        let repo = client.repository(fx.location(REPO)).expect("repository");
        repo.fetch_manifest(&artifact.digest, &kind)
            .await
            .expect(name);
        let token_request = fx
            .requests()
            .into_iter()
            .find(|r| r.contains("GET /token"))
            .expect(name);
        match name {
            "no service" => assert!(!token_request.contains("service="), "{token_request}"),
            "realm with a query" => assert!(
                token_request.contains("/token?client=js&service=fake-registry&scope="),
                "{token_request}"
            ),
            _ => {}
        }
    }
}

/// Proves authentication failures say what failed: another scheme, no challenge, a token
/// the registry refuses, a token endpoint that errors, answers with no JSON, or holds no
/// token.
#[tokio::test]
async fn authentication_failures_say_what_failed() {
    let artifact = bundle();
    let cases: Vec<(&str, Change, &str)> = vec![
        (
            "basic",
            change(|b| {
                b.token = Some("t".to_owned());
                b.challenge_scheme = Some("Basic".to_owned());
            }),
            "Basic",
        ),
        (
            "no challenge",
            change(|b| {
                b.token = Some("t".to_owned());
                b.no_challenge = true;
            }),
            "no WWW-Authenticate",
        ),
        (
            "refused",
            change(|b| {
                b.refuse_token = true;
                b.token_answer = Some(r#"{"token":"x"}"#.to_owned());
            }),
            "refused the token",
        ),
        (
            "token status",
            change(|b| {
                b.token = Some("t".to_owned());
                b.token_status = Some(500);
            }),
            "500",
        ),
        (
            "not json",
            change(|b| {
                b.token = Some("t".to_owned());
                b.token_answer = Some("nope".to_owned());
            }),
            "not JSON",
        ),
        (
            "empty",
            change(|b| {
                b.token = Some("t".to_owned());
                b.token_answer = Some(r#"{"token":""}"#.to_owned());
            }),
            "no token",
        ),
    ];
    for (name, apply, text) in cases {
        let fx = started(&artifact).await;
        fx.behave(apply);
        let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
        assert!(err.to_string().contains(text), "{name}: {err}");
    }
}

/// Proves a redirect is followed without credentials: the registry's token is not sent to
/// the target, whether the `Location` is a path or a full URL, and the bytes are still
/// verified.
#[tokio::test]
async fn a_redirect_is_followed_without_credentials() {
    let artifact = bundle();
    for to in [
        "/cdn/{digest}".to_owned(),
        "https://registry.test:{port}/cdn/{digest}".to_owned(),
    ] {
        let fx = started(&artifact).await;
        let to = to.replace("{port}", &fx.port.to_string());
        fx.behave(|b| {
            b.token = Some("tok".to_owned());
            b.redirect_blobs = Some(to);
        });
        let mut sink = Recording::default();
        first_layer(&fx, &artifact, &mut sink)
            .await
            .expect("through the redirect");
        assert_eq!(sink.bytes, layers()[0]);
        let log = fx.requests();
        let blob = log
            .iter()
            .rev()
            .find(|r| r.contains("/blobs/"))
            .expect("blob request");
        assert!(blob.ends_with("[Bearer tok]"), "{blob}");
        let cdn = log
            .iter()
            .find(|r| r.contains("/cdn/"))
            .expect("redirect target");
        assert!(cdn.ends_with("[]"), "the target got credentials: {cdn}");
    }
}

/// Proves a redirect that cannot be followed is an error and not a hang: a loop, a
/// redirect with no target, a target that is not https, one that is not a URL, and a host
/// name that cannot be a server's.
#[tokio::test]
async fn redirects_that_cannot_be_followed_are_errors() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let mut sink = Recording::default();
    fx.behave(|b| b.redirect_loop = true);
    let err = first_layer(&fx, &artifact, &mut sink)
        .await
        .expect_err("loop");
    assert!(err.to_string().contains("more than 5 redirects"), "{err}");
    fx.behave(|b| {
        b.redirect_loop = false;
        b.redirect_without_location = true;
    });
    let err = first_layer(&fx, &artifact, &mut sink)
        .await
        .expect_err("no location");
    assert!(err.to_string().contains("no Location"), "{err}");
    fx.behave(|b| b.redirect_without_location = false);
    for (to, want) in [
        ("http://registry.test/x", "not https"),
        ("https://exa mple/x", "invalid"),
        ("https://-bad.example/x", "invalid dns name"),
    ] {
        fx.behave(|b| b.redirect_blobs = Some(to.to_owned()));
        let err = first_layer(&fx, &artifact, &mut sink).await.expect_err(to);
        assert!(err.to_string().contains(want), "{to}: {err}");
    }
    assert!(!sink.finished);
}

/// Proves the address rules on every request, redirect and name: a redirect to a
/// loopback, link-local, private, shared-range, unique-local, unspecified or IPv4-mapped
/// address, and to a name that resolves to loopback, is refused before any connection;
/// the registry itself is refused at a private address unless its entry says private.
#[tokio::test]
async fn redirects_to_refused_addresses_are_not_followed() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let port = fx.port;
    for (to, class) in [
        (
            format!("https://127.0.0.1:{port}/cdn/{{digest}}"),
            "loopback",
        ),
        (format!("https://[::1]:{port}/cdn/{{digest}}"), "loopback"),
        (
            format!("https://[::ffff:7f00:1]:{port}/cdn/{{digest}}"),
            "loopback",
        ),
        (
            format!("https://rebind.test:{port}/cdn/{{digest}}"),
            "loopback",
        ),
        ("https://169.254.1.1/latest".to_owned(), "link-local"),
        (
            "https://169.254.169.254/latest".to_owned(),
            "cloud-metadata",
        ),
        ("https://168.63.129.16/x".to_owned(), "cloud-metadata"),
        ("https://[fd00:ec2::254]/x".to_owned(), "cloud-metadata"),
        ("https://198.18.0.1/x".to_owned(), "multicast or reserved"),
        ("https://[2002:7f00:1::1]/x".to_owned(), "loopback"),
        ("https://[fec0::1]/x".to_owned(), "site-local"),
        ("https://10.0.0.1/x".to_owned(), "private"),
        ("https://192.168.1.1/x".to_owned(), "private"),
        ("https://[::ffff:a00:1]/x".to_owned(), "private"),
        ("https://100.64.0.1/x".to_owned(), "shared-range"),
        ("https://[fd00::1]/x".to_owned(), "unique-local"),
        ("https://0.0.0.0/x".to_owned(), "unspecified"),
    ] {
        fx.behave(|b| b.redirect_blobs = Some(to.clone()));
        let mut sink = Recording::default();
        let err = first_layer(&fx, &artifact, &mut sink).await.expect_err(&to);
        assert!(
            matches!(&err, Error::Address { class: c, .. } if c.to_string() == class),
            "{to}: {err}"
        );
        assert!(!sink.finished);
    }
    assert!(
        fx.requests()
            .iter()
            .all(|r| !r.contains("/cdn/") && !r.contains(" GET /x")),
        "a refused address was connected to: {:?}",
        fx.requests()
    );

    // The registry's own host at a loopback address, not listed as private.
    let fx = started(&artifact).await;
    let client = fx.client_for(&[&format!("{}/team", fx.at(REGISTRY))], false);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect_err("public entry");
    assert!(matches!(err, Error::Address { .. }), "{err}");
    assert!(fx.requests().is_empty());
}

/// Proves the token endpoint is pinned: a realm on a host nobody listed, a loopback
/// literal, a plain-http realm and a realm that is no URL are refused; another listed host
/// is asked; and a listed host at a refused address is refused by the address rules.
#[tokio::test]
async fn the_token_realm_must_be_the_registry_or_a_listed_host() {
    let artifact = bundle();
    for realm in [
        "https://evil.test/token".to_owned(),
        "http://registry.test/token".to_owned(),
        "not a url".to_owned(),
    ] {
        let fx = started(&artifact).await;
        fx.behave(|b| {
            b.token = Some("tok".to_owned());
            b.realm = Some(realm.clone());
        });
        let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
        assert!(matches!(err, Error::Realm(_)), "{realm}: {err}");
    }
    let fx = started(&artifact).await;
    let literal = format!("https://127.0.0.1:{}/token", fx.port);
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.realm = Some(literal);
    });
    let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
    assert!(matches!(err, Error::Realm(_)), "{err}");

    // Another listed host: asked, and its token is used.
    let other = format!("https://{}/token", fx.at(OTHER));
    fx.behave(|b| b.realm = Some(other));
    let both = fx.client_for(
        &[
            &format!("{}/team", fx.at(REGISTRY)),
            &format!("{}/team", fx.at(OTHER)),
        ],
        true,
    );
    let repo = both.repository(fx.location(REPO)).expect("repository");
    repo.fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("listed realm");
    let token_request = fx
        .requests()
        .into_iter()
        .find(|r| r.contains("GET /token"))
        .expect("token");
    assert!(token_request.starts_with(&fx.at(OTHER)), "{token_request}");

    // Listed, but not private, and it resolves to loopback.
    let public_other = fx.client_with(vec![
        Allow::parse(&format!("{}/team", fx.at(REGISTRY)))
            .expect("entry")
            .private()
            .loopback(),
        Allow::parse(&format!("{}/team", fx.at(OTHER))).expect("entry"),
    ]);
    let repo = public_other
        .repository(fx.location(REPO))
        .expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect_err("rebinding");
    assert!(matches!(err, Error::Address { .. }), "{err}");
}

/// Proves transport failures are errors and not hangs: a status, a server that hangs up,
/// one that never answers (a zero head timeout), one that stalls mid-body (zero idle
/// timeout), a body that ends early, and a handshake that never ends (zero connect
/// timeout).
#[tokio::test]
async fn transport_failures_are_errors_not_hangs() {
    let artifact = bundle();
    let kind = Kind::bundle();
    let fx = started(&artifact).await;
    fx.behave(|b| b.status = Some(404));
    let err = manifest_error(&fx, &artifact, &kind).await;
    assert!(matches!(err, Error::Status { status: 404, .. }), "{err}");
    fx.behave(|b| {
        b.status = None;
        b.hang_up = true;
    });
    let err = manifest_error(&fx, &artifact, &kind).await;
    assert!(matches!(err, Error::Transport { .. }), "{err}");

    let fx = started(&artifact).await;
    let zero = Timeouts {
        head: Duration::ZERO,
        ..Timeouts::default()
    };
    fx.behave(|b| b.stall_head = true);
    let client = fx.client().with_timeouts(zero);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect_err("stalled head");
    assert!(err.to_string().contains("no response headers"), "{err}");

    let fx = started(&artifact).await;
    let manifest = {
        let client = fx.client();
        let repo = client.repository(fx.location(REPO)).expect("repository");
        repo.fetch_manifest(&artifact.digest, &kind)
            .await
            .expect("manifest")
    };
    let zero = Timeouts {
        idle: Duration::ZERO,
        ..Timeouts::default()
    };
    let client = fx.client().with_timeouts(zero);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    fx.behave(|b| b.stall_body = true);
    let err = repo
        .fetch_blob(&manifest.layers()[0], &mut Recording::default())
        .await
        .expect_err("stall");
    assert!(err.to_string().contains("stalled"), "{err}");
    fx.behave(|b| {
        b.stall_body = false;
        b.short_body = true;
    });
    let err = repo
        .fetch_blob(&manifest.layers()[0], &mut Recording::default())
        .await;
    assert!(matches!(err, Err(Error::Transport { .. })), "{err:?}");

    let (silent, _task) = Fixture::silent().await;
    let entry = format!("{REGISTRY}:{silent}/team");
    let zero = Timeouts {
        connect: Duration::ZERO,
        ..Timeouts::default()
    };
    let client = fx.client_for(&[&entry], true).with_timeouts(zero);
    let location = Location::parse(&format!("{REGISTRY}:{silent}/{REPO}")).expect("location");
    let repo = client.repository(location).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect_err("handshake");
    assert!(err.to_string().contains("timed out"), "{err}");
}

/// Proves names and certificates are checked: a name that does not resolve, a registry
/// whose certificate is not trusted (the system roots, another authority, none at all).
#[tokio::test]
async fn names_and_certificates_that_do_not_check_out_are_errors() {
    let artifact = bundle();
    let kind = Kind::bundle();
    let fx = started(&artifact).await;
    let policy = || {
        Policy::new(vec![
            Allow::parse(&format!("{}/team", fx.at(REGISTRY)))
                .expect("entry")
                .private()
                .loopback(),
        ])
    };
    let other_authority = Fixture::start().await.cert.clone();
    for (name, client, text) in [
        ("system roots", Client::new(policy()), "TLS"),
        (
            "another authority",
            Client::with_roots(policy(), vec![other_authority]),
            "TLS",
        ),
        (
            "no roots",
            Client::with_roots(policy(), vec![]),
            "no trusted root certificates",
        ),
    ] {
        let client = client.with_resolved(REGISTRY, support::LOOPBACK);
        let repo = client.repository(fx.location(REPO)).expect("repository");
        let err = repo
            .fetch_manifest(&artifact.digest, &kind)
            .await
            .expect_err(name);
        assert!(matches!(err, Error::Transport { .. }), "{name}: {err}");
        assert!(err.to_string().contains(text), "{name}: {err}");
    }
    let nowhere = Policy::new(vec![
        Allow::parse("nonexistent.invalid:443/team").expect("entry"),
    ]);
    let client = Client::new(nowhere);
    let location = Location::parse("nonexistent.invalid/team/x").expect("location");
    let repo = client.repository(location).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect_err("no such name");
    assert!(matches!(err, Error::Transport { .. }), "{err}");
}

/// Proves a connection that is refused and a token that cannot be sent are errors: the
/// port nothing listens on, and a token with a newline in it, which is no header value.
#[tokio::test]
async fn a_refused_connection_and_an_unsendable_token_are_errors() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let closed = Fixture::silent().await;
    let port = closed.0;
    closed.1.abort();
    let _ = closed.1.await;
    let entry = format!("{REGISTRY}:{port}/team");
    let client = fx.client_for(&[&entry], true);
    let location = Location::parse(&format!("{REGISTRY}:{port}/{REPO}")).expect("location");
    let repo = client.repository(location).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("connect:"), "{err}");

    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_answer = Some(r#"{"token":"a\nb"}"#.to_owned());
    });
    let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
    assert!(matches!(err, Error::Transport { .. }), "{err}");
}

/// Proves nothing secret in a URL reaches an error: a redirect target's query (a signed
/// CDN link) and userinfo, the query of a plain-http target, and the query of a token
/// endpoint are all absent from the text of the error and from its `Debug` form.
#[tokio::test]
async fn urls_in_errors_carry_no_query_or_credentials() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let port = fx.port;
    for to in [
        format!("https://registry.test:{port}/cdn/{{digest}}?sig=SECRET"),
        format!("https://user:PASSWORD@registry.test:{port}/cdn/{{digest}}?sig=SECRET"),
    ] {
        fx.behave(|b| {
            b.redirect_blobs = Some(to.clone());
            b.hang_up_cdn = true;
        });
        let mut sink = Recording::default();
        let err = first_layer(&fx, &artifact, &mut sink).await.expect_err(&to);
        let text = format!("{err} {err:?}");
        assert!(matches!(err, Error::Transport { .. }), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("PASSWORD"), "{text}");
        assert!(
            fx.requests().iter().any(|r| r.contains("/cdn/")),
            "the redirect was not followed: {:?}",
            fx.requests()
        );
    }
    fx.behave(|b| b.redirect_blobs = Some("http://registry.test/x?q=SECRET".to_owned()));
    let err = first_layer(&fx, &artifact, &mut Recording::default())
        .await
        .expect_err("http target");
    let text = format!("{err} {err:?}");
    assert!(matches!(err, Error::Insecure(_)), "{text}");
    assert!(!text.contains("SECRET"), "{text}");

    let fx = started(&artifact).await;
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.realm = Some(format!(
            "https://registry.test:{}/token?key=SECRET",
            fx.port
        ));
        b.token_status = Some(500);
    });
    let err = manifest_error(&fx, &artifact, &Kind::bundle()).await;
    let text = format!("{err} {err:?}");
    assert!(matches!(err, Error::Status { status: 500, .. }), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
}

/// A manifest document with these two fields hostile.
fn hostile_manifest(media_type: &str, layer_digest: &str) -> Vec<u8> {
    format!(
        r#"{{"schemaVersion":2,"mediaType":"{media_type}","artifactType":"application/vnd.komira.js.bundle.v1","config":{{"mediaType":"application/vnd.komira.js.bundle.config.v1+json","digest":"sha256:{zeros}","size":1}},"layers":[{{"mediaType":"application/vnd.komira.js.bundle.layer.v1.tar+zstd","digest":"{layer_digest}","size":1}}]}}"#,
        zeros = "0".repeat(64)
    )
    .into_bytes()
}

/// Proves text a registry supplies is escaped and cut before it is echoed: control
/// bytes (escape, CR, LF) in a manifest's media type, a 64 KiB digest, a 64 KiB
/// redirect target and a 64 KiB token realm give single-line errors of bounded size.
#[tokio::test]
async fn hostile_text_is_escaped_and_truncated_in_errors() {
    let artifact = bundle();
    let kind = Kind::bundle();
    let long = "z".repeat(64 << 10);
    let fx = started(&artifact).await;

    let evil = hostile_manifest(r"application/x\u001b[31m\r\nINJECTED", "sha256:00");
    let pin = Digest::of(&evil);
    fx.publish_manifest_as(&pin, &evil);
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo.fetch_manifest(&pin, &kind).await.expect_err("media");
    let text = format!("{err} {err:?}");
    assert!(text.contains("INJECTED"), "{text}");
    assert!(!text.contains(['\u{1b}', '\r', '\n']), "{text:?}");

    let big = hostile_manifest("application/vnd.oci.image.manifest.v1+json", &long);
    let pin = Digest::of(&big);
    fx.publish_manifest_as(&pin, &big);
    let err = repo.fetch_manifest(&pin, &kind).await.expect_err("digest");
    assert!(matches!(err, Error::Digest(_)), "{err}");
    assert!(
        format!("{err} {err:?}").len() < 1024,
        "{}",
        err.to_string().len()
    );

    fx.behave(|b| b.redirect_blobs = Some(format!("https://exa mple/{long}")));
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let err = repo
        .fetch_blob(&manifest.layers()[0], &mut Recording::default())
        .await
        .expect_err("target");
    assert!(
        format!("{err} {err:?}").len() < 1024,
        "{}",
        err.to_string().len()
    );

    let fx = started(&artifact).await;
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.realm = Some(format!("https://evil.test/{long}"));
    });
    let err = manifest_error(&fx, &artifact, &kind).await;
    assert!(matches!(err, Error::Realm(_)), "{err}");
    assert!(
        format!("{err} {err:?}").len() < 1024,
        "{}",
        err.to_string().len()
    );
}

/// Proves a redirect cannot probe other ports: a target on any port but 443 or the
/// registry's own is refused before a connection, with an error that says why.
#[tokio::test]
async fn redirects_may_only_use_port_443_or_the_registrys_own() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    for port in [22u16, 8443, 65535] {
        fx.behave(|b| {
            b.redirect_blobs = Some(format!("https://registry.test:{port}/cdn/{{digest}}"));
        });
        let err = first_layer(&fx, &artifact, &mut Recording::default())
            .await
            .expect_err("port");
        assert!(err.to_string().contains("only port 443"), "{port}: {err}");
    }
    assert!(fx.requests().iter().all(|r| !r.contains("/cdn/")));
    fx.behave(|b| {
        b.redirect_blobs = Some(format!("https://registry.test:{}/cdn/{{digest}}", fx.port));
    });
    first_layer(&fx, &artifact, &mut Recording::default())
        .await
        .expect("the registry's own port");
    fx.behave(|b| b.redirect_blobs = Some("https://registry.test/cdn/{digest}".to_owned()));
    let err = first_layer(&fx, &artifact, &mut Recording::default())
        .await
        .expect_err("443 is dialled, nothing listens");
    assert!(!err.to_string().contains("only port 443"), "{err}");
}

/// Proves a `Location` is resolved as RFC 3986 says: a `://` inside a query is part of a
/// path on the same host, `//host/path` is a network-path reference to another host, and
/// dot segments of a relative reference are removed.
#[tokio::test]
async fn a_location_is_resolved_as_a_uri_reference() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let port = fx.port;
    let client = fx.client_for(
        &[
            &format!("{}/team", fx.at(REGISTRY)),
            &format!("{}/team", fx.at(OTHER)),
        ],
        true,
    );
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    for (to, want) in [
        (
            "/cdn/{digest}?u=https://x".to_owned(),
            format!("registry.test:{port} GET /cdn/"),
        ),
        (
            format!("//other.test:{port}/cdn/{{digest}}"),
            format!("other.test:{port} GET /cdn/"),
        ),
        (
            "../../../../cdn/{digest}".to_owned(),
            format!("registry.test:{port} GET /cdn/"),
        ),
    ] {
        fx.behave(|b| b.redirect_blobs = Some(to.clone()));
        let mut sink = Recording::default();
        repo.fetch_blob(&manifest.layers()[0], &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{to}: {e}"));
        assert_eq!(sink.bytes, layers()[0], "{to}");
        let seen = fx.requests();
        let last = seen.last().expect("request");
        assert!(last.starts_with(&want), "{to}: {last}");
    }
}

/// Proves a `#` in a token realm cannot swallow the scope: the fragment is dropped, and
/// the token request carries the service and scope.
#[tokio::test]
async fn a_fragment_in_the_realm_does_not_swallow_the_scope() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.realm = Some(format!("https://registry.test:{}/token#frag", fx.port));
    });
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    repo.fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    let request = fx
        .requests()
        .into_iter()
        .find(|r| r.contains("GET /token"))
        .expect("token request");
    assert!(
        request.contains("scope=repository:team/bundle:pull"),
        "{request}"
    );
}

/// Proves a fetch is bounded as a whole and not only per wait: with the per-wait limits
/// at their defaults, a manifest or a blob whose total limit is zero fails with a
/// time-limit error and not a stall, and the sink is never finished.
#[tokio::test]
async fn a_fetch_that_outlasts_its_time_limit_fails() {
    let artifact = bundle();
    let kind = Kind::bundle();
    let fx = started(&artifact).await;
    let zero = Timeouts {
        manifest: Duration::ZERO,
        ..Timeouts::default()
    };
    let client = fx.client().with_timeouts(zero);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect_err("manifest limit");
    assert!(
        matches!(&err, Error::Transport { why, .. } if why.contains("time limit")),
        "{err}"
    );

    let manifest = {
        let client = fx.client();
        let repo = client.repository(fx.location(REPO)).expect("repository");
        repo.fetch_manifest(&artifact.digest, &kind)
            .await
            .expect("manifest")
    };
    let zero = Timeouts {
        blob: Duration::ZERO,
        ..Timeouts::default()
    };
    let client = fx.client().with_timeouts(zero);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    fx.behave(|b| b.stall_body = true);
    let mut sink = Recording::default();
    let err = repo
        .fetch_blob(&manifest.layers()[0], &mut sink)
        .await
        .expect_err("blob limit");
    assert!(err.to_string().contains("time limit"), "{err}");
    assert!(!err.to_string().contains("stalled"), "{err}");
    assert!(!sink.finished);
    let err = repo
        .fetch_config(&manifest, &kind)
        .await
        .expect_err("config uses the blob limit");
    assert!(err.to_string().contains("time limit"), "{err}");
}

/// Proves a manifest hands out what its checks covered: the exact bytes it was pinned by,
/// its digest and artifact type, and each descriptor's media type, digest and size.
#[tokio::test]
async fn a_manifest_exposes_its_verified_bytes_and_descriptors() {
    let artifact = bundle();
    let fx = started(&artifact).await;
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    assert_eq!(manifest.bytes(), artifact.manifest.as_slice());
    assert_eq!(Digest::of(manifest.bytes()), *manifest.digest());
    assert_eq!(*manifest.digest(), artifact.digest);
    let config = manifest.config();
    assert_eq!(
        (config.media_type(), config.size()),
        (
            "application/vnd.komira.js.bundle.config.v1+json",
            artifact.config.len() as u64
        )
    );
    assert_eq!(*config.digest(), Digest::of(&artifact.config));
    let seen: Vec<(String, u64)> = manifest
        .layers()
        .iter()
        .map(|l| (l.digest().to_string(), l.size()))
        .collect();
    let want: Vec<(String, u64)> = layers()
        .iter()
        .map(|l| (Digest::of(l).to_string(), l.len() as u64))
        .collect();
    assert_eq!(seen, want);
    assert_eq!(
        manifest.layers()[0].media_type(),
        "application/vnd.komira.js.bundle.layer.v1.tar+zstd"
    );
}

/// Proves the roots are reported: certificates that cannot be read are counted and left
/// out, and a client with none that can be read fails every request and says how many
/// it could not read.
#[tokio::test]
async fn unreadable_roots_are_counted_and_no_roots_is_an_error() {
    let fx = Fixture::start().await;
    let junk = rustls::pki_types::CertificateDer::from(vec![1u8, 2, 3]);
    let policy = || Policy::new(vec![]);
    let mixed = Client::with_roots(policy(), vec![fx.cert.clone(), junk.clone()]);
    let summary = mixed.roots().expect("roots");
    assert_eq!((summary.loaded, summary.unreadable), (1, 1));
    let none = Client::with_roots(policy(), vec![]);
    let err = none.roots().expect_err("no roots");
    assert!(
        err.to_string()
            .contains("no trusted root certificates (0 could not be read)"),
        "{err}"
    );
    let only_junk = Client::with_roots(policy(), vec![junk.clone(), junk]);
    let err = only_junk.roots().expect_err("only junk");
    assert!(
        err.to_string()
            .contains("no trusted root certificates (2 could not be read)"),
        "{err}"
    );
}
