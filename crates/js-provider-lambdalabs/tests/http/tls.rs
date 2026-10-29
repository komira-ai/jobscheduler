//! The https path: trust comes from the bundle the caller names, and only from it.

use js_provider_lambdalabs::{ApiError, ApiKey, HttpApi, LambdaApi};

use crate::support::{Cert, PATIENT, Reply, Scratch, Server, fixture};

fn api(server: &Server, name: &str, trusted: &Cert) -> HttpApi {
    let scratch = Scratch::new(name);
    let key = ApiKey::from_file(&scratch.key_file("tls-key")).unwrap();
    let bundle = scratch.write("roots.pem", trusted.pem.as_bytes(), 0o644);
    HttpApi::connect(&server.url("https"), &key, Some(&bundle), PATIENT).unwrap()
}

#[test]
fn a_server_with_the_trusted_certificate_is_read() {
    let cert = Cert::new();
    let server = Server::tls(&cert);
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances.json")),
    );
    let list = api(&server, "tls-ok", &cert).list_instances().unwrap();
    assert_eq!(list.len(), 7);
    assert_eq!(
        server.seen()[0].header("authorization"),
        Some("Bearer tls-key")
    );
}

#[test]
fn a_server_with_another_certificate_is_refused_before_the_key_is_sent() {
    let server = Server::tls(&Cert::new());
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances.json")),
    );
    let err = api(&server, "tls-wrong", &Cert::new())
        .list_instances()
        .unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
    assert!(server.seen().is_empty());
}

#[test]
fn an_https_client_does_not_fall_back_to_plain_http() {
    let cert = Cert::new();
    let server = Server::plain();
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances.json")),
    );
    let scratch = Scratch::new("tls-downgrade");
    let key = ApiKey::from_file(&scratch.key_file("tls-key")).unwrap();
    let bundle = scratch.write("roots.pem", cert.pem.as_bytes(), 0o644);
    let url = server.url("https");
    let api = HttpApi::connect(&url, &key, Some(&bundle), PATIENT).unwrap();
    assert!(matches!(api.list_instances(), Err(ApiError::Transport(_))));
    assert!(server.seen().is_empty());
}

#[test]
fn with_no_bundle_a_self_signed_server_is_refused_before_the_key_is_sent() {
    // The bundled public roots do not hold a test certificate, so a client given no
    // bundle refuses this server: the default is a root set, not "trust anything".
    let server = Server::tls(&Cert::new());
    server.on(
        "GET /api/v1/instances",
        Reply::json(200, &fixture("instances.json")),
    );
    let scratch = Scratch::new("tls-no-bundle");
    let key = ApiKey::from_file(&scratch.key_file("tls-key")).unwrap();
    let api = HttpApi::connect(&server.url("https"), &key, None, PATIENT).unwrap();
    assert!(matches!(api.list_instances(), Err(ApiError::Transport(_))));
    assert!(server.seen().is_empty());
}
