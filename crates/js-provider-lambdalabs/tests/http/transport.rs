//! Failures below HTTP: nobody listening, nobody answering, an answer cut short.

use std::time::Duration;

use js_provider_lambdalabs::{ApiError, ApiKey, HttpApi, LambdaApi};

use crate::reads::client;
use crate::support::{Reply, Scratch, Server, hangup_url};

#[test]
fn a_server_that_hangs_up_is_a_transport_failure() {
    let scratch = Scratch::new("transport-hangup");
    let key = ApiKey::from_file(&scratch.key_file("k")).unwrap();
    let api = HttpApi::connect(&hangup_url(), &key, None, Duration::from_secs(10)).unwrap();
    assert!(matches!(api.list_instances(), Err(ApiError::Transport(_))));
}

#[test]
fn a_server_that_never_answers_fails_at_the_limit() {
    let server = Server::plain();
    server.on("GET /api/v1/instances", Reply::Hang);
    let scratch = Scratch::new("transport-hang");
    let key = ApiKey::from_file(&scratch.key_file("k")).unwrap();
    let api =
        HttpApi::connect(&server.url("http"), &key, None, Duration::from_millis(300)).unwrap();
    let err = api.list_instances().unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
    // A failed list is an error, never an empty list.
    assert!(api.list_instances().is_err());
}

#[test]
fn an_answer_cut_short_is_a_transport_failure() {
    let server = Server::plain();
    server.on(
        "GET /api/v1/instances",
        Reply::Raw(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 500\r\nconnection: close\r\n\r\n{\"data\": ["
                .to_vec(),
        ),
    );
    let err = client(&server, "k").list_instances().unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
}
