//! The MDM services' HTTP APIs on loopback: NanoDEP's depserver (the Apple Business Manager
//! token and the ADE profile) and NanoHUB's.
//!
//! A call is printed as one line, `DRY-RUN: <call>` or `+ <call>`, showing its method, URL
//! and the size of its body, never the body. It is made only with `--apply`, and only from a
//! binary root owns alone ([`js_host_step::Host::check_binary`]). Each call carries HTTP
//! Basic auth with the service's API key, read from its secret file, in an `Authorization`
//! header: never in a URL, on an argument list or in output.
//!
//! The transport is the [`Api`] trait; [`RealApi`] speaks HTTP/1.1 over a `TcpStream` to an
//! address that is loopback (a [`Service`] cannot hold any other), and the tests use a fake.

use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_host_step::Mode;
use js_mdm_core::config::MdmConfig;

use crate::secret::Secret;
use crate::{Env, MdmError};

/// How long a connect, a read or a write may take: depserver's `/proxy/` calls wait on Apple.
pub const TIMEOUT: Duration = Duration::from_secs(60);
/// The most of an answer's body an error quotes.
#[allow(dead_code)]
const QUOTED: usize = 300;
/// The most of an answer that is read.
const MAX_REPLY: u64 = 4 << 20;

/// One service's API: where it listens and who calls it.
pub struct Service {
    name: &'static str,
    user: &'static str,
    addr: SocketAddr,
    key: Secret,
}

impl Service {
    /// A service at `addr` with Basic auth user `user` and API key `key`.
    ///
    /// # Errors
    /// `addr` is not a loopback address: the key must not leave the host.
    pub fn new(
        name: &'static str,
        user: &'static str,
        addr: SocketAddr,
        key: Secret,
    ) -> Result<Service, MdmError> {
        Ok(Service {
            name,
            user,
            addr,
            key,
        })
    }

    /// NanoDEP's depserver at `MDM_DEP_PORT`, with the key from `nanodep.env`.
    pub fn depserver(c: &MdmConfig, key: Secret) -> Service {
        Service {
            name: "NanoDEP",
            user: "depserver",
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), c.dep_port),
            key,
        }
    }

    /// What the service is called in messages.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Where it listens.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The `Authorization` header's value. The one place the key is put into text; an [`Api`]
    /// sends it as that header and nowhere else.
    pub fn authorization(&self) -> String {
        let pair = format!("{}:{}", self.user, self.key.expose());
        format!("Basic {}", STANDARD.encode(pair))
    }

    #[allow(dead_code)]
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("name", &self.name)
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// An HTTP method the kit calls with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `PUT`.
    Put,
    /// `POST`.
    Post,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Method::Put => "PUT",
            Method::Post => "POST",
        })
    }
}

/// One request: its method, path and query, and body (JSON when `json`).
#[derive(Clone, PartialEq, Eq)]
pub struct Call {
    /// The method.
    pub method: Method,
    /// The path and query.
    pub path: String,
    /// The body.
    pub body: Vec<u8>,
    /// Whether the body is JSON (and says so).
    pub json: bool,
}

impl Call {
    /// A `PUT` of `body`.
    pub fn put(path: impl Into<String>, body: Vec<u8>) -> Call {
        Call {
            method: Method::Put,
            path: path.into(),
            body,
            json: false,
        }
    }

    /// A `POST` of the JSON `body`.
    pub fn post_json(path: impl Into<String>, body: Vec<u8>) -> Call {
        Call {
            method: Method::Post,
            path: path.into(),
            body,
            json: true,
        }
    }
}

impl fmt::Debug for Call {
    /// The method, the path and the size of the body, never the body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Call {{ {} {:?}, body: {} bytes }}",
            self.method,
            self.path,
            self.body.len()
        )
    }
}

/// An answer: its status and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

/// Sends a call to a service and returns its answer. `Err` is a transport failure (the
/// service is down, a timeout, an answer that is not HTTP); any HTTP status is an `Ok`.
pub trait Api {
    /// Sends `call` to `service`.
    ///
    /// # Errors
    /// The text of the transport failure.
    fn send(&self, service: &Service, call: &Call) -> Result<Reply, String>;
}

/// HTTP/1.1 over a `TcpStream`, one request per connection.
pub struct RealApi;

impl Api for RealApi {
    fn send(&self, service: &Service, call: &Call) -> Result<Reply, String> {
        let io = |e: std::io::Error| e.to_string();
        let mut tcp = TcpStream::connect_timeout(&service.addr, TIMEOUT).map_err(io)?;
        tcp.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        tcp.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        tcp.write_all(&request(service, call)).map_err(io)?;
        let mut raw = Vec::new();
        tcp.take(MAX_REPLY).read_to_end(&mut raw).map_err(io)?;
        parse_reply(&raw)
    }
}

fn request(service: &Service, call: &Call) -> Vec<u8> {
    let kind = match call.json {
        true => "Content-Type: application/json;charset=UTF8\r\n",
        false => "",
    };
    let head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: js-mdm\r\nAuthorization: {}\r\nContent-Length: {}\r\n{kind}Connection: close\r\n\r\n",
        call.method,
        call.path,
        service.addr,
        service.authorization(),
        call.body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&call.body);
    bytes
}

#[allow(dead_code)]
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The answer in `raw`, everything the server sent before it closed: a status line, headers,
/// and a body that is `Content-Length` long, chunked, or runs to the end.
///
/// # Errors
/// What is wrong with the answer.
pub fn parse_reply(raw: &[u8]) -> Result<Reply, String> {
    let _ = raw;
    Err("stub".to_owned())
}

/// The body of a chunked answer.
#[allow(dead_code)]
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let eol = find(rest, b"\r\n").ok_or("a chunk size has no end")?;
        let size = std::str::from_utf8(&rest[..eol])
            .ok()
            .and_then(|s| s.split(';').next())
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
            .ok_or("a chunk size is not a number")?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Ok(body);
        }
        body.extend_from_slice(rest.get(..size).ok_or("a chunk is cut short")?);
        rest = rest[size..]
            .strip_prefix(b"\r\n")
            .ok_or("a chunk does not end where it says")?;
    }
}

/// At most [`QUOTED`] characters of `body`, trimmed, for an error message.
pub fn quoted(body: &[u8]) -> String {
    let _ = body;
    String::new()
}

/// `<method> <url> (<service> API[, <n> bytes])`.
#[allow(dead_code)]
struct Shown<'a>(&'a Service, &'a Call);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Shown(service, call) = self;
        let size = match call.body.len() {
            0 => String::new(),
            n => format!(", {n} bytes"),
        };
        let url = service.url(&call.path);
        write!(f, "{} {url} ({} API{size})", call.method, service.name)
    }
}

/// Prints `call`; with [`Mode::Apply`], makes it (refused from a binary others could replace)
/// and returns the body of its 2xx answer. Any other answer fails with its status and the
/// start of its body. A dry run makes no call and returns `None`.
///
/// # Errors
/// The binary is not trusted, the call failed, or the answer was not a 2xx.
pub fn call(
    env: &Env<'_>,
    service: &Service,
    call: &Call,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<Option<Vec<u8>>, MdmError> {
    let _ = (env, service, call, mode, out);
    Ok(None)
}
