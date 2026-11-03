//! Credentials for a registry's token realm, against registries in this process: a
//! private pull works with them, and they reach the registry's own token realm and
//! nothing else (no other host, no manifest or blob, no redirect target, no plain http).

mod support;

use js_oci::{Allow, BlobSink, BundleSource, Client, Credentials, Error, Kind, Location, Policy};
use support::{Artifact, Fixture, LOOPBACK, OTHER, REGISTRY, REPO};

/// The user and password of the tests, and the header they make (worked out by hand:
/// base64 of `puller:s3cret-pass-1`).
const USER: &str = "puller";
const PASSWORD: &str = "s3cret-pass-1";
const BASIC: &str = "Basic cHVsbGVyOnMzY3JldC1wYXNzLTE=";

/// What the token request of a scope-less test asks for.
const TOKEN_QUERY: &str = "service=fake-registry&scope=repository:team/bundle:pull";

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

fn credentials() -> Credentials {
    Credentials::new(USER, PASSWORD).expect("credentials")
}

fn bundle() -> Artifact {
    Artifact::bundle(vec![b"first layer".to_vec(), vec![7u8; 5000]])
}

/// A registry over TLS holding `artifact` whose realm issues the token `tok`, and only
/// to a request that carries [`BASIC`].
async fn private_registry(artifact: &Artifact) -> Fixture {
    let fx = Fixture::start().await;
    fx.publish(artifact);
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_basic = Some(BASIC.to_owned());
    });
    fx
}

/// The entry `<registry.test:port>/<prefix>`, reachable at the fixture's loopback address.
fn entry(fx: &Fixture, prefix: &str) -> Allow {
    Allow::parse(&format!("{}/{prefix}", fx.at(REGISTRY)))
        .expect("entry")
        .private()
        .loopback()
}

/// The manifest of `artifact` from `location`, through a repository of its own (so a
/// token is asked for again).
async fn manifest(client: &Client, location: Location, artifact: &Artifact) -> Result<(), Error> {
    let repo = client.repository(location)?;
    repo.fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .map(|_| ())
}

/// Proves a private pull works: the realm, which issues a token only to a known user,
/// is sent the credentials, and the manifest, the config and every layer are pulled by
/// digest with the token it issued. Catches credentials never sent, sent in a wrong
/// header, or sent to the registry in place of the token.
#[tokio::test]
async fn private_pull_succeeds_with_credentials() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let client = fx.client_with(vec![entry(&fx, "team").with_credentials(credentials())]);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let kind = Kind::bundle();
    let manifest = repo
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let config = repo.fetch_config(&manifest, &kind).await.expect("config");
    assert_eq!(config, artifact.config);
    for (layer, want) in manifest.layers().iter().zip(&artifact.layers) {
        let mut sink = Keep::default();
        repo.fetch_blob(layer, &mut sink).await.expect("layer");
        assert_eq!(&sink.0, want);
    }
    let (host, digest) = (fx.at(REGISTRY), &artifact.digest);
    let blob = |d: &js_oci::Digest| format!("{host} GET /v2/{REPO}/blobs/{d} [Bearer tok]");
    let mut want = vec![
        format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
        format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
        format!("{host} GET /v2/{REPO}/manifests/{digest} [Bearer tok]"),
        blob(manifest.config().digest()),
    ];
    want.extend(manifest.layers().iter().map(|l| blob(l.digest())));
    assert_eq!(fx.requests(), want);
}

/// Proves the credentials stay on the registry's host: a realm the policy accepts (the
/// host of another entry) on another host is asked anonymously even when it wants basic
/// auth, so the pull fails there. Catches credentials sent to whatever realm passes the
/// realm checks.
#[tokio::test]
async fn credentials_not_sent_to_a_realm_on_another_host() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let elsewhere = format!("https://{}/token", fx.at(OTHER));
    fx.behave(|b| b.realm = Some(elsewhere.clone()));
    let other = Allow::parse(&format!("{}/elsewhere", fx.at(OTHER)))
        .expect("entry")
        .private()
        .loopback();
    let client = fx.client_with(vec![
        entry(&fx, "team").with_credentials(credentials()),
        other,
    ]);
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("the other host's realm wants basic auth");
    assert_eq!(
        err.to_string(),
        format!("{elsewhere}: the registry answered 401")
    );
    let digest = &artifact.digest;
    assert_eq!(
        fx.requests(),
        [
            format!("{} GET /v2/{REPO}/manifests/{digest} []", fx.at(REGISTRY)),
            format!("{} GET /token?{TOKEN_QUERY} []", fx.at(OTHER)),
        ]
    );
}

/// Proves manifests and blobs never carry the credentials: every request but the token
/// request has the bearer token or nothing, a blob the registry redirects to a CDN path
/// included. Catches the credentials sent with every request of an entry that has them.
#[tokio::test]
async fn credentials_not_sent_to_manifest_or_blob_requests() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let cdn = format!("https://{}/cdn/{{digest}}", fx.at(OTHER));
    fx.behave(|b| b.redirect_blobs = Some(cdn));
    let other = Allow::parse(&format!("{}/elsewhere", fx.at(OTHER)))
        .expect("entry")
        .private()
        .loopback();
    let client = fx.client_with(vec![
        entry(&fx, "team").with_credentials(credentials()),
        other,
    ]);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    let mut sink = Keep::default();
    let layer = &manifest.layers()[0];
    repo.fetch_blob(layer, &mut sink).await.expect("layer");
    assert_eq!(sink.0, artifact.layers[0]);
    let (host, digest) = (fx.at(REGISTRY), &artifact.digest);
    let layer = layer.digest();
    assert_eq!(
        fx.requests(),
        [
            format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
            format!("{host} GET /v2/{REPO}/manifests/{digest} [Bearer tok]"),
            format!("{host} GET /v2/{REPO}/blobs/{layer} [Bearer tok]"),
            format!("{} GET /cdn/{layer} []", fx.at(OTHER)),
        ]
    );
}

/// Proves a redirect from the token realm drops the credentials, even to a path of the
/// same host: the target, which also wants basic auth, is asked without them and the
/// pull fails. Catches the `Authorization` of the token request kept across a redirect.
#[tokio::test]
async fn credentials_not_sent_after_a_redirect() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    fx.behave(|b| b.redirect_token = Some("/issued".to_owned()));
    let client = fx.client_with(vec![entry(&fx, "team").with_credentials(credentials())]);
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("the redirect target wants basic auth");
    let host = fx.at(REGISTRY);
    assert_eq!(
        err.to_string(),
        format!("https://{host}/token: the registry answered 401")
    );
    let digest = &artifact.digest;
    assert_eq!(
        fx.requests(),
        [
            format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
            format!("{host} GET /issued []"),
        ]
    );
}

/// Proves an entry pulled over plain http never sends its credentials: its own token
/// realm, at the same loopback authority, is asked without them and refuses. Catches the
/// credentials sent in clear because the realm is the registry's own.
#[tokio::test]
async fn credentials_not_sent_over_plain_http() {
    let artifact = bundle();
    let fx = Fixture::start_plain().await;
    fx.publish(&artifact);
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_basic = Some(BASIC.to_owned());
    });
    let authority = format!("127.0.0.1:{}", fx.port);
    let plain = Allow::parse(&format!("{authority}/team"))
        .expect("entry")
        .loopback()
        .plain_http_loopback()
        .with_credentials(credentials());
    let client = Client::with_roots(Policy::new(vec![plain]), Vec::new());
    let location = Location::parse(&format!("{authority}/{REPO}")).expect("location");
    let err = manifest(&client, location, &artifact)
        .await
        .expect_err("the realm wants basic auth");
    assert_eq!(
        err.to_string(),
        format!("http://{authority}/token: the registry answered 401")
    );
    let digest = &artifact.digest;
    assert_eq!(
        fx.requests(),
        [
            format!("{authority} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{authority} GET /token?{TOKEN_QUERY} []"),
        ]
    );
}

/// Proves an entry without credentials pulls as it did: its token request carries
/// nothing (and succeeds at an anonymous realm), and a realm that wants basic auth
/// refuses it. Catches credentials of one entry used for another, or a header sent with
/// nothing in it.
#[tokio::test]
async fn anonymous_registry_is_unaffected() {
    let artifact = bundle();
    let fx = Fixture::start().await;
    fx.publish(&artifact);
    fx.behave(|b| b.token = Some("tok".to_owned()));
    let other = Allow::parse(&format!("{}/elsewhere", fx.at(OTHER)))
        .expect("entry")
        .private()
        .loopback()
        .with_credentials(credentials());
    let client = fx.client_with(vec![entry(&fx, "team"), other]);
    manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect("an anonymous pull");
    let token_line = format!("{} GET /token?{TOKEN_QUERY} []", fx.at(REGISTRY));
    assert_eq!(fx.requests()[1], token_line);

    fx.behave(|b| b.token_basic = Some(BASIC.to_owned()));
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("the realm wants basic auth");
    assert!(matches!(err, Error::Status { status: 401, .. }), "{err}");
    assert_eq!(fx.requests()[4], token_line);
}

/// The `Authorization` of the last token request the fixture saw, between the brackets
/// of its log line.
fn last_token_authorization(fx: &Fixture) -> String {
    let line = fx
        .requests()
        .into_iter()
        .rfind(|line| line.contains(" GET /token?"))
        .expect("a token request");
    let (_, auth) = line.split_once(" [").expect("brackets");
    auth.trim_end_matches(']').to_owned()
}

/// Proves which entry's credentials a repository uses: the most specific entry's (the
/// longest prefix), whether it has them or not, and of several entries of that prefix
/// only credentials they all carry. Catches the credentials of a broader entry used for
/// a narrower one that has none, and one of two disagreeing entries winning by order.
#[tokio::test]
async fn credentials_follow_the_most_specific_entry() {
    let artifact = bundle();
    let fx = Fixture::start().await;
    fx.publish(&artifact);
    fx.behave(|b| b.token = Some("tok".to_owned()));
    let with = |prefix: &str| entry(&fx, prefix).with_credentials(credentials());
    let without = |prefix: &str| entry(&fx, prefix);
    for (case, allow, want) in [
        ("broad has them", vec![with("team"), without(REPO)], ""),
        ("narrow has them", vec![without("team"), with(REPO)], BASIC),
        ("both have them", vec![with(REPO), with(REPO)], BASIC),
        ("first has them", vec![with(REPO), without(REPO)], ""),
        ("second has them", vec![without(REPO), with(REPO)], ""),
    ] {
        let client = fx.client_with(allow);
        manifest(&client, fx.location(REPO), &artifact)
            .await
            .expect(case);
        assert_eq!(last_token_authorization(&fx), want, "{case}");
    }
}

/// Proves nothing a caller prints holds the user, the password or the header: the
/// `Debug` of the credentials, of an entry and policy that carry them, of a client and
/// a repository, and the error and its `Debug` when the realm refuses them. Catches a
/// derived `Debug` on the credentials, and a realm's refusal that echoes the request.
#[tokio::test]
async fn credentials_do_not_appear_in_debug_or_errors() {
    assert_eq!(format!("{:?}", credentials()), "Credentials(<redacted>)");
    let allow = Allow::parse("registry.example/team")
        .expect("entry")
        .with_credentials(credentials());
    assert_eq!(
        format!("{allow:?}"),
        "Allow { authority: Authority { host: \"registry.example\", port: 443 }, \
         prefix: \"team\", private: false, loopback: false, plain_http: false, \
         port_named: false, credentials: Some(Credentials(<redacted>)) }"
    );

    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let wrong = Credentials::new(USER, "wrong-pass").expect("credentials");
    let policy = Policy::new(vec![entry(&fx, "team").with_credentials(wrong)]);
    let client =
        Client::with_roots(policy.clone(), vec![fx.cert.clone()]).with_resolved(REGISTRY, LOOPBACK);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let err = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect_err("the realm refuses the wrong password");
    let url = format!("https://{}/token", fx.at(REGISTRY));
    assert_eq!(err.to_string(), format!("{url}: the registry answered 401"));
    assert_eq!(
        format!("{err:?}"),
        format!("Status {{ url: {url:?}, status: 401 }}")
    );
    // The wrong password did reach the realm, so the test looks for the right thing.
    assert_eq!(
        last_token_authorization(&fx),
        "Basic cHVsbGVyOndyb25nLXBhc3M="
    );
    let printed = [
        format!("{policy:?}"),
        format!("{client:?}"),
        format!("{repo:?}"),
    ];
    for text in printed {
        for secret in [USER, "wrong-pass", "cHVsbGVy", "Basic"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(
            text.contains("Credentials(<redacted>)") || text.starts_with("Repository"),
            "{text}"
        );
    }
}
