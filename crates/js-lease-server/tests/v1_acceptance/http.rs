//! The job service's HTTP/JSON gateway (`/v1/jobs` and the read routes) on the server's job
//! port, over plain HTTP/1.1 on loopback with the bearer token, as any HTTP client calls it.

use std::net::SocketAddr;

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

/// A client of the gateway at `addr`.
pub struct Gateway {
    base: String,
    agent: ureq::Agent,
}

impl Gateway {
    /// The gateway on the job port `addr`.
    pub fn new(addr: SocketAddr) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // Loopback only: no proxy the environment names.
            .proxy(None)
            .timeout_global(Some(crate::agent::HANG_GUARD))
            .build()
            .new_agent();
        Self {
            base: format!("http://{addr}"),
            agent,
        }
    }

    /// Reads an answer.
    fn read(answer: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Reply {
        let mut answer = answer.expect("the gateway answers");
        let status = answer.status().as_u16();
        let text = answer.body_mut().read_to_string().expect("the body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
        };
        Reply { status, body }
    }

    /// `GET path`.
    pub fn get(&self, path: &str) -> Reply {
        let call = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {TOKEN}"))
            .call();
        Self::read(call)
    }

    /// `POST path` with the JSON `body`.
    pub fn post(&self, path: &str, body: &Value) -> Reply {
        let call = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .send(body.to_string());
        Self::read(call)
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
