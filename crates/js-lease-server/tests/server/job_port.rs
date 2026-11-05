//! The job service's port (`--job-listen`) over real sockets on 127.0.0.1, port 0: gRPC and
//! the HTTP/JSON gateway on the one port, the bearer token in front of both, the lease
//! routes kept apart from it, TLS, and the stop. Every call of the job service answers
//! `UNIMPLEMENTED` until it is served; `GetJob` is served from the server's job store, and
//! these tests ask it for a job that does not exist, which the store answers `unknown-job`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::Uri;
use hyper_util::rt::TokioIo;
use js_lease::Timestamp;
use js_proto::v1::GetJobRequest;
use js_proto::v1::job_service_client::JobServiceClient;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

use crate::agents_support::start_agents;
use crate::job_listen::job_args;
use crate::support::{Harness, Scratch, TOKEN, guard, request};
use crate::tls::{connector, test_identity, tls_request};

/// The gateway's answer to `GetJob` of a job the store does not hold.
const UNKNOWN_JOB_BODY: &str = r#"{"error":"unknown-job"}"#;

/// A token that is not the server's.
const WRONG_TOKEN: &str = "wrong-token-0123456789-abcdefghijklmnop";

/// A gateway route of the job service: `GetJob` of `job-1`, which no test submits.
const GET_JOB: &str = "/v1/jobs/job-1";

/// A server with agent registration on and the job service on a loopback port.
async fn job_server(name: &str) -> Harness {
    let scratch = Scratch::new(name);
    let args = job_args(&scratch, "127.0.0.1:0");
    Harness::with(scratch, args, None).await
}

/// A gRPC client of the job service at `addr`, over plain HTTP/2.
async fn client(addr: SocketAddr) -> JobServiceClient<Channel> {
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    JobServiceClient::new(channel)
}

/// A TLS client that trusts exactly `trusted` and offers HTTP/2, then HTTP/1.1.
fn h2_connector(trusted: &str) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(trusted.as_bytes()).unwrap())
        .unwrap();
    let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    TlsConnector::from(Arc::new(config))
}

/// A gRPC client of the job service at `addr`, over TLS trusting exactly `trusted`.
async fn tls_client(addr: SocketAddr, trusted: &str) -> JobServiceClient<Channel> {
    let connector = h2_connector(trusted);
    let connect = tower::service_fn(move |_: Uri| {
        let connector = connector.clone();
        async move {
            let tcp = TcpStream::connect(addr).await?;
            let name = ServerName::IpAddress(addr.ip().into());
            let tls = connector.connect(name, tcp).await?;
            Ok::<_, std::io::Error>(TokioIo::new(tls))
        }
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_with_connector(connect)
        .await
        .unwrap();
    JobServiceClient::new(channel)
}

/// The protocol the TLS handshake with `addr` settled on, for a client that trusts
/// `trusted` and offers HTTP/2, then HTTP/1.1.
async fn negotiated(addr: SocketAddr, trusted: &str) -> Option<Vec<u8>> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::IpAddress(addr.ip().into());
    let stream = h2_connector(trusted).connect(name, tcp).await.unwrap();
    stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
}

/// The status of a `GetJob` call of `job-1` over `client`, with `token` as its bearer token
/// (none when `None`). No test submits that job, so the call fails and the status is its
/// answer.
async fn get_job(client: &mut JobServiceClient<Channel>, token: Option<&str>) -> Status {
    let mut call = tonic::Request::new(GetJobRequest {
        job_id: "job-1".to_owned(),
    });
    if let Some(token) = token {
        let value: MetadataValue<Ascii> = format!("Bearer {token}").parse().unwrap();
        call.metadata_mut().insert("authorization", value);
    }
    client
        .get_job(call)
        .await
        .expect_err("no test submits job-1")
}

/// Asserts that one address serves both transports: a tonic client's `GetJob` (HTTP/2)
/// reaches the gRPC service and an HTTP/1.1 `GET /v1/jobs/{id}` reaches the gateway, both
/// answering `unknown-job` from the server's job store; that the job port is not the lease port and
/// serves no lease route (`/v1/leases` and `/healthz` are 404 there); and that the lease
/// port serves no job route and still serves its own. It catches a listener that routes
/// one of the two protocols nowhere, or that mixes the two route tables.
#[tokio::test]
async fn the_listener_serves_grpc_and_json_on_one_port() {
    let h = job_server("job_port_both").await;
    let job = h.running.job_addr().unwrap();
    assert_ne!(job, h.addr());

    let status = get_job(&mut client(job).await, Some(TOKEN)).await;
    assert_eq!(
        (status.code(), status.message()),
        (Code::NotFound, "unknown-job")
    );
    let reply = request(job, "GET", GET_JOB, Some(TOKEN), None).await;
    assert_eq!((reply.status, reply.body.as_str()), (404, UNKNOWN_JOB_BODY));

    for path in ["/v1/leases", "/healthz"] {
        let reply = request(job, "GET", path, Some(TOKEN), None).await;
        assert_eq!(reply.status, 404, "{path}");
    }
    let lease_port = h.call("GET", GET_JOB, None).await;
    assert_eq!(lease_port.status, 404);
    assert_ne!(lease_port.body, UNKNOWN_JOB_BODY);
    assert_eq!(h.call("GET", "/v1/leases", None).await.status, 200);
}

/// Asserts that the token is checked on both protocols through the real listener: with no
/// token, and with a wrong one, a gRPC call is answered `UNAUTHENTICATED` with an empty
/// message and an HTTP request 401 with an empty body; with the token they reach the
/// job store's `unknown-job` (`NOT_FOUND`, 404). It catches a listener whose routing passes either
/// transport around the bearer layer.
#[tokio::test]
async fn the_listener_enforces_the_token_on_both_protocols() {
    let h = job_server("job_port_token").await;
    let job = h.running.job_addr().unwrap();
    let mut grpc = client(job).await;
    for token in [None, Some(WRONG_TOKEN)] {
        let status = get_job(&mut grpc, token).await;
        assert_eq!(
            (status.code(), status.message()),
            (Code::Unauthenticated, ""),
            "{token:?}"
        );
        let reply = request(job, "GET", GET_JOB, token, None).await;
        assert_eq!((reply.status, reply.body.as_str()), (401, ""), "{token:?}");
        let submit = request(job, "POST", "/v1/jobs", token, Some(b"{}")).await;
        assert_eq!(
            (submit.status, submit.body.as_str()),
            (401, ""),
            "{token:?}"
        );
    }
    let status = get_job(&mut grpc, Some(TOKEN)).await;
    assert_eq!(status.code(), Code::NotFound);
    let reply = request(job, "GET", GET_JOB, Some(TOKEN), None).await;
    assert_eq!((reply.status, reply.body.as_str()), (404, UNKNOWN_JOB_BODY));
}

/// Asserts that without `--job-listen` the server is as it was: it serves no job port (no
/// second address is bound), and no job route is on the lease port (404 with the token),
/// whose own routes answer as before. The lease goldens (`gpu_path_pin`) are this
/// server's byte for byte. It catches the job service mounted, or a port bound, by a
/// server that was not asked for it.
#[tokio::test]
async fn job_listen_off_by_default_leaves_the_server_as_it_was() {
    let h = start_agents("job_port_off", true).await;
    assert_eq!(h.running.job_addr(), None);
    for path in ["/v1/jobs", GET_JOB] {
        assert_eq!(h.call("GET", path, None).await.status, 404, "{path}");
    }
    assert_eq!(h.call("POST", "/v1/jobs", None).await.status, 404);
    assert_eq!(h.call("GET", "/v1/leases", None).await.status, 200);
}

/// Asserts that the server's stop ends the job port with the others: a keep-alive
/// connection on the job port, answered once (so it is open and idle), gets no answer to
/// a second request after `shutdown` returns, which it does only once every listener has
/// drained; the server closed it. It catches a job port left serving after the stop, or
/// one the stop does not wait for.
#[tokio::test]
async fn the_listener_stops_with_the_server() {
    let h = job_server("job_port_stop").await;
    let job = h.running.job_addr().unwrap();
    let mut open = TcpStream::connect(job).await.unwrap();
    let head =
        format!("GET {GET_JOB} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\n\r\n");
    open.write_all(head.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    guard(async {
        while !seen.ends_with(UNKNOWN_JOB_BODY.as_bytes()) {
            let mut chunk = [0u8; 1024];
            let read = open.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "the connection closed before its answer");
            seen.extend_from_slice(&chunk[..read]);
        }
    })
    .await;
    assert!(seen.starts_with(b"HTTP/1.1 404"), "{seen:?}");

    let Harness { running, .. } = h;
    running.shutdown().await.unwrap();
    let _ = open.write_all(head.as_bytes()).await;
    let mut rest = Vec::new();
    let _ = open.read_to_end(&mut rest).await;
    assert_eq!(String::from_utf8_lossy(&rest), "");
}

/// Asserts that with `--tls-cert` and `--tls-key` the job port speaks TLS as the lease
/// port does, over the same pair: a client that offers HTTP/2 and HTTP/1.1 settles on
/// HTTP/2 there (and on HTTP/1.1 on the lease port, which speaks nothing else), gRPC over
/// TLS and HTTP/1.1 over TLS both reach the service, a plain request gets no answer, and
/// after a reload a client that trusts only the renewed certificate is served there too.
/// It catches a job port served in clear beside a TLS lease port, one that cannot carry
/// gRPC over TLS, and one that keeps the old pair after a reload.
#[tokio::test]
async fn the_job_port_speaks_tls_over_the_reloaded_pair() {
    let scratch = Scratch::new("job_port_tls");
    let (cert, key) = test_identity();
    let cert_file = scratch.write("test-only-cert.pem", &cert, 0o644);
    let key_file = scratch.write("test-only-key.pem", &key, 0o600);
    let mut args = job_args(&scratch, "127.0.0.1:0");
    args.push("--tls-cert".into());
    args.push(cert_file.into_os_string());
    args.push("--tls-key".into());
    args.push(key_file.into_os_string());
    let h = Harness::with(scratch, args, None).await;
    // A time inside the test certificates' validity, which a reload checks.
    h.clock.set(Timestamp::from_millis(1_893_456_000_000));
    let job = h.running.job_addr().unwrap();

    assert_eq!(negotiated(job, &cert).await, Some(b"h2".to_vec()));
    assert_eq!(
        negotiated(h.addr(), &cert).await,
        Some(b"http/1.1".to_vec())
    );
    let status = get_job(&mut tls_client(job, &cert).await, Some(TOKEN)).await;
    assert_eq!(status.code(), Code::NotFound);
    let reply = tls_request(job, &connector(&cert), "GET", GET_JOB, Some(TOKEN), None).await;
    assert_eq!((reply.status, reply.body.as_str()), (404, UNKNOWN_JOB_BODY));
    assert_eq!(
        request(job, "GET", GET_JOB, Some(TOKEN), None).await.status,
        0
    );

    let (renewed, renewed_key) = test_identity();
    h.scratch.write("test-only-cert.pem", &renewed, 0o644);
    h.scratch.write("test-only-key.pem", &renewed_key, 0o600);
    h.running.reload_tls().unwrap();
    let reply = tls_request(job, &connector(&renewed), "GET", GET_JOB, Some(TOKEN), None).await;
    assert_eq!((reply.status, reply.body.as_str()), (404, UNKNOWN_JOB_BODY));
    let status = get_job(&mut tls_client(job, &renewed).await, Some(TOKEN)).await;
    assert_eq!(status.code(), Code::NotFound);
}
