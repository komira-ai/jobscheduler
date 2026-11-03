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

/// The requests `fx` saw after the first `before`.
fn since(fx: &Fixture, before: usize) -> Vec<String> {
    fx.requests().split_off(before)
}

/// Proves a redirect from the token realm drops the credentials wherever it goes and
/// never gets them back: to a path of the same host, to another host, and through
/// another host back to the registry's own; and that a redirect to plain http is refused
/// before it is followed. Each target also wants basic auth, so it refuses. Catches the
/// `Authorization` of the token request kept across a redirect, restored when a chain
/// returns to the registry, or sent in clear.
#[tokio::test]
async fn credentials_not_sent_after_a_redirect() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let other = Allow::parse(&format!("{}/elsewhere", fx.at(OTHER)))
        .expect("entry")
        .private()
        .loopback();
    let client = fx.client_with(vec![
        entry(&fx, "team").with_credentials(credentials()),
        other,
    ]);
    let (host, there, digest) = (fx.at(REGISTRY), fx.at(OTHER), &artifact.digest);
    let first = vec![
        format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
        format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
    ];
    let refused = format!("https://{host}/token: the registry answered 401");
    let hop = |from: &str, to: String| (from.to_owned(), to);
    let cases = [
        (
            "the same host",
            vec![hop("/token", "/issued".to_owned())],
            vec![format!("{host} GET /issued []")],
            refused.clone(),
        ),
        (
            "another host",
            vec![hop("/token", format!("https://{there}/issued"))],
            vec![format!("{there} GET /issued []")],
            refused.clone(),
        ),
        (
            "back to the registry",
            vec![
                hop("/token", format!("https://{there}/hop")),
                hop("/hop", format!("https://{host}/issued")),
            ],
            vec![
                format!("{there} GET /hop []"),
                format!("{host} GET /issued []"),
            ],
            refused.clone(),
        ),
        (
            "plain http",
            vec![hop("/token", format!("http://{host}/issued"))],
            vec![],
            format!("http://{host}/issued is not https"),
        ),
    ];
    for (case, redirects, after, want) in cases {
        fx.behave(|b| b.redirects = redirects);
        let before = fx.requests().len();
        let err = manifest(&client, fx.location(REPO), &artifact)
            .await
            .expect_err(case);
        assert_eq!(err.to_string(), want, "{case}");
        let mut log = first.clone();
        log.extend(after);
        assert_eq!(since(&fx, before), log, "{case}");
    }
}

/// Proves the credentials follow the registry's own realm and nothing that only looks
/// like it: a realm that spells the registry's host in capitals is the registry's (the
/// authority is compared as the connection sees it) and gets them; the same host on
/// another port, though the policy lists it, is asked anonymously; a realm the policy
/// refuses (plain http, an unlisted host, a host allowed as a realm whose address is
/// refused, a host whose certificate is not trusted) fails before any request reaches
/// it; and a challenge for basic auth on the registry itself is refused. Catches a realm
/// compared by its text, by host without port, or credentials sent before the realm's
/// checks.
#[tokio::test]
async fn credentials_follow_only_the_registrys_own_realm() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let side = Fixture::start().await;
    side.behave(|b| {
        b.token = Some("tok".to_owned());
        b.token_basic = Some(BASIC.to_owned());
    });
    let untrusted = Fixture::start().await;
    let (host, port, digest) = (fx.at(REGISTRY), fx.port, &artifact.digest);
    let listed = |fixture: &Fixture| {
        Allow::parse(&format!("{}/elsewhere", fixture.at(REGISTRY)))
            .expect("entry")
            .private()
            .loopback()
    };
    let policy = Policy::new(vec![
        entry(&fx, "team").with_credentials(credentials()),
        listed(&side),
        listed(&untrusted),
    ])
    .allow_realm(&fx.at(OTHER))
    .expect("realm host");
    let client = Client::with_roots(policy, vec![fx.cert.clone(), side.cert.clone()])
        .with_resolved(REGISTRY, LOOPBACK)
        .with_resolved(OTHER, LOOPBACK)
        .with_resolved("unlisted.test", LOOPBACK);
    let manifest_line = format!("{host} GET /v2/{REPO}/manifests/{digest} []");

    fx.behave(|b| b.realm = Some(format!("https://REGISTRY.TEST:{port}/token")));
    manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect("the registry's own realm, in capitals");
    assert_eq!(
        fx.requests()[..2],
        [
            manifest_line.clone(),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]")
        ]
    );

    let side_realm = format!("https://{}/token", side.at(REGISTRY));
    fx.behave(|b| b.realm = Some(side_realm.clone()));
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("another port wants basic auth");
    assert_eq!(
        err.to_string(),
        format!("{side_realm}: the registry answered 401")
    );
    assert_eq!(
        side.requests(),
        [format!("{} GET /token?{TOKEN_QUERY} []", side.at(REGISTRY))]
    );

    let realm_refusal = |realm: &str| {
        format!("the token realm {realm} is not the registry's host or an allowed host")
    };
    let plain = format!("http://{host}/token");
    let unlisted = format!("https://unlisted.test:{port}/token");
    let address = format!("https://{}/token", fx.at(OTHER));
    for (realm, want) in [
        (plain.clone(), realm_refusal(&plain)),
        (unlisted.clone(), realm_refusal(&unlisted)),
        (
            address,
            "other.test resolves to 127.0.0.1, a loopback address".to_owned(),
        ),
    ] {
        fx.behave(|b| b.realm = Some(realm.clone()));
        let before = fx.requests().len();
        let err = manifest(&client, fx.location(REPO), &artifact)
            .await
            .expect_err(&realm);
        assert_eq!(err.to_string(), want, "{realm}");
        assert_eq!(
            since(&fx, before),
            std::slice::from_ref(&manifest_line),
            "{realm}"
        );
    }

    fx.behave(|b| b.realm = Some(format!("https://{}/token", untrusted.at(REGISTRY))));
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("an untrusted certificate");
    assert!(matches!(err, Error::Transport { .. }), "{err}");
    assert!(
        untrusted.requests().is_empty(),
        "{:?}",
        untrusted.requests()
    );

    fx.behave(|b| {
        b.realm = None;
        b.challenge_scheme = Some("Basic".to_owned());
    });
    let before = fx.requests().len();
    let err = manifest(&client, fx.location(REPO), &artifact)
        .await
        .expect_err("a basic challenge");
    assert_eq!(
        err.to_string(),
        "registry authentication: the registry asks for \"Basic\" authentication; only \
         Bearer tokens are supported"
    );
    assert_eq!(since(&fx, before), [manifest_line]);
}

/// Proves the token the realm issued for the credentials is kept and renewed: a later
/// request reuses it, a request the registry later refuses it for gets a new one from the
/// realm with the same credentials, and a registry that refuses the token it just issued
/// ends the pull with that said. Catches a token asked for at every request, a renewal
/// made without the credentials, and a loop of renewals.
#[tokio::test]
async fn the_token_of_the_credentials_is_reused_and_renewed() {
    let artifact = bundle();
    let fx = private_registry(&artifact).await;
    let client = fx.client_with(vec![entry(&fx, "team").with_credentials(credentials())]);
    let repo = client.repository(fx.location(REPO)).expect("repository");
    let manifest = repo
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    let (host, digest) = (fx.at(REGISTRY), &artifact.digest);
    let layers = manifest.layers();
    let mut sink = Keep::default();
    repo.fetch_blob(&layers[0], &mut sink)
        .await
        .expect("first layer");
    fx.behave(|b| b.token = Some("tok2".to_owned()));
    let before = fx.requests().len();
    let mut sink = Keep::default();
    repo.fetch_blob(&layers[1], &mut sink)
        .await
        .expect("second layer");
    assert_eq!(sink.0, artifact.layers[1]);
    let blob = format!("{host} GET /v2/{REPO}/blobs/{}", layers[1].digest());
    assert_eq!(
        since(&fx, before),
        [
            format!("{blob} [Bearer tok]"),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
            format!("{blob} [Bearer tok2]"),
        ]
    );
    assert_eq!(
        fx.requests()[..5],
        [
            format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
            format!("{host} GET /v2/{REPO}/manifests/{digest} [Bearer tok]"),
            format!(
                "{host} GET /v2/{REPO}/blobs/{} [Bearer tok]",
                layers[0].digest()
            ),
            format!("{blob} [Bearer tok]"),
        ],
        "the first token served both the manifest and the first layer"
    );

    fx.behave(|b| b.refuse_token = true);
    let before = fx.requests().len();
    let fresh = client.repository(fx.location(REPO)).expect("repository");
    let err = fresh
        .fetch_manifest(&artifact.digest, &Kind::bundle())
        .await
        .expect_err("the registry refuses its token");
    let url = format!("https://{host}/v2/{REPO}/manifests/{digest}");
    assert_eq!(
        err.to_string(),
        format!("registry authentication: {url}: the registry refused the token it issued")
    );
    assert_eq!(
        since(&fx, before),
        [
            format!("{host} GET /v2/{REPO}/manifests/{digest} []"),
            format!("{host} GET /token?{TOKEN_QUERY} [{BASIC}]"),
            format!("{host} GET /v2/{REPO}/manifests/{digest} [Bearer tok2]"),
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
    let other = |prefix: &str| {
        entry(&fx, prefix).with_credentials(Credentials::new("other", "pw-2").expect("credentials"))
    };
    for (case, allow, want) in [
        ("broad has them", vec![with("team"), without(REPO)], ""),
        ("narrow has them", vec![without("team"), with(REPO)], BASIC),
        ("both have them", vec![with(REPO), with(REPO)], BASIC),
        ("first has them", vec![with(REPO), without(REPO)], ""),
        ("second has them", vec![without(REPO), with(REPO)], ""),
        ("they differ, ours first", vec![with(REPO), other(REPO)], ""),
        (
            "they differ, ours second",
            vec![other(REPO), with(REPO)],
            "",
        ),
    ] {
        let client = fx.client_with(allow);
        manifest(&client, fx.location(REPO), &artifact)
            .await
            .expect(case);
        assert_eq!(last_token_authorization(&fx), want, "{case}");
    }
}

/// Proves sibling repositories of one registry, pulled at once through one client, each
/// send only their own entry's credentials: `team/a` and `team/b` carry different ones,
/// and `team/ab` (a neighbour of `team/a` by text, not by path component) falls under the
/// broad `team` entry, which has none. Catches credentials taken by text prefix, and one
/// repository's credentials used for another's token request while both are in flight.
#[tokio::test]
async fn sibling_repositories_send_only_their_own_credentials() {
    let artifact = bundle();
    let fx = Fixture::start().await;
    fx.publish(&artifact);
    fx.behave(|b| {
        b.token = Some("tok".to_owned());
        b.challenge_no_scope = true;
    });
    let alpha = Credentials::new("alpha", "pw-a").expect("credentials");
    let beta = Credentials::new("beta", "pw-b").expect("credentials");
    let client = fx.client_with(vec![
        entry(&fx, "team"),
        entry(&fx, "team/a").with_credentials(alpha),
        entry(&fx, "team/b").with_credentials(beta),
    ]);
    let at =
        |repo: &str| Location::parse(&format!("{}/{repo}", fx.at(REGISTRY))).expect("location");
    let (a, b, ab) = tokio::join!(
        manifest(&client, at("team/a/app"), &artifact),
        manifest(&client, at("team/b/app"), &artifact),
        manifest(&client, at("team/ab/app"), &artifact),
    );
    for pulled in [a, b, ab] {
        pulled.expect("a pull");
    }
    let host = fx.at(REGISTRY);
    let mut tokens: Vec<String> = fx
        .requests()
        .into_iter()
        .filter(|line| line.contains(" GET /token?"))
        .collect();
    tokens.sort();
    let token = |repo: &str, auth: &str| {
        format!("{host} GET /token?service=fake-registry&scope=repository:{repo}:pull [{auth}]")
    };
    assert_eq!(
        tokens,
        [
            token("team/a/app", "Basic YWxwaGE6cHctYQ=="),
            token("team/ab/app", ""),
            token("team/b/app", "Basic YmV0YTpwdy1i"),
        ]
    );
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
    let port = fx.port;
    let allow_shown = format!(
        "Allow {{ authority: Authority {{ host: \"registry.test\", port: {port} }}, prefix: \
         \"team\", private: true, loopback: true, plain_http: false, port_named: true, \
         credentials: Some(Credentials(<redacted>)) }}"
    );
    let policy_shown = format!("Policy {{ allow: [{allow_shown}], realms: [] }}");
    assert_eq!(format!("{policy:?}"), policy_shown);
    assert_eq!(
        format!("{client:?}"),
        format!(
            "Client {{ http: Http {{ policy: {policy_shown}, resolve: {{\"registry.test\": \
             127.0.0.1}}, timeouts: Timeouts {{ connect: 30s, head: 60s, idle: 60s, manifest: \
             300s, blob: 7200s }}, .. }} }}"
        )
    );
    let authority = format!("Authority {{ host: \"registry.test\", port: {port} }}");
    assert_eq!(
        format!("{repo:?}"),
        format!(
            "Repository {{ location: Location {{ registry: {authority}, endpoint: {authority}, \
             repository: \"team/bundle\" }}, .. }}"
        )
    );
}
