//! What the tests of host certificates share: a CA of the test's own that mints
//! certificates of any shape, a server that trusts it over TLS on 127.0.0.1, and a client that
//! presents one (or none) and reports a handshake or a connection that failed as an error.
//!
//! Every certificate and key is made fresh in the test; nothing here is a secret.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, SanType, SerialNumber, date_time_ymd,
};
use rustls::ClientConfig;
use rustls::SupportedProtocolVersion;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

use crate::agents_support::agent_args;
use crate::support::{Harness, Kept, Reply, Scratch, TOKEN, guard, parse_reply, replace};
use crate::tls::test_identity;

/// The URI SAN hostkeeper puts on a host's own certificate.
pub const HOST_ROLE_URI: &str = "urn:hostkeeper:host-role:host";

/// The URI SAN hostkeeper puts on a certificate of the agent role.
pub const AGENT_ROLE_URI: &str = "urn:hostkeeper:role:agent";

/// A time inside the validity of the certificates the tests mint, 2030-01-01T00:00:00Z, which a
/// reload of the server's pair checks the server's clock against.
pub const START_OF_2030_MS: u64 = 1_893_456_000_000;

/// What a minted certificate has. [`Spec::host`] is the host certificate of a host, shaped as
/// hostkeeper issues it besides its DNS SAN; a test changes one thing about it.
#[derive(Clone)]
pub struct Spec {
    pub dns: Vec<String>,
    pub uris: Vec<String>,
    /// The subject common name, which the server does not read.
    pub cn: Option<String>,
    /// Whether the certificate is a CA's.
    pub ca: bool,
    pub usages: Vec<ExtendedKeyUsagePurpose>,
    /// The years the certificate is valid from and until (1 January of each).
    pub years: (i32, i32),
    pub serial: u64,
}

impl Spec {
    /// The host certificate of `host`.
    pub fn host(host: &str) -> Self {
        Self {
            dns: vec![host.to_owned()],
            uris: vec![HOST_ROLE_URI.to_owned()],
            cn: Some(format!("host:{host}")),
            ca: false,
            usages: vec![ExtendedKeyUsagePurpose::ClientAuth],
            years: (2020, 3000),
            serial: 1,
        }
    }

    /// The same, with the serial `serial`.
    pub fn serial(self, serial: u64) -> Self {
        Self { serial, ..self }
    }
}

/// A certificate (leaf first) and its key, as PEM text.
#[derive(Clone)]
pub struct Identity {
    pub cert_pem: String,
    pub key_pem: String,
}

/// A certificate authority of the test's own.
pub struct Ca(CertifiedIssuer<'static, KeyPair>);

impl Ca {
    pub fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "test-only hostkeeper CA");
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap())
    }

    /// The CA's certificate as PEM.
    pub fn pem(&self) -> String {
        self.0.pem()
    }

    /// A certificate of `spec` signed by this CA, with a new key.
    pub fn issue(&self, spec: &Spec) -> Identity {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(spec.dns.clone()).unwrap();
        for uri in &spec.uris {
            let uri = Ia5String::try_from(uri.clone()).unwrap();
            params.subject_alt_names.push(SanType::URI(uri));
        }
        params.distinguished_name = DistinguishedName::new();
        if let Some(cn) = &spec.cn {
            params
                .distinguished_name
                .push(DnType::CommonName, cn.clone());
        }
        if spec.ca {
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        }
        params.extended_key_usages = spec.usages.clone();
        params.serial_number = Some(SerialNumber::from(spec.serial));
        params.not_before = date_time_ymd(spec.years.0, 1, 1);
        params.not_after = date_time_ymd(spec.years.1, 1, 1);
        let cert = params.signed_by(&key, &self.0).unwrap();
        Identity {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

/// A server over TLS that trusts the CA of its test, and the files it was started from.
pub struct Rig {
    pub h: Harness,
    pub ca: Ca,
    /// The certificate of the server's own pair, which a client trusts.
    pub server_cert: String,
    pub ca_file: PathBuf,
    options: Options,
}

/// How a rig is started.
#[derive(Clone, Copy)]
pub struct Options {
    /// Where the bindings are kept.
    pub kept: Kept,
    /// Whether the job service is on (the CPU pool, as the job tests place on).
    pub jobs: bool,
    /// `--agent-hello-burst`, when the test needs a small one.
    pub burst: Option<&'static str>,
}

impl Options {
    pub const FILES: Options = Options {
        kept: Kept::Files,
        jobs: false,
        burst: None,
    };
}

/// The flags of a server with the pool, TLS and the CA at `ca_file`, and the job service when
/// `options` say so.
pub fn args(scratch: &Scratch, options: Options, ca_file: &Path) -> Vec<OsString> {
    let (cert, key) = test_identity();
    let tls_cert = scratch.write("test-only-cert.pem", &cert, 0o644);
    let tls_key = scratch.write("test-only-key.pem", &key, 0o600);
    let base = match options.jobs {
        true => crate::job_hosts::placing_args(scratch, options.kept),
        false => options.kept.args(scratch, agent_args(scratch)),
    };
    // Room for the several hosts a test says hello for.
    let pool = std::fs::read_to_string(scratch.path("agent-pool.toml")).unwrap();
    scratch.write(
        "agent-pool.toml",
        &pool.replace("max_machines = 2", "max_machines = 8"),
        0o644,
    );
    let mut args = base;
    for (flag, value) in [
        ("--tls-cert", tls_cert),
        ("--tls-key", tls_key),
        ("--agent-client-ca", ca_file.to_path_buf()),
    ] {
        args.push(flag.into());
        args.push(value.into_os_string());
    }
    if let Some(burst) = options.burst {
        args.push("--agent-hello-burst".into());
        args.push(burst.into());
    }
    args
}

impl Rig {
    /// A server started as `options` say, trusting a CA made for it.
    pub async fn start(name: &str, options: Options) -> Self {
        Self::start_with(name, options, None).await
    }

    /// [`Rig::start`] with a second pool, whose configuration file holds `second_pool`.
    pub async fn start_with(name: &str, options: Options, second_pool: Option<&str>) -> Self {
        let scratch = Scratch::new(&options.kept.named(name));
        let ca = Ca::new();
        let ca_file = scratch.write("hostkeeper-ca.pem", &ca.pem(), 0o644);
        let mut args = args(&scratch, options, &ca_file);
        if let Some(pool) = second_pool {
            let file = scratch.write("second-pool.toml", pool, 0o644);
            args.extend(["--agent-pool-config".into(), file.into_os_string()]);
        }
        let server_cert = std::fs::read_to_string(scratch.path("test-only-cert.pem")).unwrap();
        let h = Harness::with(scratch, args).await;
        h.clock
            .set(js_types::Timestamp::from_millis(START_OF_2030_MS));
        Self {
            h,
            ca,
            server_cert,
            ca_file,
            options,
        }
    }

    /// Stops the server and starts another over the same files with `--boot resume`, trusting
    /// the same CA, as the program does after a restart.
    pub async fn restarted(self) -> Self {
        let Self {
            h,
            ca,
            ca_file,
            options,
            ..
        } = self;
        let Harness {
            running, scratch, ..
        } = h;
        running.shutdown().await.unwrap();
        let args = args(&scratch, options, &ca_file);
        let args = replace(args, "--boot", "resume");
        let server_cert = std::fs::read_to_string(scratch.path("test-only-cert.pem")).unwrap();
        let h = Harness::with(scratch, args).await;
        h.clock
            .set(js_types::Timestamp::from_millis(START_OF_2030_MS));
        Self {
            h,
            ca,
            server_cert,
            ca_file,
            options,
        }
    }

    /// The address of the agent routes.
    pub fn addr(&self) -> SocketAddr {
        self.h.addr()
    }

    /// The host list a lease client reads (`GET /v1/hosts` with the client token), as JSON.
    pub async fn listing(&self) -> Value {
        let reply = self
            .client(None)
            .send("GET", "/v1/hosts", Some(TOKEN), None)
            .await;
        let reply = reply.unwrap();
        assert_eq!(reply.status, 200, "{}", reply.body);
        reply.json()
    }

    /// A client of this server presenting `identity`, or none.
    pub fn client(&self, identity: Option<&Identity>) -> Client {
        Client {
            addr: self.addr(),
            trusted: self.server_cert.clone(),
            identity: identity.cloned(),
            tls12: false,
        }
    }
}

/// A client that trusts one server certificate and presents one client certificate or none.
pub struct Client {
    pub addr: SocketAddr,
    pub trusted: String,
    pub identity: Option<Identity>,
    /// Speaks TLS 1.2 only.
    pub tls12: bool,
}

impl Client {
    /// The same client, speaking TLS 1.2 only.
    pub fn tls12(self) -> Self {
        Self {
            tls12: true,
            ..self
        }
    }

    /// A TLS connector with this client's identity. One connector keeps its sessions, so a
    /// client that connects again with it resumes a session when the server allows that.
    pub fn connector(&self) -> TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        let server = CertificateDer::from_pem_slice(self.trusted.as_bytes()).unwrap();
        roots.add(server).unwrap();
        let versions: &[&SupportedProtocolVersion] = match self.tls12 {
            true => &[&rustls::version::TLS12],
            false => rustls::DEFAULT_VERSIONS,
        };
        let builder = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(versions)
            .unwrap()
            .with_root_certificates(roots);
        let config = match &self.identity {
            Some(identity) => {
                let chain = CertificateDer::pem_slice_iter(identity.cert_pem.as_bytes());
                let chain: Vec<_> = chain.collect::<Result<_, _>>().unwrap();
                let key = PrivateKeyDer::from_pem_slice(identity.key_pem.as_bytes()).unwrap();
                builder.with_client_auth_cert(chain, key).unwrap()
            }
            None => builder.with_no_client_auth(),
        };
        TlsConnector::from(Arc::new(config))
    }

    /// One request over a new connection: the reply, or what failed (the handshake, or the
    /// connection that ended with no reply, which is how a client certificate the server
    /// refuses shows in TLS 1.3, where the client finishes its handshake first).
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Reply, String> {
        guard(self.exchange(&self.connector(), method, path, bearer, body)).await
    }

    /// [`Client::send`] over `connector` (see [`Client::connector`]), so that the connections
    /// of one client share a TLS session store.
    pub async fn send_with(
        &self,
        connector: &TlsConnector,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Reply, String> {
        guard(self.exchange(connector, method, path, bearer, body)).await
    }

    async fn exchange(
        &self,
        connector: &TlsConnector,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Reply, String> {
        let tcp = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        // The server certificate names 127.0.0.1, whichever loopback address is dialled.
        let name = ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into());
        let mut stream = match connector.connect(name, tcp).await {
            Ok(stream) => stream,
            Err(error) => return Err(format!("handshake: {error}")),
        };
        let body = body.map(|body| body.to_string().into_bytes());
        let body = body.unwrap_or_default();
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
        if let Some(token) = bearer {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut message = head.into_bytes();
        message.extend_from_slice(&body);
        let sent = stream.write_all(&message).await;
        let mut raw = Vec::new();
        let read = stream.read_to_end(&mut raw).await;
        match (raw.is_empty(), sent.err().or(read.err())) {
            (true, Some(error)) => Err(format!("connection: {error}")),
            (true, None) => Err("connection: closed with no reply".to_owned()),
            (false, _) => Ok(parse_reply(&raw)),
        }
    }

    /// POST of `body` to the hello route.
    pub async fn hello(&self, bearer: Option<&str>, body: &Value) -> Result<Reply, String> {
        self.send("POST", "/v1/agent/hello", bearer, Some(body))
            .await
    }

    /// POST of `body` to the heartbeat route.
    pub async fn beat(&self, bearer: Option<&str>, body: &Value) -> Result<Reply, String> {
        self.send("POST", "/v1/agent/heartbeat", bearer, Some(body))
            .await
    }

    /// GET of the host `host`'s own route.
    pub async fn host(&self, bearer: Option<&str>, host: &str) -> Result<Reply, String> {
        self.send("GET", &format!("/v1/hosts/{host}"), bearer, None)
            .await
    }
}

/// The session a welcome opened: the heartbeat of it.
pub fn beat_of(welcome: &Reply, host: &str) -> Value {
    assert_eq!(welcome.status, 200, "{}", welcome.body);
    let welcome = welcome.json();
    json!({
        "host_id": host,
        "epoch": welcome["epoch"],
        "session": welcome["session"],
        "phase": "serving",
    })
}
