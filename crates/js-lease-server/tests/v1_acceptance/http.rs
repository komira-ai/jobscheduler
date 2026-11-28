//! The job service's HTTP/JSON gateway (`/v1/jobs` and the read routes) on the server's job
//! port, over HTTP/1.1 on TLS on loopback with the bearer token, as any HTTP client calls it.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;

use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use serde_json::Value;

use crate::server::TOKEN;

/// An answer: its status and its JSON body (`Null` when it has none).
#[derive(Debug)]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body, parsed.
    pub body: Value,
}

/// A client of the gateway at `addr`, over TLS to a server whose certificate it trusts.
pub struct Gateway {
    addr: SocketAddr,
    config: Arc<ClientConfig>,
}

impl Gateway {
    /// The gateway on the job port `addr`, whose server certificate is in the file `trust`.
    pub fn new(addr: SocketAddr, trust: &Path) -> Self {
        let pem = std::fs::read(trust).expect("the server's certificate");
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(&pem).expect("a certificate"))
            .expect("a trusted certificate");
        let config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .expect("the protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self {
            addr,
            config: Arc::new(config),
        }
    }

    /// One request over a new TLS connection, read to the end of the connection.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let name = ServerName::IpAddress(std::net::IpAddr::from([127, 0, 0, 1]).into());
        let session = ClientConnection::new(self.config.clone(), name).expect("a session");
        let tcp = TcpStream::connect(self.addr).expect("the job port");
        tcp.set_read_timeout(Some(crate::agent::HANG_GUARD))
            .expect("a read timeout");
        let mut stream = StreamOwned::new(session, tcp);
        let body = body.map(Value::to_string).unwrap_or_default();
        let message = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
             Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(message.as_bytes()).expect("the request");
        let mut raw = Vec::new();
        // A server that ends the connection without a close_notify is an error after the
        // whole answer was read; what was read is the answer.
        let _ = stream.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, text) = text.split_once("\r\n\r\n").expect("an answer with a head");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status");
        let chunked = head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        let text = if chunked {
            unchunked(text)
        } else {
            text.to_owned()
        };
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
        };
        Reply { status, body }
    }

    /// `GET path`.
    pub fn get(&self, path: &str) -> Reply {
        self.call("GET", path, None)
    }

    /// `POST path` with the JSON `body`.
    pub fn post(&self, path: &str, body: &Value) -> Reply {
        self.call("POST", path, Some(body))
    }

    /// The job `id` as `GetJob` answers, which must be 200.
    pub fn job(&self, id: &str) -> Value {
        let got = self.get(&format!("/v1/jobs/{id}"));
        assert_eq!(got.status, 200, "GetJob {id}: {:?}", got.body);
        got.body["job"].clone()
    }

    /// The state of the job `id`.
    pub fn state(&self, id: &str) -> String {
        self.job(id)["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// The body of a chunked HTTP/1.1 message, with the chunk sizes left out.
fn unchunked(text: &str) -> String {
    let mut rest = text;
    let mut body = String::new();
    while let Some((size, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).expect("a chunk size");
        if size == 0 {
            break;
        }
        body.push_str(&after[..size]);
        rest = after[size..].trim_start_matches("\r\n");
    }
    body
}
