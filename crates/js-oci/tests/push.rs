//! Publishing an artifact from an image-layout directory to registries served in memory (the
//! fixture's [`Network`]: no socket is bound or connected): what a publish sends and in what
//! order, that the same manifest under its tag again sends nothing, that a tag naming another
//! manifest is never moved, and every answer of a registry a publish must not go on from.

mod support;

use js_oci::{
    Allow, BundleSource, Credentials, Digest, Error, Kind, Layout, Published, Repository, Tag,
};
use js_testkit::dir::TestDir;
use support::{Artifact, Fixture, Network, REGISTRY, REPO, write_layout};

/// The user and password of the tests, and the header they make (base64 of
/// `pusher:s3cret-push-1`, worked out by hand).
const USER: &str = "pusher";
const PASSWORD: &str = "s3cret-push-1";
const BASIC: &str = "Basic cHVzaGVyOnMzY3JldC1wdXNoLTE=";

/// What a publish's token request asks for: the repository's pull and push scope, whatever
/// the challenge names.
const PUSH_QUERY: &str = "service=fake-registry&scope=repository:team/bundle:pull%2Cpush";

/// A bundle of two layers.
fn bundle(marker: u8) -> Artifact {
    Artifact::bundle(vec![vec![marker; 11], vec![7u8; 5000]])
}

fn tag(text: &str) -> Tag {
    Tag::parse(text).expect("tag")
}

/// `artifact` written as a layout under a scratch directory (kept alive by the returned
/// guard), and the layout opened.
async fn layout(name: &str, artifact: &Artifact) -> (TestDir, Layout) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    write_layout(&dir, artifact);
    let layout = Layout::open(dir.path()).await.expect("layout");
    (dir, layout)
}

/// A registry in memory whose token realm issues `tok`.
fn registry() -> Fixture {
    let fx = Fixture::in_memory();
    fx.behave(|b| b.token = Some("tok".to_owned()));
    fx
}

/// The requests from the `before`th on.
fn since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests().split_off(before)
}

/// Publishes `artifact`, read from `layout`, under `tag` through a repository of its own.
async fn publish(
    fx: &Fixture,
    layout: &Layout,
    artifact: &Artifact,
    under: &str,
) -> Result<Published, Error> {
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    repo.publish(layout, &artifact.digest, &Kind::bundle(), &tag(under))
        .await
}

/// Proves a publish sends, in this order, the tag's lookup with a token asked for with the
/// pull and push scope, one upload (start and `PUT` with its digest) of the config and of
/// each layer, and the manifest's `PUT` under the tag; and that the registry then serves the
/// manifest, byte for byte, under the tag and under its digest, and every blob. Catches a
/// manifest sent before its blobs, a pull token used for a write, a blob sent to the wrong
/// URL or with no digest, and bytes changed on the way.
#[tokio::test]
async fn a_layout_is_published_under_its_tag_and_pulls_back_by_digest() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-layout", &artifact).await;
    let fx = registry();
    let published = publish(&fx, &layout, &artifact, "1")
        .await
        .expect("publish");
    assert_eq!(published, Published::Pushed);

    let host = fx.at(REGISTRY);
    let upload = |n: usize, blob: &[u8]| {
        [
            format!("{host} POST /v2/{REPO}/blobs/uploads/ [Bearer tok]"),
            format!(
                "{host} PUT /v2/{REPO}/blobs/uploads/{n}?digest={} [Bearer tok]",
                Digest::of(blob)
            ),
        ]
    };
    let mut want = vec![
        format!("{host} GET /v2/{REPO}/manifests/1 []"),
        format!("{host} GET /token?{PUSH_QUERY} []"),
        format!("{host} GET /v2/{REPO}/manifests/1 [Bearer tok]"),
    ];
    want.extend(upload(4, &artifact.config));
    want.extend(upload(6, &artifact.layers[0]));
    want.extend(upload(8, &artifact.layers[1]));
    want.push(format!("{host} PUT /v2/{REPO}/manifests/1 [Bearer tok]"));
    assert_eq!(fx.requests(), want);

    let media = "application/vnd.oci.image.manifest.v1+json".to_owned();
    let stored = (media, artifact.manifest.clone());
    assert_eq!(fx.manifest("1"), Some(stored.clone()));
    assert_eq!(fx.manifest(artifact.digest.as_str()), Some(stored));
    for blob in [&artifact.config].into_iter().chain(&artifact.layers) {
        assert_eq!(fx.blob(&Digest::of(blob)).as_ref(), Some(blob));
    }

    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let kind = Kind::bundle();
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    assert_eq!(manifest.bytes(), artifact.manifest.as_slice());
    let config = repo.fetch_config(&manifest, &kind).await.expect("config");
    assert_eq!(config, artifact.config);
}

/// Proves the publish is idempotent: the same manifest under its tag a second time asks the
/// registry what the tag names and sends nothing else. Catches a publish that uploads again
/// or puts the manifest again.
#[tokio::test]
async fn the_same_manifest_under_its_tag_again_sends_only_the_lookup() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-again", &artifact).await;
    let fx = registry();
    publish(&fx, &layout, &artifact, "1").await.expect("first");
    let before = fx.requests().len();
    let again = publish(&fx, &layout, &artifact, "1").await.expect("again");
    assert_eq!(again, Published::AlreadyThere);
    let host = fx.at(REGISTRY);
    assert_eq!(
        since(&fx, before),
        [
            format!("{host} GET /v2/{REPO}/manifests/1 []"),
            format!("{host} GET /token?{PUSH_QUERY} []"),
            format!("{host} GET /v2/{REPO}/manifests/1 [Bearer tok]"),
        ]
    );
}

/// Proves a tag is never moved: a different manifest under a tag that names one already is
/// refused, with both digests in the error, nothing is uploaded, and the tag still names the
/// first; under a new tag it is published. Catches a publish that overwrites a tag.
#[tokio::test]
async fn a_tag_that_names_another_manifest_is_refused_and_left_as_it_is() {
    let first = bundle(1);
    let second = bundle(2);
    let (_one, first_layout) = layout("js-oci-push-first", &first).await;
    let (_two, second_layout) = layout("js-oci-push-second", &second).await;
    let fx = registry();
    publish(&fx, &first_layout, &first, "1")
        .await
        .expect("first");
    let before = fx.requests().len();
    let refused = publish(&fx, &second_layout, &second, "1")
        .await
        .expect_err("a moved tag");
    assert_eq!(
        refused.to_string(),
        format!(
            "the tag 1 names {}, not {}: a tag is never moved to another manifest; publish under a new tag",
            first.digest, second.digest
        )
    );
    assert!(matches!(refused, Error::TagHeld { .. }));
    let host = fx.at(REGISTRY);
    assert_eq!(
        since(&fx, before).last(),
        Some(&format!("{host} GET /v2/{REPO}/manifests/1 [Bearer tok]"))
    );
    assert_eq!(
        fx.manifest("1").map(|(_, b)| b),
        Some(first.manifest.clone())
    );

    let fresh = publish(&fx, &second_layout, &second, "2")
        .await
        .expect("new tag");
    assert_eq!(fresh, Published::Pushed);
    assert_eq!(fx.manifest("2").map(|(_, b)| b), Some(second.manifest));
}

/// Proves the push token is asked for with the entry's credentials, at the registry's own
/// realm, and that the writes carry the token it issued and never the credentials. Catches
/// credentials left off the push's token request, or sent with an upload.
#[tokio::test]
async fn the_push_token_is_asked_for_with_the_entrys_credentials() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-credentials", &artifact).await;
    let fx = registry();
    fx.behave(|b| b.token_basic = Some(BASIC.to_owned()));
    let credentials = Credentials::new(USER, PASSWORD).expect("credentials");
    let entry = Allow::parse(&format!("{}/team", fx.at(REGISTRY)))
        .expect("entry")
        .private()
        .loopback()
        .with_credentials(credentials);
    let client = fx.client_with(vec![entry]);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let published = repo
        .publish(&layout, &artifact.digest, &Kind::bundle(), &tag("1"))
        .await
        .expect("publish");
    assert_eq!(published, Published::Pushed);
    let requests = fx.requests();
    let host = fx.at(REGISTRY);
    assert_eq!(
        requests[1],
        format!("{host} GET /token?{PUSH_QUERY} [{BASIC}]")
    );
    let writes: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains(" POST ") || r.contains(" PUT "))
        .collect();
    assert_eq!(writes.len(), 7);
    assert!(
        writes.iter().all(|r| r.ends_with("[Bearer tok]")),
        "{writes:?}"
    );
}

/// Proves a token asked for to pull is not sent with a publish: a repository that pulled
/// first asks for a token again, with the push scope, before its lookup carries one. Catches
/// a write sent with a token that cannot write.
#[tokio::test]
async fn a_pull_token_is_not_sent_with_a_publish() {
    let pulled = bundle(3);
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-after-pull", &artifact).await;
    let fx = registry();
    fx.publish(&pulled);
    let client = fx.client();
    let repo: Repository<'_> = client.repository(fx.location(REPO)).expect("repository");
    repo.fetch_manifest(&pulled.digest, &Kind::bundle())
        .await
        .expect("pull");
    let before = fx.requests().len();
    repo.publish(&layout, &artifact.digest, &Kind::bundle(), &tag("1"))
        .await
        .expect("publish");
    let host = fx.at(REGISTRY);
    assert_eq!(
        since(&fx, before)[..2],
        [
            format!("{host} GET /v2/{REPO}/manifests/1 []"),
            format!("{host} GET /token?{PUSH_QUERY} []"),
        ]
    );
}

/// Proves an upload's `Location` that carries a query of its own gets the digest after it
/// with `&`, and a relative one is resolved against the upload's start. Catches a second `?`
/// in the blob's URL.
#[tokio::test]
async fn an_upload_location_with_a_query_gets_the_digest_added_to_it() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-query", &artifact).await;
    let fx = registry();
    fx.behave(|b| b.upload_location = Some("u1?_state=abc".to_owned()));
    publish(&fx, &layout, &artifact, "1")
        .await
        .expect("publish");
    let host = fx.at(REGISTRY);
    let config = Digest::of(&artifact.config);
    assert_eq!(
        fx.requests()[4],
        format!("{host} PUT /v2/{REPO}/blobs/uploads/u1?_state=abc&digest={config} [Bearer tok]")
    );
}

/// Proves the token goes with no upload off the registry's own scheme, host and port: an
/// upload that would go on at another host, over plain http, at another port, or at no URL
/// at all is refused before anything is sent there, and so is an answer with no `Location`.
/// Catches the token and the bytes sent wherever a registry says.
#[tokio::test]
async fn an_upload_location_off_the_registry_is_refused_before_any_bytes_go_there() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-elsewhere", &artifact).await;
    let fx = registry();
    let port = fx.port;
    let start = format!("https://{}/v2/{REPO}/blobs/uploads/", fx.at(REGISTRY));
    for (location, shown) in [
        (
            format!("https://other.test:{port}/v2/{REPO}/blobs/uploads/9"),
            format!("https://other.test:{port}/v2/{REPO}/blobs/uploads/9"),
        ),
        (
            format!("http://registry.test:{port}/v2/{REPO}/blobs/uploads/9"),
            format!("http://registry.test:{port}/v2/{REPO}/blobs/uploads/9"),
        ),
        (
            format!("https://registry.test:{}/u", port + 1),
            format!("https://registry.test:{}/u", port + 1),
        ),
        (
            "/a b".to_owned(),
            format!("https://registry.test:{port}/a b"),
        ),
    ] {
        fx.behave(|b| b.upload_location = Some(location.clone()));
        let before = fx.requests().len();
        let refused = publish(&fx, &layout, &artifact, "1")
            .await
            .expect_err("refused");
        assert_eq!(
            refused.to_string(),
            format!("{start}: the upload goes on at {shown}, not on the registry's own host"),
            "{location}"
        );
        assert!(
            since(&fx, before).iter().all(|r| !r.contains(" PUT ")),
            "{location}"
        );
    }
    fx.behave(|b| b.upload_location = Some(String::new()));
    let refused = publish(&fx, &layout, &artifact, "1")
        .await
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!("{start}: the upload's answer has no Location")
    );
    assert!(matches!(refused, Error::Upload { .. }));
    assert_eq!(fx.manifest("1"), None);
}

/// Proves a registry that refuses writes fails the publish with its status, and that a tag
/// lookup that fails (not with 404) is not taken for an absent tag: nothing is uploaded.
/// Catches a refused write taken for a pushed one, and an error page taken for "no tag".
#[tokio::test]
async fn a_refused_write_or_lookup_fails_the_publish_with_the_registrys_status() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-refused", &artifact).await;
    let fx = registry();
    let host = fx.at(REGISTRY);
    fx.behave(|b| b.write_status = Some(403));
    let refused = publish(&fx, &layout, &artifact, "1")
        .await
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!("https://{host}/v2/{REPO}/blobs/uploads/: the registry answered 403")
    );

    let fx = registry();
    fx.behave(|b| b.status = Some(500));
    let refused = publish(&fx, &layout, &artifact, "1")
        .await
        .expect_err("refused");
    let host = fx.at(REGISTRY);
    assert_eq!(
        refused.to_string(),
        format!("https://{host}/v2/{REPO}/manifests/1: the registry answered 500")
    );
    assert!(fx.requests().iter().all(|r| !r.contains(" POST ")));
}

/// Proves a registry that says it stored the manifest as another digest fails the publish,
/// and one that says nothing of the digest does not. Catches a registry's rewrite of the
/// manifest (a different digest under the tag) taken for success.
#[tokio::test]
async fn a_manifest_stored_as_another_digest_is_refused() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-stored", &artifact).await;
    let other = format!("sha256:{}", "0".repeat(64));
    let fx = registry();
    fx.behave(|b| b.stored_digest = Some(other.clone()));
    let refused = publish(&fx, &layout, &artifact, "1")
        .await
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "https://{}/v2/{REPO}/manifests/1: the registry stored the manifest as \"{other}\", not {}",
            fx.at(REGISTRY),
            artifact.digest
        )
    );

    let fx = registry();
    fx.behave(|b| b.stored_digest = Some(String::new()));
    let published = publish(&fx, &layout, &artifact, "1")
        .await
        .expect("publish");
    assert_eq!(published, Published::Pushed);
}

/// Proves a publish checks the source before it asks the registry anything: a digest the
/// layout does not hold sends no request at all. Catches a publish of bytes nobody checked.
#[tokio::test]
async fn a_digest_the_layout_does_not_hold_sends_nothing() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-missing", &artifact).await;
    let fx = registry();
    let client = fx.client();
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let absent = bundle(9).digest;
    let refused = repo
        .publish(&layout, &absent, &Kind::bundle(), &tag("1"))
        .await;
    assert!(refused.is_err());
    assert_eq!(fx.requests(), Vec::<String>::new());
}

/// Proves a publish to a registry over plain http on loopback goes over http to that exact
/// authority, uploads included. Catches an upload refused because its `Location` is checked
/// against https alone.
#[tokio::test]
async fn a_publish_to_a_plain_http_loopback_registry_goes_over_http() {
    let artifact = bundle(1);
    let (_dir, layout) = layout("js-oci-push-plain", &artifact).await;
    let fx = Network::default().start_plain();
    let entry = Allow::parse(&format!("127.0.0.1:{}/team", fx.port))
        .expect("entry")
        .loopback()
        .plain_http_loopback();
    let client = js_oci::Client::with_roots(js_oci::Policy::new(vec![entry]), Vec::new())
        .with_connector(fx.network().connector());
    let location =
        js_oci::Location::parse(&format!("127.0.0.1:{}/{REPO}", fx.port)).expect("location");
    let repo = client.repository(location).expect("repository");
    let published = repo
        .publish(&layout, &artifact.digest, &Kind::bundle(), &tag("1"))
        .await
        .expect("publish");
    assert_eq!(published, Published::Pushed);
    assert_eq!(fx.manifest("1").map(|(_, b)| b), Some(artifact.manifest));
}

/// Proves a publish that finds a layout whose blob was altered stops at that blob: what
/// was sent before it stays, the manifest is not put. Catches a tag that names a manifest
/// whose blobs the registry does not hold.
#[tokio::test]
async fn an_altered_blob_in_the_layout_stops_the_publish_before_the_manifest() {
    let artifact = bundle(1);
    let (dir, layout) = layout("js-oci-push-altered", &artifact).await;
    let layer = support::blob_path(&dir, &Digest::of(&artifact.layers[1]));
    std::fs::write(&layer, vec![8u8; 5000]).expect("alter");
    let fx = registry();
    let refused = publish(&fx, &layout, &artifact, "1").await;
    assert!(refused.is_err());
    assert_eq!(fx.manifest("1"), None);
    assert!(fx.blob(&Digest::of(&artifact.config)).is_some());
}
