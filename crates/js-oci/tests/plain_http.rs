//! The plain-http loopback entry against registries in this process: what it pulls over
//! http, and every way the flag must not widen what the client reaches.

mod support;

use js_oci::{Allow, BlobSink, BundleSource, Client, Error, Kind, Location, Policy};
use support::{Artifact, Fixture, LOOPBACK, REGISTRY, REPO};

/// A sink that keeps the bytes.
#[derive(Default)]
struct Keep(Vec<u8>);

#[async_trait::async_trait]
impl BlobSink for Keep {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.0.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bundle() -> Artifact {
    Artifact::bundle(vec![b"first layer".to_vec(), vec![7u8; 5000]])
}

/// A registry over plain http holding `artifact`.
async fn plain(artifact: &Artifact) -> Fixture {
    let fx = Fixture::start_plain().await;
    fx.publish(artifact);
    fx
}

/// The flagged entry for `fx` as `127.0.0.1`, with the loopback exemption its address
/// needs.
fn flagged(fx: &Fixture) -> Allow {
    Allow::parse(&format!("127.0.0.1:{}/team", fx.port))
        .expect("entry")
        .loopback()
        .plain_http_loopback()
}

fn client(allow: Vec<Allow>) -> Client {
    Client::with_roots(Policy::new(allow), Vec::new())
        .with_resolved(REGISTRY, LOOPBACK)
        .with_resolved("localhost", LOOPBACK)
}

fn here(fx: &Fixture) -> Location {
    Location::parse(&format!("127.0.0.1:{}/{REPO}", fx.port)).expect("location")
}

/// Pulls the manifest of `artifact` and its first layer through `client`.
async fn pull_first(
    client: &Client,
    location: Location,
    artifact: &Artifact,
) -> Result<Vec<u8>, Error> {
    let repo = client.repository(location)?;
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await?;
    let mut sink = Keep::default();
    repo.fetch_blob(&manifest.layers()[0], &mut sink).await?;
    Ok(sink.0)
}

/// Proves the flag does what it is for: an entry for `127.0.0.1:PORT` pulls manifest,
/// config and layers over plain http with no certificate, answers a token challenge at
/// the same authority over http, and follows a redirect to the same authority without
/// credentials.
#[tokio::test]
async fn a_flagged_loopback_entry_is_pulled_over_plain_http() {
    let artifact = bundle();
    let fx = plain(&artifact).await;
    let client = client(vec![flagged(&fx)]);
    let repo = client.repository(here(&fx)).expect("repository");
    let kind = Kind::bundle();
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    assert_eq!(
        repo.fetch_config(&manifest, &kind).await.expect("config"),
        artifact.config
    );
    let mut layer = Keep::default();
    repo.fetch_blob(&manifest.layers()[1], &mut layer)
        .await
        .expect("layer");
    assert_eq!(layer.0, vec![7u8; 5000]);
    let host = format!("127.0.0.1:{}", fx.port);
    assert!(
        fx.requests().iter().all(|r| r.starts_with(&host)),
        "{:?}",
        fx.requests()
    );

    fx.behave(|b| b.token = Some("tok".to_owned()));
    let bytes = pull_first(&client, here(&fx), &artifact)
        .await
        .expect("with a token");
    assert_eq!(bytes, b"first layer");
    let tokens = fx
        .requests()
        .into_iter()
        .filter(|r| r.contains("GET /token"));
    assert_eq!(tokens.count(), 1);

    for to in [
        "/cdn/{digest}".to_owned(),
        format!("http://127.0.0.1:{}/cdn/{{digest}}", fx.port),
    ] {
        fx.behave(|b| b.redirect_blobs = Some(to));
        let bytes = pull_first(&client, here(&fx), &artifact)
            .await
            .expect("through the redirect");
        assert_eq!(bytes, b"first layer");
    }
    let log = fx.requests();
    let cdn = log.iter().rev().find(|r| r.contains("/cdn/")).expect("cdn");
    assert!(cdn.ends_with("[]"), "the target got credentials: {cdn}");
}

/// Proves the flag is honoured only for a loopback IP literal: on a host name that
/// resolves to loopback, `localhost`, or another loopback address, the entry stays https
/// (a TLS registry is pulled as before and a plain one is not reached over http).
#[tokio::test]
async fn the_flag_on_a_name_changes_nothing_and_a_plain_registry_is_not_reached() {
    let artifact = bundle();
    let tls = Fixture::start_named(
        ["registry.test", "localhost", "127.0.0.1"]
            .map(str::to_owned)
            .to_vec(),
    )
    .await;
    tls.publish(&artifact);
    for name in [REGISTRY, "localhost"] {
        let entry = Allow::parse(&format!("{name}:{}/team", tls.port))
            .expect("entry")
            .loopback()
            .plain_http_loopback();
        let client = Client::with_roots(Policy::new(vec![entry]), vec![tls.cert.clone()])
            .with_resolved(REGISTRY, LOOPBACK)
            .with_resolved("localhost", LOOPBACK);
        let location = Location::parse(&format!("{name}:{}/{REPO}", tls.port)).expect("location");
        let bytes = pull_first(&client, location, &artifact)
            .await
            .expect("still https");
        assert_eq!(bytes, b"first layer", "{name}");
    }

    let fx = plain(&artifact).await;
    for name in [REGISTRY, "localhost"] {
        let entry = Allow::parse(&format!("{name}:{}/team", fx.port))
            .expect("entry")
            .loopback()
            .plain_http_loopback();
        let client = client(vec![entry]);
        let location = Location::parse(&format!("{name}:{}/{REPO}", fx.port)).expect("location");
        let err = pull_first(&client, location, &artifact)
            .await
            .expect_err(name);
        assert!(matches!(err, Error::Transport { .. }), "{name}: {err}");
    }
    let other = Allow::parse(&format!("127.0.0.2:{}/team", fx.port))
        .expect("entry")
        .loopback()
        .plain_http_loopback();
    let location = Location::parse(&format!("127.0.0.2:{}/{REPO}", fx.port)).expect("location");
    pull_first(&client(vec![other]), location, &artifact)
        .await
        .expect_err("another loopback address");
    assert!(fx.requests().is_empty(), "{:?}", fx.requests());
}

/// Proves a loopback http pull needs the flag: the same entry without it speaks https,
/// and the plain registry sees no request.
#[tokio::test]
async fn without_the_flag_a_loopback_http_pull_is_refused() {
    let artifact = bundle();
    let fx = plain(&artifact).await;
    let entry = Allow::parse(&format!("127.0.0.1:{}/team", fx.port))
        .expect("entry")
        .loopback();
    let err = pull_first(&client(vec![entry]), here(&fx), &artifact)
        .await
        .expect_err("no flag");
    assert!(matches!(err, Error::Transport { .. }), "{err}");
    assert!(fx.requests().is_empty(), "{:?}", fx.requests());
}

/// Proves a redirect off the exact authority is refused before any connection: another
/// port, another loopback address, a host name (even one that resolves to loopback), an
/// https upgrade of the same host and of another host, a private address.
#[tokio::test]
async fn a_redirect_off_the_authority_is_refused() {
    let artifact = bundle();
    let fx = plain(&artifact).await;
    let other = plain(&artifact).await;
    let client = client(vec![flagged(&fx)]);
    let port = fx.port;
    for to in [
        format!("http://127.0.0.1:{}/cdn/{{digest}}", other.port),
        format!("http://127.0.0.2:{port}/cdn/{{digest}}"),
        format!("http://[::1]:{port}/cdn/{{digest}}"),
        format!("http://registry.test:{port}/cdn/{{digest}}"),
        format!("http://localhost:{port}/cdn/{{digest}}"),
        format!("http://10.0.0.1:{port}/cdn/{{digest}}"),
        format!("https://127.0.0.1:{port}/cdn/{{digest}}"),
        format!("https://registry.test:{port}/cdn/{{digest}}"),
        "https://example.com/cdn/{digest}".to_owned(),
        "http://127.0.0.1/cdn/{digest}".to_owned(),
    ] {
        fx.behave(|b| b.redirect_blobs = Some(to.clone()));
        let err = pull_first(&client, here(&fx), &artifact)
            .await
            .expect_err(&to);
        assert!(matches!(err, Error::PlainHttp(_)), "{to}: {err}");
    }
    assert!(
        fx.requests().iter().all(|r| !r.contains("/cdn/")),
        "{:?}",
        fx.requests()
    );
    assert!(other.requests().is_empty(), "{:?}", other.requests());
}

/// Proves a token realm is held to the same rule: only plain http at the registry's own
/// authority, not another port, another loopback address, a name, an https URL, nor the
/// authority of another listed entry.
#[tokio::test]
async fn a_token_realm_off_the_authority_is_refused() {
    let artifact = bundle();
    let fx = plain(&artifact).await;
    let other = plain(&artifact).await;
    let policy = Policy::new(vec![flagged(&fx), flagged(&other)])
        .allow_realm("auth.example")
        .expect("realm");
    let client = Client::with_roots(policy, Vec::new()).with_resolved(REGISTRY, LOOPBACK);
    let port = fx.port;
    for realm in [
        format!("http://127.0.0.1:{}/token", other.port),
        format!("http://127.0.0.2:{port}/token"),
        format!("http://registry.test:{port}/token"),
        format!("http://localhost:{port}/token"),
        format!("https://127.0.0.1:{port}/token"),
        "https://auth.example/token".to_owned(),
        "http://auth.example/token".to_owned(),
    ] {
        fx.behave(|b| {
            b.token = Some("tok".to_owned());
            b.realm = Some(realm.clone());
        });
        let err = pull_first(&client, here(&fx), &artifact)
            .await
            .expect_err(&realm);
        assert!(matches!(err, Error::Realm(_)), "{realm}: {err}");
    }
    assert!(
        fx.requests().iter().all(|r| !r.contains("GET /token")),
        "{:?}",
        fx.requests()
    );
    assert!(other.requests().is_empty(), "{:?}", other.requests());
}

/// Proves the flag stays on its entry: an https entry beside a flagged one is pulled
/// over TLS, cannot be sent by redirect or token realm to the flagged authority over
/// http, and a sibling entry for the same address without the flag is not pulled over
/// http.
#[tokio::test]
async fn the_flag_on_an_entry_does_not_affect_other_entries() {
    let artifact = bundle();
    let tls = Fixture::start().await;
    tls.publish(&artifact);
    let fx = plain(&artifact).await;
    let tls_entry = Allow::parse(&format!("{}/team", tls.at(REGISTRY)))
        .expect("entry")
        .private()
        .loopback();
    let sibling = Allow::parse(&format!("127.0.0.1:{}/other", fx.port))
        .expect("entry")
        .loopback();
    let client = Client::with_roots(
        Policy::new(vec![flagged(&fx), tls_entry, sibling]),
        vec![tls.cert.clone()],
    )
    .with_resolved(REGISTRY, LOOPBACK);

    assert_eq!(
        pull_first(&client, tls.location(REPO), &artifact)
            .await
            .expect("tls entry"),
        b"first layer"
    );
    let plain_pull = pull_first(&client, here(&fx), &artifact).await;
    assert_eq!(plain_pull.expect("flagged entry"), b"first layer");
    let before = fx.requests().len();

    tls.behave(|b| b.redirect_blobs = Some(format!("http://127.0.0.1:{}/cdn/x", fx.port)));
    let err = pull_first(&client, tls.location(REPO), &artifact)
        .await
        .expect_err("redirect to the flagged authority");
    assert!(matches!(err, Error::Insecure(_)), "{err}");

    tls.behave(|b| {
        b.redirect_blobs = None;
        b.token = Some("tok".to_owned());
        b.realm = Some(format!("http://127.0.0.1:{}/token", fx.port));
    });
    let err = pull_first(&client, tls.location(REPO), &artifact)
        .await
        .expect_err("realm at the flagged authority");
    assert!(matches!(err, Error::Realm(_)), "{err}");

    let sibling_pin = Location::parse(&format!("127.0.0.1:{}/other/bundle", fx.port)).expect("pin");
    let err = pull_first(&client, sibling_pin, &artifact)
        .await
        .expect_err("sibling entry");
    assert!(matches!(err, Error::Transport { .. }), "{err}");
    assert_eq!(fx.requests().len(), before, "{:?}", fx.requests());
}
