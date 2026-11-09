//! A well-formed client gets the answer the server has for it, on both kinds of listener:
//! when it shuts its sending side once its request is sent, and when the route answers
//! without reading the body. The HTTP/1.1 tests run over an in-memory pipe and queue the
//! whole of what the client sends (its requests, and its half-close when it makes one)
//! before the server reads a byte, so the order is the test's, not the scheduler's.

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::http::StatusCode;
use axum::routing::post;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

use super::*;
use crate::api::BODY_LIMIT;

/// Room in the pipe for everything a test sends and everything the server answers, so
/// neither side waits on the other to write.
const PIPE: usize = 512 * 1024;

/// How many bytes the route [`part`] reads before it answers.
const PART: usize = 1000;

/// A route that refuses before it reads the body, as the hello limit does.
async fn refuse() -> StatusCode {
    StatusCode::TOO_MANY_REQUESTS
}

/// A route that refuses a token before it reads the body, as the job port's bearer layer
/// does.
async fn deny() -> StatusCode {
    StatusCode::UNAUTHORIZED
}

/// A route that reads the body and says how long it was, or refuses with the status of
/// the failed read.
async fn measure(body: Result<Bytes, BytesRejection>) -> (StatusCode, String) {
    match body {
        Ok(body) => (StatusCode::OK, format!("read {}", body.len())),
        Err(rejection) => (rejection.status(), "refused".to_string()),
    }
}

/// A route that reads at least [`PART`] bytes of the body and answers without the rest.
async fn part(body: Body) -> &'static str {
    use tonic::codegen::tokio_stream::StreamExt;
    let mut stream = body.into_data_stream();
    let mut read = 0;
    while read < PART {
        read += stream.next().await.unwrap().unwrap().len();
    }
    "partly read"
}

/// The routes of the tests that do not need the server's own.
fn app() -> Router {
    Router::new()
        .route("/refuse", post(refuse))
        .route("/deny", post(deny))
        .route("/measure", post(measure))
        .route("/part", post(part))
}

/// One request as the client writes it: `path` POSTed with `body` under a declared length
/// of `declared`, with `extra` header lines, closing the connection after its answer when
/// `last`.
fn request(path: &str, body: &[u8], declared: usize, extra: &str, last: bool) -> Vec<u8> {
    let close = if last { "Connection: close\r\n" } else { "" };
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: t\r\n{close}{extra}Content-Length: {declared}\r\n\r\n"
    );
    let mut message = head.into_bytes();
    message.extend_from_slice(body);
    message
}

/// The pipe's client end with `sent` written on it, and its sending side shut when
/// `shut`; and the server end.
async fn sent(sent: &[u8], shut: bool) -> (DuplexStream, Io) {
    let (mut client, server) = duplex(PIPE);
    client.write_all(sent).await.unwrap();
    if shut {
        client.shutdown().await.unwrap();
    }
    (client, Box::pin(server))
}

/// Serves the server end with `app` over `speech` and reads the client end to its end: all
/// the answers, as text.
async fn answers(speech: Speech, client: DuplexStream, server: Io, app: Router) -> String {
    answers_under(Limits::default(), speech, client, server, app).await
}

/// [`answers`], with the connection held to `limits`.
async fn answers_under(
    limits: Limits,
    speech: Speech,
    mut client: DuplexStream,
    server: Io,
    app: Router,
) -> String {
    let (_stop, stopping) = watch::channel(());
    let served = async {
        match speech {
            Speech::Http1 => talk_http1(server, app, limits, stopping).await,
            Speech::Both => {
                let (_cut, cut) = watch::channel(());
                let gate = Arc::new(Semaphore::new(1));
                let permit = Arc::new(gate.try_acquire_owned().unwrap());
                let streams = Streams { permit, cut };
                talk_both(server, app, limits, stopping, streams).await;
            }
        }
    };
    let mut raw = Vec::new();
    let read = async { client.read_to_end(&mut raw).await.unwrap() };
    tokio::join!(served, read);
    String::from_utf8(raw).unwrap()
}

/// The status line and the body of each of `answers`, in order: each body is as long as
/// its `content-length` says.
fn split(mut answers: &str) -> Vec<(&str, &str)> {
    let mut split = Vec::new();
    while let Some((head, rest)) = answers.split_once("\r\n\r\n") {
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map_or(0, |length| length.parse().unwrap());
        let (body, next) = rest.split_at(length);
        split.push((head.lines().next().unwrap(), body));
        answers = next;
    }
    assert_eq!(answers, "", "bytes after the last answer");
    split
}

/// The status lines of `answers`, in order.
fn statuses(answers: &str) -> Vec<&str> {
    split(answers)
        .into_iter()
        .map(|(status, _)| status)
        .collect()
}

/// The body of the last of `answers`.
fn last_body(answers: &str) -> &str {
    split(answers).last().map_or("", |(_, body)| body)
}

/// Asserts that a route that answers before it reads the body (the hello limit's 429)
/// reaches a client that sent its whole request and shut its sending side, on the
/// HTTP/1.1 listener and on the listener that also speaks HTTP/2. The server sees the
/// body end and the end of the client's sending side before the route has answered.
/// Catches a connection that takes that end for the client giving up and closes with no
/// answer: the empty reply a test of the hello limit read as status 0.
#[tokio::test]
async fn a_refusal_before_the_body_reaches_a_client_that_half_closed() {
    for speech in [Speech::Http1, Speech::Both] {
        let message = request("/refuse", b"{\"host_id\":\"h\"}", 15, "", true);
        let (client, server) = sent(&message, true).await;
        let answers = answers(speech, client, server, app()).await;
        let expected = ["HTTP/1.1 429 Too Many Requests"];
        assert_eq!(statuses(&answers), expected, "{speech:?}: {answers:?}");
    }
}

/// The agent routes over a pool of four hosts, with their files under `dir`: the pool's
/// join token is forty `j`s, and a credential may say five hellos in a burst.
fn agent_routes(dir: &std::path::Path) -> Router {
    use js_registry::{PoolConfig, Registry, SequentialSessions};
    use js_types::{ManualClock, Timestamp};
    use js_types::{ProviderId, ResourceClass, Resources, Shape};

    let clock = ManualClock::new(Timestamp::from_millis(5_000_000));
    let shared = crate::adapt::SharedClock::new(clock.clone());
    let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a100".to_owned())).unwrap();
    let arch = ("arch".to_owned(), "x86_64".to_owned());
    let shape = Shape::new(resources, [arch].into_iter().collect());
    let pool = PoolConfig::new(
        ProviderId::new("burst").unwrap(),
        ResourceClass::gpu_host(),
        shape,
        4,
    );
    let placer = Arc::new(js_sched_placer::place::SchedPlacer::default());
    let registry = Registry::with_sessions(pool, clock, SequentialSessions::new(), placer);
    let agents = crate::agents::Agents::over(registry, &shared, dir);
    crate::agent_api::router(Arc::new(agents))
}

/// Asserts that a hello refused at its rate limit, with its body, reaches a client that
/// sent its whole request and shut its sending side, through the agent routes: once the
/// pool token has spent its burst of five, the client reads the 429 `rate-limited` with
/// its `Retry-After`. Catches the close with no answer that a test of the hello limit read
/// as status 0.
#[tokio::test]
async fn a_hello_over_its_rate_limit_reaches_a_client_that_half_closed() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/listener_unit");
    let dir = js_testkit::dir::TestDir::under(root, "hello_half_close");
    let app = agent_routes(dir.path());
    let token = format!("Authorization: Bearer {}\r\n", "j".repeat(40));
    let hello = br#"{"protocol":1,"host_id":"half-1"}"#;
    let message = request("/v1/agent/hello", hello, hello.len(), &token, true);
    for _ in 0..5 {
        let (client, server) = sent(&message, false).await;
        let spent = answers(Speech::Http1, client, server, app.clone()).await;
        assert_eq!(statuses(&spent), ["HTTP/1.1 400 Bad Request"], "{spent:?}");
    }
    let (client, server) = sent(&message, true).await;
    let refused = answers(Speech::Http1, client, server, app).await;
    assert_eq!(
        statuses(&refused),
        ["HTTP/1.1 429 Too Many Requests"],
        "{refused:?}"
    );
    assert!(refused.contains("\r\nretry-after: 1\r\n"), "{refused:?}");
    assert_eq!(last_body(&refused), r#"{"error":"rate-limited"}"#);
}

/// Asserts that a route that reads the whole body answers a client that sent its whole
/// request and shut its sending side, on both listeners: the body arrives whole and the
/// answer says so. Catches the same close for a route that reads the body.
#[tokio::test]
async fn a_route_that_reads_the_body_answers_a_client_that_half_closed() {
    for speech in [Speech::Http1, Speech::Both] {
        let message = request("/measure", b"0123456789", 10, "", true);
        let (client, server) = sent(&message, true).await;
        let answers = answers(speech, client, server, app()).await;
        assert_eq!(
            statuses(&answers),
            ["HTTP/1.1 200 OK"],
            "{speech:?}: {answers:?}"
        );
        assert_eq!(last_body(&answers), "read 10", "{speech:?}");
    }
}

/// Asserts that a client that shuts its sending side in the middle of its body (it
/// declared more than it sent) is refused, not served a short body: the route's read of
/// the body fails and the client gets the 400 of that failure. Catches a half-close that
/// is taken for the end of the body.
#[tokio::test]
async fn a_body_cut_short_by_a_half_close_is_not_taken_whole() {
    for speech in [Speech::Http1, Speech::Both] {
        let message = request("/measure", b"0123", 10, "", true);
        let (client, server) = sent(&message, true).await;
        let answers = answers(speech, client, server, app()).await;
        let expected = ["HTTP/1.1 400 Bad Request"];
        assert_eq!(statuses(&answers), expected, "{speech:?}: {answers:?}");
        assert_eq!(last_body(&answers), "refused", "{speech:?}");
    }
}

/// Asserts that what a route leaves unread of a body is read before the answer goes, up to
/// the body limit: a request answered before its body is read, or after part of it, is
/// followed on the same connection by a second request, which is answered when what was
/// left of the first body was at most the limit; when far more was left, the connection
/// closes after the first answer and the second is not answered. The bodies are larger
/// than the server reads at once, so it cannot have read them in passing. Catches a server
/// that leaves the body unread, which closes the connection on bytes it never read (the
/// kernel then resets it, and the reset can cost the client the answer), and one that
/// reads on without a limit.
#[tokio::test]
async fn an_answer_before_the_body_reads_it_up_to_the_limit() {
    let cases = [
        ("/refuse", BODY_LIMIT / 2, 2),
        ("/refuse-expecting", BODY_LIMIT / 2, 2),
        ("/refuse", BODY_LIMIT, 2),
        ("/refuse", 4 * BODY_LIMIT, 1),
        ("/part", BODY_LIMIT, 2),
        ("/part", 4 * BODY_LIMIT, 1),
    ];
    for speech in [Speech::Http1, Speech::Both] {
        for (path, size, answered) in cases {
            // An `Expect` other than `100-continue` asks nothing of the server: its client
            // sends the body at once, and it is read like any other.
            let (path, extra) = match path {
                "/refuse-expecting" => ("/refuse", "Expect: something-else\r\n"),
                _ => (path, ""),
            };
            let mut message = request(path, &vec![b' '; size], size, extra, false);
            message.extend(request("/measure", b"abc", 3, "", true));
            let (client, server) = sent(&message, false).await;
            let answers = answers(speech, client, server, app()).await;
            let first = match path {
                "/refuse" => "HTTP/1.1 429 Too Many Requests",
                _ => "HTTP/1.1 200 OK",
            };
            let expected = [first, "HTTP/1.1 200 OK"];
            let seen = statuses(&answers);
            assert_eq!(
                seen,
                expected[..answered],
                "{speech:?} {path} {size}: {answers:?}"
            );
        }
    }
}

/// Asserts that the rest of a body its route did not read is waited for no longer than a
/// head is: a client refused before its body is read, which sent part of the body and then
/// nothing more without closing, gets the refusal once the head's time has passed, long
/// before the read limit would cut it, on both listeners. The clock is tokio's paused test
/// clock, which moves only when every task waits, so the order of the two times is fixed,
/// not raced. Catches a wait on the rest of the body bounded only by the read limit, which
/// lets a client with no token hold a connection by trickling a body.
#[tokio::test(start_paused = true)]
async fn the_rest_of_a_body_is_waited_for_only_as_long_as_a_head() {
    let limits = Limits {
        header_timeout: Duration::from_secs(10),
        read_timeout: Duration::from_secs(3600),
        ..Limits::default()
    };
    for speech in [Speech::Http1, Speech::Both] {
        let message = request("/refuse", b"0123", 10, "", true);
        let (client, server) = sent(&message, false).await;
        let answered = answers_under(limits, speech, client, server, app());
        let cut_long_before = tokio::time::sleep(limits.read_timeout / 2);
        let answers = tokio::select! {
            answers = answered => answers,
            () = cut_long_before => panic!("{speech:?}: no answer while the body was awaited"),
        };
        let expected = ["HTTP/1.1 429 Too Many Requests"];
        assert_eq!(statuses(&answers), expected, "{speech:?}: {answers:?}");
    }
}

/// Asserts that a request that waits to be told to send its body (`Expect: 100-continue`),
/// and is refused before its body is read, gets the refusal and no `100 Continue`: the
/// client sends no body and keeps its sending side open, and the answer is the 429 alone.
/// Catches a server that reads on such a body, which tells the client to send it and then
/// waits for a body that does not come.
#[tokio::test]
async fn a_refused_request_that_expects_continue_is_not_asked_for_its_body() {
    for speech in [Speech::Http1, Speech::Both] {
        let expect = "Expect: 100-continue\r\n";
        let message = request("/refuse", b"", 10, expect, true);
        let (client, server) = sent(&message, false).await;
        let answers = answers(speech, client, server, app()).await;
        let expected = ["HTTP/1.1 429 Too Many Requests"];
        assert_eq!(statuses(&answers), expected, "{speech:?}: {answers:?}");
    }
}

/// Asserts that an HTTP/2 request refused before its body is read is answered while its
/// client is still sending: a gRPC call whose stream of messages never ends, refused with a
/// 401 by its route, ends in `Unauthenticated` (the 401's gRPC status). The head's time is
/// set far above the read limit, so a server that waits for the end of an HTTP/2 body
/// before answering holds the call until the read limit cuts the connection, and the call
/// ends in another status. Catches that wait.
#[tokio::test]
async fn an_http2_request_refused_before_its_body_is_answered_while_it_sends() {
    use tonic::client::Grpc;
    use tonic::codegen::http::uri::PathAndQuery;
    use tonic::codegen::tokio_stream;
    use tonic::transport::Endpoint;
    use tonic_prost::ProstCodec;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let limits = Limits {
        header_timeout: Duration::from_secs(3600),
        read_timeout: Duration::from_secs(1),
        ..Limits::default()
    };
    let slots = Arc::new(Slots::new(limits.max_connections));
    let (stop, stopped) = oneshot::channel();
    let served = tokio::spawn(serve_both(listener, None, app(), limits, slots, stopped));
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = Grpc::new(channel);
    grpc.ready().await.unwrap();
    let path = PathAndQuery::from_static("/deny");
    let request = tonic::Request::new(tokio_stream::pending::<()>());
    let called = grpc.client_streaming::<_, (), (), _>(request, path, ProstCodec::default());
    let status = called.await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    drop(grpc);
    stop.send(()).unwrap();
    served.await.unwrap();
}
