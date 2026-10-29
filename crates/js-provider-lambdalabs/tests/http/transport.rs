//! Failures below HTTP: nobody listening, nobody answering, an answer cut short.

use std::sync::mpsc;
use std::thread;
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

/// How long a test waits for a call that is bound by a short limit before it calls the
/// limit broken. Far above the limit, so only a client with no limit meets it.
const DEADLINE: Duration = Duration::from_secs(20);

#[test]
fn a_server_that_never_answers_fails_at_the_limit() {
    let server = Server::plain();
    server.on("GET /api/v1/instances", Reply::Hang);
    let scratch = Scratch::new("transport-hang");
    let key = ApiKey::from_file(&scratch.key_file("k")).unwrap();
    let api =
        HttpApi::connect(&server.url("http"), &key, None, Duration::from_millis(300)).unwrap();
    // The call runs on its own thread so that a client with no limit fails this test at
    // the deadline instead of hanging it; the thread ends when the server drops its end.
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let first = api.list_instances();
        // A failed list is an error, never an empty list.
        let second = api.list_instances();
        let _ = send.send((first, second));
    });
    let (first, second) = receive
        .recv_timeout(DEADLINE)
        .expect("the call outlived the client's limit");
    assert!(matches!(first, Err(ApiError::Transport(_))), "{first:?}");
    assert!(second.is_err());
}

#[test]
fn an_answer_over_the_size_limit_is_a_transport_failure() {
    let server = Server::plain();
    // Valid JSON, padded: a client that read it all would take it for an empty list.
    let body = format!("{{\"data\": []}}{}", " ".repeat(8 * 1024 * 1024));
    server.on("GET /api/v1/instances", Reply::json(200, &body));
    let err = client(&server, "k").list_instances().unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)), "{err:?}");
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
