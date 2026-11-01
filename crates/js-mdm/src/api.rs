//! The MDM services' HTTP APIs on loopback: NanoDEP's depserver (the Apple Business Manager
//! token and the ADE profile) and NanoHUB's.
//!
//! A call is printed as one line, `DRY-RUN: <call>` or `+ <call>`, showing its method, URL
//! and the size of its body, never the body. It is made only with `--apply`, and only from a
//! binary root owns alone, in directories only root can write ([`js_host_step::Host::check_binary`]),
//! and only to a listener root or the service user owns ([`js_host_step::Host::listener_uids`]). Each call carries HTTP
//! Basic auth with the service's API key, read from its secret file, in an `Authorization`
//! header: never in a URL, on an argument list or in output.
//!
//! The transport is the [`Api`] trait; [`RealApi`] speaks HTTP/1.1 over a `TcpStream` to an
//! address that is loopback (a [`Service`] cannot hold any other), and the tests use a fake.

use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use js_host_step::Mode;
use js_mdm_core::config::MdmConfig;

use crate::secret::Secret;
use crate::{Env, MdmError};

/// How long a connect, a read or a write may take on its own: depserver's `/proxy/` calls wait
/// on Apple.
pub const TIMEOUT: Duration = Duration::from_secs(60);
/// How long a whole call may take, from the connect to the last byte of the answer: a server
/// that sends a byte at a time ends here.
pub const TOTAL: Duration = Duration::from_secs(120);
/// The most of an answer's body an error quotes.
const QUOTED: usize = 300;
/// The most of an answer that is read.
const MAX_REPLY: u64 = 4 << 20;

/// One service's API: where it listens and who calls it.
pub struct Service {
    name: &'static str,
    user: &'static str,
    addr: SocketAddr,
    key: Secret,
    owners: Vec<u32>,
}

impl Service {
    /// A service at `addr` with Basic auth user `user` and API key `key`. Only a listener owned
    /// by root or by one of `owners` (the service's own user) is sent the key.
    ///
    /// # Errors
    /// `addr` is not a loopback address: the key must not leave the host.
    pub fn new(
        name: &'static str,
        user: &'static str,
        addr: SocketAddr,
        key: Secret,
        owners: &[u32],
    ) -> Result<Service, MdmError> {
        match addr.ip().is_loopback() {
            true => Ok(Service {
                name,
                user,
                addr,
                key,
                owners: owners.to_vec(),
            }),
            false => Err(MdmError::NotLoopback(addr.to_string())),
        }
    }

    /// NanoDEP's depserver at `MDM_DEP_PORT`, with the key from `nanodep.env`; `owners` are the
    /// uids besides root's that depserver may run as.
    ///
    /// # Errors
    /// As [`Service::new`].
    pub fn depserver(c: &MdmConfig, key: Secret, owners: &[u32]) -> Result<Service, MdmError> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), c.dep_port);
        Service::new("NanoDEP", "depserver", addr, key, owners)
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

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// `text` with the key, its base64 forms (alone and as `user:key`) and every line that names
    /// an `Authorization` header replaced: a server may reflect the request it was sent.
    pub fn redact(&self, text: &str) -> String {
        let (key, pair) = (
            self.key.expose(),
            format!("{}:{}", self.user, self.key.expose()),
        );
        let mut needles = vec![key.to_owned(), pair.clone()];
        for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
            needles.push(engine.encode(key));
            needles.push(engine.encode(&pair));
        }
        // The longest first: the pair holds the key, a padded form holds its unpadded one.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        let mut text = text.to_owned();
        for needle in needles {
            text = text.replace(&needle, "[redacted]");
        }
        let lines: Vec<&str> = text
            .lines()
            .map(|l| match l.to_ascii_lowercase().contains("authorization") {
                true => "[redacted]",
                false => l,
            })
            .collect();
        lines.join("\n")
    }

    /// At most [`QUOTED`] characters of `body`, redacted, trimmed and with control characters
    /// escaped, for an error message.
    pub fn quote(&self, body: &[u8]) -> String {
        let text = self.redact(&String::from_utf8_lossy(body));
        printable(text.trim()).chars().take(QUOTED).collect()
    }
}

/// `text` with every control character written out (`\n`, `\u{1b}`): text from a server is
/// never put on a terminal as it came.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c.is_control() {
            true => c.escape_default().to_string(),
            false => c.to_string(),
        })
        .collect()
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

/// HTTP/1.1 over a `TcpStream`, one request per connection. A call, from the connect to the
/// last byte of the answer, takes at most `total`.
pub struct RealApi {
    /// The longest a whole call may take.
    pub total: Duration,
}

impl Default for RealApi {
    fn default() -> RealApi {
        RealApi { total: TOTAL }
    }
}

impl Api for RealApi {
    fn send(&self, service: &Service, call: &Call) -> Result<Reply, String> {
        let io = |e: std::io::Error| e.to_string();
        let end = Instant::now() + self.total;
        // What is left of the deadline, at most one step's timeout and at least a millisecond (a
        // zero timeout is refused): past the deadline every read waits a millisecond, so a
        // server that only trickles bytes fails at the next gap.
        let left = || {
            let left = end.saturating_duration_since(Instant::now());
            left.clamp(Duration::from_millis(1), TIMEOUT)
        };
        let mut tcp = TcpStream::connect_timeout(&service.addr, left()).map_err(io)?;
        tcp.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        tcp.write_all(&request(service, call)).map_err(io)?;
        let (mut raw, mut chunk) = (Vec::new(), [0u8; 8192]);
        let mut capped = (&tcp).take(MAX_REPLY);
        loop {
            tcp.set_read_timeout(Some(left())).map_err(io)?;
            let n = capped.read(&mut chunk).map_err(io)?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
        }
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The answer in `raw`, everything the server sent before it closed: a status line, headers,
/// and a body that is `Content-Length` long, chunked, or runs to the end.
///
/// # Errors
/// What is wrong with the answer.
pub fn parse_reply(raw: &[u8]) -> Result<Reply, String> {
    let end = find(raw, b"\r\n\r\n").ok_or("the answer has no end of headers")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let lines: Vec<&str> = head.split("\r\n").collect();
    let status = lines[0]
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("the answer has no status")?;
    let header = |name: &str| {
        lines.iter().find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then_some(v.trim())
        })
    };
    let body = &raw[end + 4..];
    let body = match (header("transfer-encoding"), header("content-length")) {
        (Some(t), _) if t.eq_ignore_ascii_case("chunked") => dechunk(body)?,
        (_, Some(n)) => {
            let n: usize = n
                .parse()
                .map_err(|_| "the answer's length is not a number")?;
            let whole = body.get(..n).ok_or("the answer is shorter than it says")?;
            whole.to_vec()
        }
        _ => body.to_vec(),
    };
    Ok(Reply { status, body })
}

/// The body of a chunked answer.
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

/// `<method> <url> (<service> API[, <n> bytes])`.
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
    let shown = Shown(service, call).to_string();
    if mode == Mode::DryRun {
        writeln!(out, "DRY-RUN: {shown}")?;
        return Ok(None);
    }
    env.host
        .check_binary()
        .map_err(|e| MdmError::Untrusted(e.to_string()))?;
    let owners = env.host.listener_uids(service.addr.port())?;
    if let Some(uid) = owners
        .into_iter()
        .find(|uid| *uid != 0 && !service.owners.contains(uid))
    {
        return Err(MdmError::Squatter {
            port: service.addr.port(),
            uid,
        });
    }
    writeln!(out, "+ {shown}")?;
    let reply = env.api.send(service, call).map_err(|why| MdmError::Api {
        call: shown.clone(),
        why,
    })?;
    match (200..300).contains(&reply.status) {
        true => Ok(Some(reply.body)),
        false => Err(MdmError::Api {
            call: shown,
            why: format!("answered {}: {}", reply.status, service.quote(&reply.body)),
        }),
    }
}
