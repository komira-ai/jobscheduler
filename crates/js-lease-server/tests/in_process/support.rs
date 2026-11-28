//! What the in-process tests share: scratch directories, the flags of a server with agent
//! registration, a clock the test moves, the two ways of reaching a server (its routers in
//! process, or its listeners over loopback) and the messages an agent sends.

use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use js_lease_server::in_process::{InProcess, as_host};
use js_lease_server::{Cli, Config, Running, SharedClock, TickHandle, channel_ticker};
use js_testkit::dir::TestDir;
use js_types::{HostId, ManualClock, Monotonic, Timestamp};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, date_time_ymd,
};
use rustls::ClientConfig;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tower::ServiceExt;

/// The wall time every test clock starts at.
pub const T0: u64 = 1_000_000;

/// The bearer token of the test servers.
pub const TOKEN: &str = "test-token-0123456789-abcdefghijkl";

/// The pool registering hosts join: room for two machines of the shape [`hello_body`]
/// announces.
const POOL: &str = r#"
name = "burst"
class = "gpu-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184
gpus = 1
gpu_class = "nvidia-a100"

[capabilities]
arch = "x86_64"
isa = "x86-64-v3"
"#;

/// A directory under the target directory that holds one test's files and is removed
/// when the test ends; every call gets a directory of its own.
pub struct Scratch {
    pub dir: PathBuf,
    _owner: TestDir,
}

impl Scratch {
    pub fn new(name: &str) -> Self {
        let owner = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("js-lease-server-in-process"),
            name,
        );
        Self {
            dir: owner.path().to_path_buf(),
            _owner: owner,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Writes `text` to the file `name` with `mode` and returns its path.
    pub fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    pub fn state(&self) -> PathBuf {
        self.path("leases.toml")
    }
}

/// The certificates of the tests, made once: a CA whose host certificates the servers admit
/// hosts by, and the certificate and key of the servers themselves (for 127.0.0.1), which the
/// clients of a listening server trust.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
    server_cert: String,
    server_key: String,
}

fn pki() -> &'static Pki {
    static PKI: std::sync::OnceLock<Pki> = std::sync::OnceLock::new();
    PKI.get_or_init(|| {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only hostkeeper CA");
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let names = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        let made = rcgen::generate_simple_self_signed(names).unwrap();
        Pki {
            ca,
            server_cert: made.cert.pem(),
            server_key: made.signing_key.serialize_pem(),
        }
    })
}

/// The host certificate of `host`, as hostkeeper issues one, signed by the CA of the tests: its
/// chain and key.
fn host_identity(host: &str) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![host.to_owned()]).unwrap();
    let uri = Ia5String::try_from("urn:hostkeeper:host-role:host".to_owned()).unwrap();
    params.subject_alt_names.push(SanType::URI(uri));
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("host:{host}"));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(3000, 1, 1);
    let cert = params.signed_by(&key, &pki().ca).unwrap();
    let pem = cert.pem();
    let chain = CertificateDer::pem_slice_iter(pem.as_bytes());
    let chain = chain.collect::<Result<Vec<_>, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes()).unwrap();
    (chain, key)
}

/// The host an agent-API call is for: the host of `/v1/hosts/<host>`, or the `host_id` of the
/// body of a message to `/v1/agent/`. `None` for every other call (a client's).
fn host_of(path: &str, body: Option<&Value>) -> Option<String> {
    if let Some(rest) = path.strip_prefix("/v1/hosts/") {
        return rest.split(['/', '?']).next().map(str::to_owned);
    }
    path.starts_with("/v1/agent/").then(|| {
        let named = body.and_then(|body| body["host_id"].as_str());
        named.unwrap_or("unnamed-host").to_owned()
    })
}

/// The flags of a test server: an ephemeral loopback port, `boot`, an owner-only token file,
/// registration of hosts into [`POOL`], and TLS with the CA that admits hosts by their
/// certificates.
pub fn args(scratch: &Scratch, boot: &str) -> Vec<OsString> {
    let token = scratch.write("token", TOKEN, 0o600);
    let pool = scratch.write("agent-pool.toml", POOL, 0o644);
    let cert = scratch.write("tls-cert.pem", &pki().server_cert, 0o644);
    let key = scratch.write("tls-key.pem", &pki().server_key, 0o600);
    let ca = scratch.write("host-ca.pem", &pki().ca.pem(), 0o644);
    let flags: Vec<(&str, OsString)> = vec![
        ("--listen", "127.0.0.1:0".into()),
        ("--state-file", scratch.state().into_os_string()),
        ("--boot", boot.into()),
        ("--token-file", token.into_os_string()),
        ("--tls-cert", cert.into_os_string()),
        ("--tls-key", key.into_os_string()),
        ("--agent-client-ca", ca.into_os_string()),
        ("--agent-pool-config", pool.into_os_string()),
    ];
    let mut args: Vec<OsString> = vec!["js-lease-server".into()];
    for (flag, value) in flags {
        args.push(flag.into());
        args.push(value);
    }
    args
}

/// The configuration of [`args`], its metrics timed on `monotonic`.
pub fn config(scratch: &Scratch, boot: &str, monotonic: &Stepping) -> Config {
    let cli = Cli::try_parse_from(args(scratch, boot)).unwrap();
    let config = Config::from_cli(cli).unwrap();
    config.with_monotonic(Arc::new(monotonic.clone()))
}

/// A monotonic clock that moves by `step` at every reading, so a step
/// timed with one reading before and one after lasts exactly `step`, and no real clock is
/// read.
#[derive(Debug, Clone)]
pub struct Stepping {
    micros: Arc<AtomicU64>,
    step: Duration,
}

impl Stepping {
    pub fn of(step: Duration) -> Self {
        Self {
            micros: Arc::new(AtomicU64::new(0)),
            step,
        }
    }
}

impl Monotonic for Stepping {
    fn reading(&self) -> Duration {
        let step = u64::try_from(self.step.as_micros()).unwrap();
        Duration::from_micros(self.micros.fetch_add(step, Ordering::SeqCst))
    }
}

/// A clock standing at [`T0`] that the test moves, and the server's handle on it.
pub fn clock() -> (ManualClock, SharedClock) {
    let clock = ManualClock::new(Timestamp::from_millis(T0));
    let shared = SharedClock::new(clock.clone());
    (clock, shared)
}

/// An in-process server on the flags of [`args`], with its clock.
pub struct Cell {
    pub server: InProcess,
    pub clock: ManualClock,
    /// Owns the server's files.
    _scratch: Scratch,
}

impl Cell {
    pub fn start(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let (clock, shared) = clock();
        let monotonic = Stepping::of(Duration::from_millis(1));
        let config = config(&scratch, "initialise", &monotonic);
        let server = InProcess::start(config, shared).unwrap();
        Self {
            server,
            clock,
            _scratch: scratch,
        }
    }

    /// A request to the client router, with the client token.
    pub async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let router = self.server.client_router();
        routed(&router, method, path, Some(TOKEN), body).await
    }

    /// A request to the agent router, presenting the host the body names.
    pub async fn agent(&self, path: &str, body: &Value) -> Reply {
        let router = self.server.agent_router();
        routed(&router, "POST", path, None, Some(body)).await
    }
}

/// What a request got back: the status and the body, as JSON when it is (`Null` when it
/// is empty, a string when it is not JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

fn reply(status: u16, bytes: &[u8]) -> Reply {
    let text = String::from_utf8_lossy(bytes);
    let body = match text.is_empty() {
        true => Value::Null,
        false => serde_json::from_str(&text).unwrap_or(Value::String(text.into_owned())),
    };
    Reply { status, body }
}

/// One request answered by `router`, called as a service: no socket, no peer address. A call
/// to an agent route presents the host its body (or its path) names, as the listener does for
/// a connection that carries that host's certificate ([`as_host`]).
pub async fn routed(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let bytes = body.map(Value::to_string).unwrap_or_default();
    let mut request = request.body(Body::from(bytes)).unwrap();
    if let Some(host) = host_of(path, body) {
        as_host(&mut request, &HostId::new(&host).unwrap());
    }
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    reply(status, &bytes)
}

/// A server started the listening way, with the ticker of its table.
pub struct Listening {
    pub running: Running,
    pub ticks: TickHandle,
    /// The ticker of the pool checks, which a server without an elastic pool drops.
    _checks: TickHandle,
}

impl Listening {
    pub async fn start(config: Config, clock: SharedClock) -> Self {
        let (ticks, ticker) = channel_ticker();
        let (checks, check_ticker) = channel_ticker();
        let running = js_lease_server::start_with_checks(config, clock, ticker, check_ticker)
            .await
            .unwrap();
        Self {
            running,
            ticks,
            _checks: checks,
        }
    }

    /// One HTTP/1.1 request over a new loopback connection to the client listener, read to
    /// the end of the connection.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Reply {
        exchange(self.running.addr(), method, path, token, body).await
    }

    /// [`Self::request`] to the job service's listener, with the client token.
    pub async fn job_request(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let addr = self.running.job_addr().expect("the job service is on");
        exchange(addr, method, path, Some(TOKEN), body).await
    }
}

/// One HTTP/1.1 request over a new TLS connection to `addr`, read to the end of the
/// connection. It presents the host certificate of the host an agent-API call is for, and none
/// for any other call.
async fn exchange(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(pki().server_cert.as_bytes()).unwrap())
        .unwrap();
    let builder = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    let config = match host_of(path, body) {
        Some(host) => {
            let (chain, key) = host_identity(&host);
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into());
    let mut stream = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    let authorization = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    let message = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
         {authorization}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(message.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    reply(status, body.as_bytes())
}

/// A hello a host of [`POOL`] sends: the pool's shape exactly.
pub fn hello_body(host: &str) -> Value {
    json!({
        "host_id": host,
        "pool": "burst",
        "agent_version": "0.1.0",
        "shape": {
            "cpu_millis": 8000,
            "memory_bytes": 17179869184u64,
            "gpus": 1,
            "gpu_class": "nvidia-a100",
        },
        "capabilities": { "arch": "x86_64", "isa": "x86-64-v3" },
        "labels": { "rack": "r1" },
        "phase": "serving",
    })
}

/// The hello of `host` that says it is draining, so the scheduler places no job on it and a
/// queued job stays queued.
pub fn hello_body_draining(host: &str) -> Value {
    let mut body = hello_body(host);
    body["phase"] = json!("draining");
    body
}

/// The heartbeat of the session a welcome opened for `host`.
pub fn beat_body(host: &str, welcome: &Reply) -> Value {
    assert_eq!(welcome.status, 200, "{:?}", welcome.body);
    json!({
        "host_id": host,
        "epoch": welcome.body["epoch"],
        "session": welcome.body["session"],
        "phase": "serving",
    })
}
