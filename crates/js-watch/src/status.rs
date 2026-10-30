//! The lease server's heartbeat and expected state, read from its status page.

use std::collections::BTreeSet;
use std::time::Duration;

use js_alert::http::{Plain, UrlError, agent, check};
use serde::Deserialize;
use ureq::Agent;

/// The most bytes of status page read.
const PAGE_LIMIT: u64 = 8 * 1024 * 1024;

/// What the status page says that the watcher uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// The time of the server's last finished scaler tick, in the server's own
    /// milliseconds; `None` before its first. Only whether it changes is used, so a skew
    /// between the two clocks does not matter.
    pub beat: Option<u64>,
    /// The names and provider ids of the machines the server's records hold.
    pub machines: BTreeSet<String>,
}

/// The status could not be read. The words hold no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StatusError(String);

impl StatusError {
    /// An error that reads as `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Where the watcher reads the server's status.
pub trait StatusSource: Send + Sync {
    /// The status now.
    ///
    /// # Errors
    ///
    /// [`StatusError`] when the page is not reachable or not the pools JSON.
    fn read(&self) -> Result<Status, StatusError>;
}

/// The status read from `GET /v1/pools` of the lease server.
pub struct HttpStatus {
    agent: Agent,
    url: String,
}

impl HttpStatus {
    /// A reader of `url`; each read is bound by `limit`. The page takes no token and the
    /// request carries none, so plain `http://` to a private address is allowed.
    ///
    /// # Errors
    ///
    /// [`UrlError`] when the URL is unusable.
    pub fn new(url: &str, limit: Duration) -> Result<Self, UrlError> {
        let https = check(url, Plain::Any)?;
        Ok(Self {
            agent: agent(https, limit),
            url: url.to_owned(),
        })
    }

    fn fetch(&self) -> Result<(u16, Vec<u8>), ureq::Error> {
        let mut response = self.agent.get(self.url.as_str()).call()?;
        let code = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(PAGE_LIMIT)
            .read_to_vec()?;
        Ok((code, body))
    }
}

impl StatusSource for HttpStatus {
    fn read(&self) -> Result<Status, StatusError> {
        let (code, body) = self
            .fetch()
            .map_err(|_| StatusError::new("the status page could not be read"))?;
        if code != 200 {
            return Err(StatusError::new(format!("the status page answered {code}")));
        }
        parse(&body)
    }
}

#[derive(Deserialize)]
struct Pools {
    pools: Vec<Pool>,
}

#[derive(Deserialize)]
struct Pool {
    last_tick_ms: Option<u64>,
    machines: Vec<Machine>,
}

#[derive(Deserialize)]
struct Machine {
    host: String,
    provider_id: Option<String>,
}

/// The status in the body of `GET /v1/pools`.
///
/// # Errors
///
/// [`StatusError`] when the body is not that JSON.
pub fn parse(body: &[u8]) -> Result<Status, StatusError> {
    let page: Pools = serde_json::from_slice(body)
        .map_err(|_| StatusError::new("the status page is not the pools JSON"))?;
    let beat = page.pools.iter().filter_map(|pool| pool.last_tick_ms).max();
    let machines = page
        .pools
        .into_iter()
        .flat_map(|pool| pool.machines)
        .flat_map(|machine| [Some(machine.host), machine.provider_id])
        .flatten()
        .collect();
    Ok(Status { beat, machines })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use js_alert::Receiver;

    use super::*;

    const LIMIT: Duration = Duration::from_secs(30);

    const PAGE: &str = r#"{"pools":[{"name":"gpu","launch":"live","last_tick_ms":1700,
        "machines":[{"host":"js-a","provider_id":"i-1","state":"registered"},
                    {"host":"js-b","provider_id":null,"state":"planned"}],
        "orphans":[{"host":"js-o","provider_id":"i-9"}]}],"warnings":[]}"#;

    /// Asserts what the reader takes from the real shape of `GET /v1/pools`: the tick
    /// time, and the host name and provider id of each machine (a machine without an id
    /// yet is known by its name), and not the orphans. It catches a reader that counts
    /// an orphan as explained, which would silence the alarm for exactly the machines it
    /// exists to find.
    #[test]
    fn the_status_is_the_tick_time_and_the_machines_not_the_orphans() {
        let status = parse(PAGE.as_bytes()).unwrap();
        assert_eq!(status.beat, Some(1700));
        let machines: Vec<&str> = status.machines.iter().map(String::as_str).collect();
        assert_eq!(machines, ["i-1", "js-a", "js-b"]);
        let none = parse(br#"{"pools":[]}"#).unwrap();
        assert_eq!(none, Status::default());
        assert!(parse(b"not json").is_err());
        assert!(
            parse(br#"{"pools":[{"machines":[]}]}"#)
                .unwrap()
                .beat
                .is_none()
        );
    }

    /// Asserts the three ways a read ends: the page parsed, a non-200 answer, and a body
    /// that is not the JSON; and that the error words repeat nothing the server said. It
    /// catches a 503 page read as an empty state (which would make every instance
    /// unexplained or none).
    #[test]
    fn a_read_returns_the_page_or_a_plain_error() {
        let page = Receiver::start_with(200, PAGE);
        let url = format!("http://{}/v1/pools", page.addr());
        let status = HttpStatus::new(&url, LIMIT).unwrap().read().unwrap();
        assert_eq!(status.beat, Some(1700));
        let head = page.requests()[0].head.to_ascii_lowercase();
        assert!(head.starts_with("get /v1/pools http/1.1"), "{head}");

        let down = Receiver::start_with(503, "SECRET");
        let url = format!("http://{}/", down.addr());
        let error = HttpStatus::new(&url, LIMIT).unwrap().read().unwrap_err();
        assert_eq!(error.to_string(), "the status page answered 503");

        let odd = Receiver::start_with(200, "<html>SECRET</html>");
        let url = format!("http://{}/", odd.addr());
        let error = HttpStatus::new(&url, LIMIT).unwrap().read().unwrap_err();
        assert_eq!(error.to_string(), "the status page is not the pools JSON");
    }

    /// Asserts that an unreachable server and a body cut short are errors, not empty
    /// states. It catches a transport failure read as "the server shows no machines".
    #[test]
    fn an_unreachable_server_and_a_cut_body_are_errors() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", closed.local_addr().unwrap());
        drop(closed);
        let reader = HttpStatus::new(&url, LIMIT).unwrap();
        let words = "the status page could not be read";
        assert_eq!(reader.read().unwrap_err().to_string(), words);

        // A server that promises 100 bytes, sends 5 and hangs up.
        let cut = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", cut.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = cut.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            let reply = "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort";
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let reader = HttpStatus::new(&url, LIMIT).unwrap();
        assert!(reader.read().is_err());
        server.join().unwrap();
    }

    /// Asserts that a URL the rule refuses makes no reader. It catches a constructor that
    /// skips the check.
    #[test]
    fn a_bad_url_makes_no_reader() {
        assert!(HttpStatus::new("ftp://host", LIMIT).is_err());
    }
}
