//! The job service's port (`--job-listen`) over real sockets on 127.0.0.1, port 0: gRPC and
//! the HTTP/JSON gateway on the one port, the bearer token in front of both, the lease
//! routes kept apart from it, TLS, and the stop. `GetJob` is served from the server's job
//! store, and most of these tests ask it for a job that does not exist, which the store
//! answers `unknown-job`. The fleet calls are served over the server's registry, pool
//! status and usage sink, and the last test calls each of them over both transports.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::http::Uri;
use hyper_util::rt::TokioIo;
use js_lease::Timestamp;
use js_lease_server::{
    Config, HostLimits, KeyProfile, LoadIn, NoUsage, NodeRow, Observation, OwnerOnlyFiles,
    ProfileKey, RunEnd, SinkCounters, UsageFault, UsageSink,
};
use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::{
    CordonHostRequest, GetJobRequest, GetUsageRequest, Host, ListHostsRequest, ListPoolsRequest,
    ListProvidersRequest, UncordonHostRequest,
};
use js_types::HostId;
use js_usage::{AllocationId, DrainOutcome, EndedPage, EndedQuery, EndedReason, EndedRow};
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

use crate::agents_support::{JOIN, POOL, hello_body, session, start_agents};
use crate::config_support::{PoolText, clock, file, kinds, quote, quoted, server};
use crate::jobs_support::{
    JobBody, beat_with, cancel, commands, exited, job_agent, job_args, job_server, job_state,
    one_start, per_job_hello, running, submit_body,
};
use crate::support::{Harness, Reply, Scratch, TOKEN, guard, request};
use crate::tls::{connector, test_identity, tls_request};

/// The gateway's answer to `GetJob` of a job the store does not hold.
const UNKNOWN_JOB_BODY: &str = r#"{"error":"unknown-job"}"#;

/// A token that is not the server's.
const WRONG_TOKEN: &str = "wrong-token-0123456789-abcdefghijklmnop";

/// A gateway route of the job service: `GetJob` of `job-1`, which no test submits.
const GET_JOB: &str = "/v1/jobs/job-1";

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

/// Asserts that a job holds its host until its agent reports it ended: with one host of
/// 8 cores registered, a job of 6 cores is queued (201) and, after a tick, the agent's
/// next heartbeat reply carries its one `job_start`; the agent lists it running and the
/// job reads `JOB_STATE_RUNNING`; a second job of 6 cores is queued and, while the first
/// runs, a heartbeat carries no command and the second still reads `JOB_STATE_QUEUED`;
/// once the agent reports the first exited the result is accepted, and after a heartbeat
/// that no longer lists it and a tick the next heartbeat reply carries the second job's start, while the first reads
/// `JOB_STATE_SUCCEEDED`. A job that wants a GPU is refused 422 `no-capable-host`, and a whole-host job cancelled
/// while queued reads `JOB_STATE_CANCELLED`.
/// It catches a host given to a second job while the first still runs, and one that stays
/// held after the first job ended.
#[tokio::test]
async fn a_job_holds_its_host_until_its_agent_reports_it_ended() {
    let scratch = Scratch::new("job_port_holds_host");
    let mut args = job_args(&scratch, "127.0.0.1:0");
    // A pool of CPU hosts that names an arch: the scheduler takes a pool as a node only
    // then, and starts no job on a host with GPUs.
    let pool = POOL
        .replace("gpus = 1\ngpu_class = \"nvidia-a100\"\n", "")
        .replace("[capabilities]", "[capabilities]\narch = \"x86_64\"");
    scratch.write("agent-pool.toml", &pool, 0o644);
    args.push("--placement".into());
    args.push("js-sched".into());
    let h = Harness::with(scratch, args, None).await;
    let mut hello = per_job_hello("host-1");
    hello["shape"] = json!({ "cpu_millis": 8000, "memory_bytes": 17179869184u64, "gpus": 0 });
    let agent = job_agent(&h, &hello).await;
    let wide = |id: &str| JobBody::new(id).needs(6000, 1 << 30).wall(900);

    let first = submit_body(&h, &wide("held-1").json()).await;
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.json()["job"]["state"], "JOB_STATE_QUEUED");
    h.ticks.step().await;
    let (started, allocation) = one_start(&beat_with(&h, &agent, &[]).await);
    assert_eq!(started, "held-1");
    let held = beat_with(&h, &agent, &[running("held-1")]).await;
    assert_eq!(commands(&held), (vec![], vec![]), "{held}");
    assert_eq!(job_state(&h, "held-1").await, "JOB_STATE_RUNNING");

    let second = submit_body(&h, &wide("held-2").json()).await;
    assert_eq!(second.status, 201, "{}", second.body);
    h.ticks.step().await;
    let busy = beat_with(&h, &agent, &[running("held-1")]).await;
    assert_eq!(commands(&busy), (vec![], vec![]), "{busy}");
    assert_eq!(job_state(&h, "held-2").await, "JOB_STATE_QUEUED");

    let ended = beat_with(&h, &agent, &[exited("held-1", &allocation)]).await;
    assert_eq!(
        ended["job_acks"],
        json!([{ "job_id": "held-1", "ack": "ACCEPTED" }]),
        "{ended}"
    );
    // The host is free once a heartbeat no longer lists the first job.
    let freed = beat_with(&h, &agent, &[]).await;
    assert_eq!(commands(&freed), (vec![], vec![]), "{freed}");
    h.ticks.step().await;
    let (started, _) = one_start(&beat_with(&h, &agent, &[]).await);
    assert_eq!(started, "held-2");
    assert_eq!(job_state(&h, "held-1").await, "JOB_STATE_SUCCEEDED");

    let gpu = JobBody::new("held-gpu")
        .disk(1 << 30)
        .gpus(1, "nvidia-a100")
        .constraints(json!({ "isaLevel": "x86-64-v3" }));
    let refused = submit_body(&h, &gpu.json()).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(refused.json(), json!({ "error": "no-capable-host" }));

    let third = submit_body(&h, &JobBody::new("held-3").whole_host().json()).await;
    assert_eq!(third.status, 201, "{}", third.body);
    let cancelled = cancel(&h, "held-3").await;
    assert_eq!(cancelled.status, 200, "{}", cancelled.body);
    assert_eq!(job_state(&h, "held-3").await, "JOB_STATE_CANCELLED");
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

/// The host the fleet test registers, in the pool `gpu`.
const FLEET_HOST: &str = "fleet-host";

/// The job of the ended booking the fleet test's usage sink holds.
const ENDED_JOB: &str = "ended-job";

/// The host of that booking: one the registry does not hold, as a booking outlives its host.
const ENDED_HOST: &str = "retired-host";

/// A usage sink that records nothing and holds the ended bookings it was made with, which
/// it answers as one page whatever the query is.
struct Ended(Vec<EndedRow>);

impl UsageSink for Ended {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: HostLimits,
        now_minute: u64,
    ) -> Observation {
        NoUsage.observe(host, epoch, load, limits, now_minute)
    }

    fn run_ended(&self, end: &RunEnd, now_minute: u64) -> Result<(), UsageFault> {
        NoUsage.run_ended(end, now_minute)
    }

    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeRow)>, UsageFault> {
        NoUsage.flush(now_minute)
    }

    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        NoUsage.prune(now_minute)
    }

    fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeRow>, UsageFault> {
        NoUsage.node_rows(host, from, to, limit)
    }

    fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        NoUsage.key_profile(key)
    }

    fn counters(&self) -> SinkCounters {
        NoUsage.counters()
    }

    fn ended_rows_after(&self, _query: &EndedQuery) -> Result<EndedPage, UsageFault> {
        Ok(EndedPage {
            rows: self.0.clone(),
            next: None,
        })
    }
}

/// The booking of [`ENDED_JOB`] on [`ENDED_HOST`] that ended at 1,000,000 ms.
fn ended_row() -> EndedRow {
    EndedRow {
        id: AllocationId::parse(ENDED_JOB).unwrap(),
        profile: None,
        host: Some(HostId::new(ENDED_HOST).unwrap()),
        booked: None,
        queued_ms: Some(40),
        run_ms: Some(1000),
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: 1_000_000,
        error_ms: None,
        used: None,
    }
}

/// A server with the job service on a loopback port, one elastic pool `gpu` (hosts join it
/// with [`JOIN`]) and `usage` as its usage sink.
async fn fleet_server(name: &str, usage: Arc<dyn UsageSink>) -> Harness {
    let scratch = Scratch::new(name);
    let joins = scratch.write("joins", &format!("{JOIN}\n"), 0o600);
    let pool = PoolText::elastic(&scratch)
        .set("exec", &quote("none"))
        .set("join_token_file", &quoted(&joins));
    let listeners = "listen = \"127.0.0.1:0\"\nagent_listen = \"127.0.0.2:0\"\n\
                     job_listen = \"127.0.0.1:0\"";
    let text = file(&server(&scratch, listeners), &[&pool]);
    let path = scratch.write("js.toml", &text, 0o644);
    let config = Config::from_config_file(&path, &kinds(), &OwnerOnlyFiles, clock()).unwrap();
    let harness = Harness::over(scratch, config.with_usage_sink(usage)).await;
    harness.checks.step().await;
    harness
}

/// A hello of `host` into the pool `gpu` on the agent port, which must be admitted, and its
/// first heartbeat.
async fn join_gpu_host(h: &Harness, host: &str) {
    let mut hello = hello_body(host);
    hello["pool"] = json!("gpu");
    hello["shape"]["gpu_class"] = json!("nvidia-a10");
    let hello = hello.to_string().into_bytes();
    let agent = h.running.agent_addr().unwrap();
    let reply = request(agent, "POST", "/v1/agent/hello", Some(JOIN), Some(&hello)).await;
    let joined = session(&reply, host);
    let beat = joined.beat().to_string().into_bytes();
    let path = "/v1/agent/heartbeat";
    let reply = request(agent, "POST", path, Some(JOIN), Some(&beat)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

/// `message` as a gRPC request carrying the client token.
fn authed<T>(message: T) -> tonic::Request<T> {
    let mut call = tonic::Request::new(message);
    let value: MetadataValue<Ascii> = format!("Bearer {TOKEN}").parse().unwrap();
    call.metadata_mut().insert("authorization", value);
    call
}

/// The host `id` as `ListHosts` over gRPC lists it.
async fn grpc_host(grpc: &mut JobServiceClient<Channel>, id: &str) -> Host {
    let listed = grpc.list_hosts(authed(ListHostsRequest::default())).await;
    let hosts = listed.unwrap().into_inner().hosts;
    let found = hosts.into_iter().find(|host| host.id == id);
    found.unwrap_or_else(|| panic!("{id} is not listed over gRPC"))
}

/// The host `id` as `GET /v1/hosts` on the job port lists it.
async fn gateway_host(job: SocketAddr, id: &str) -> Value {
    let reply = request(job, "GET", "/v1/hosts", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let listing = reply.json();
    let hosts = listing["hosts"].as_array().cloned().unwrap_or_default();
    let found = hosts.into_iter().find(|host| host["id"] == id);
    found.unwrap_or_else(|| panic!("{id} is not listed by the gateway: {listing}"))
}

/// Asserts that both transports list `host` as cordoned (it has a cordon expiry) or not.
async fn assert_cordon(
    grpc: &mut JobServiceClient<Channel>,
    job: SocketAddr,
    host: &str,
    cordoned: bool,
) {
    let over_grpc = grpc_host(grpc, host).await;
    assert_eq!(over_grpc.cordon_expires_at_ms.is_some(), cordoned, "gRPC");
    let over_json = gateway_host(job, host).await;
    let expiry = over_json.get("cordonExpiresAtMs");
    assert_eq!(expiry.is_some(), cordoned, "gateway: {over_json}");
}

/// A `POST` of the empty object to the gateway route `path` on the job port.
async fn gateway_post(job: SocketAddr, path: &str) -> Reply {
    request(job, "POST", path, Some(TOKEN), Some(b"{}")).await
}

/// Asserts that the six fleet calls are served on a real server's job port, over gRPC and
/// over the gateway: with one host registered in one elastic pool and one ended booking in
/// the usage sink, `ListHosts` lists the host, `CordonHost` and `UncordonHost` change its
/// cordon (each one made over one transport and read back over both), `ListPools` and
/// `ListProviders` name the pool `gpu`, `GetUsage` returns the ended booking by job and
/// host, and a cordon of a host the registry does not hold is `NOT_FOUND` `unknown-host`
/// over gRPC and 500 `internal` over the gateway, whose error table does not name it yet. It catches a server that builds the job
/// service without the fleet group's context, whose every fleet call answers `UNIMPLEMENTED`
/// (`501`).
#[tokio::test]
async fn the_fleet_calls_are_served_on_the_job_port() {
    let h = fleet_server("job_port_fleet", Arc::new(Ended(vec![ended_row()]))).await;
    join_gpu_host(&h, FLEET_HOST).await;
    let job = h.running.job_addr().unwrap();
    let mut grpc = client(job).await;

    let listed = grpc_host(&mut grpc, FLEET_HOST).await;
    assert_eq!(listed.id, FLEET_HOST);
    assert_eq!(gateway_host(job, FLEET_HOST).await["id"], FLEET_HOST);
    assert_cordon(&mut grpc, job, FLEET_HOST, false).await;

    let cordon = CordonHostRequest {
        host_id: FLEET_HOST.to_owned(),
        cordon_secs: None,
    };
    let placed = grpc.cordon_host(authed(cordon)).await.unwrap().into_inner();
    assert_eq!(placed.host.unwrap().id, FLEET_HOST);
    assert!(!placed.already_cordoned);
    assert_cordon(&mut grpc, job, FLEET_HOST, true).await;

    let lifted = gateway_post(job, &format!("/v1/hosts/{FLEET_HOST}:uncordon")).await;
    assert_eq!(lifted.status, 200, "{}", lifted.body);
    assert_eq!(lifted.json()["host"]["id"], FLEET_HOST);
    assert_eq!(lifted.json()["lifted"], true);
    assert_cordon(&mut grpc, job, FLEET_HOST, false).await;

    let placed = gateway_post(job, &format!("/v1/hosts/{FLEET_HOST}:cordon")).await;
    assert_eq!(placed.status, 200, "{}", placed.body);
    assert_eq!(placed.json()["host"]["id"], FLEET_HOST);
    assert_eq!(placed.json()["alreadyCordoned"], Value::Null);
    assert_cordon(&mut grpc, job, FLEET_HOST, true).await;

    let uncordon = UncordonHostRequest {
        host_id: FLEET_HOST.to_owned(),
    };
    let lifted = grpc.uncordon_host(authed(uncordon)).await.unwrap();
    assert!(lifted.into_inner().lifted);
    assert_cordon(&mut grpc, job, FLEET_HOST, false).await;

    let unknown = CordonHostRequest {
        host_id: "no-such-host".to_owned(),
        cordon_secs: None,
    };
    let status = grpc
        .cordon_host(authed(unknown))
        .await
        .expect_err("the registry holds no such host");
    assert_eq!(
        (status.code(), status.message()),
        (Code::NotFound, "unknown-host")
    );
    let reply = gateway_post(job, "/v1/hosts/no-such-host:cordon").await;
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (500, r#"{"error":"internal"}"#)
    );

    let pools = grpc.list_pools(authed(ListPoolsRequest::default())).await;
    let pools = pools.unwrap().into_inner().pools;
    let names: Vec<&str> = pools.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["gpu"]);
    let reply = request(job, "GET", "/v1/pools", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["pools"].as_array().map(Vec::len), Some(1));
    assert_eq!(reply.json()["pools"][0]["name"], "gpu");

    let providers = grpc
        .list_providers(authed(ListProvidersRequest::default()))
        .await;
    let providers = providers.unwrap().into_inner().providers;
    let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["gpu"]);
    let reply = request(job, "GET", "/v1/providers", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["providers"].as_array().map(Vec::len), Some(1));
    assert_eq!(reply.json()["providers"][0]["name"], "gpu");

    let usage = grpc.get_usage(authed(GetUsageRequest::default())).await;
    let usage = usage.unwrap().into_inner();
    assert_eq!(usage.records.len(), 1);
    assert_eq!(usage.records[0].job_id, ENDED_JOB);
    assert_eq!(usage.records[0].host_id, ENDED_HOST);
    assert_eq!(usage.next_page_token, "");
    let reply = request(job, "GET", "/v1/usage", Some(TOKEN), None).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["records"].as_array().map(Vec::len), Some(1));
    assert_eq!(reply.json()["records"][0]["jobId"], ENDED_JOB);
    assert_eq!(reply.json()["records"][0]["hostId"], ENDED_HOST);
}
