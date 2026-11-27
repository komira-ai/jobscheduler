//! Plain HTTP/1.1 on loopback: tierstore's `/readyz` and `/metricz`, and the S3 PUT of the tree.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long one request may take before the test fails on it.
const LIMIT: Duration = Duration::from_secs(30);

/// An answer: its status and body.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    /// The body as text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// `method path` to `addr` with `headers` and `body`, on a connection of its own, or the reason
/// it got no answer.
pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> Result<Answer, String> {
    let mut stream =
        TcpStream::connect_timeout(&addr, LIMIT).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(LIMIT))
        .and_then(|()| stream.set_write_timeout(Some(LIMIT)))
        .map_err(|e| format!("set the time limits: {e}"))?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|e| format!("send {method} {path}: {e}"))?;
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .map_err(|e| format!("read the answer to {method} {path}: {e}"))?;
    parse(&answer).ok_or_else(|| {
        format!(
            "not an HTTP answer to {method} {path}: {}",
            String::from_utf8_lossy(&answer)
        )
    })
}

/// `GET path` from `addr`.
pub fn get(addr: SocketAddr, path: &str) -> Result<Answer, String> {
    request(addr, "GET", path, &[], &[])
}

/// The status and body of a whole answer, its body un-chunked when it came in chunks.
fn parse(answer: &[u8]) -> Option<Answer> {
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&answer[..end]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    let rest = &answer[end + 4..];
    let body = if head.contains("transfer-encoding: chunked") {
        unchunk(rest)?
    } else {
        rest.to_vec()
    };
    Some(Answer { status, body })
}

/// The bytes of a chunked body.
fn unchunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = String::from_utf8_lossy(&rest[..line]);
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line + 2..];
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}
