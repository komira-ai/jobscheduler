//! A transport that answers from a script and keeps what it was sent, and a client over
//! it with the metadata server's credential and a fixed clock.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use js_logstore::{
    HttpRequest, HttpResponse, ManualClock as ManualWall, Transport, TransportError,
};
use js_provider_gce::{Credentials, HttpGce};

/// What the transport answers.
pub type Answer = Result<HttpResponse, TransportError>;

/// The access token the metadata server's scripted answer hands out.
pub const TOKEN: &str = "example-access-token";

/// The address the metadata server's token is asked at.
pub const TOKEN_URL: &str =
    "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token";

/// The seconds since the epoch of the fixed clock: 2026-11-05 01:01:01 UTC.
pub const NOW_SECONDS: u64 = 1_793_836_800 + 3_661;

/// The requests it was sent, and the answers it has left.
pub struct Script {
    sent: Mutex<Vec<HttpRequest>>,
    answers: Mutex<VecDeque<Answer>>,
}

impl Script {
    /// A transport that gives `answers` in order.
    fn new(answers: Vec<Answer>) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.into()),
        })
    }

    /// Every request sent so far.
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }

    /// The answers not yet given.
    pub fn left(&self) -> usize {
        self.answers.lock().unwrap().len()
    }
}

impl Transport for Script {
    fn send(&self, request: HttpRequest) -> Answer {
        self.sent.lock().unwrap().push(request);
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script has an answer for every request")
    }
}

/// An answer with status 200 and `body`.
pub fn answer(body: &str) -> Answer {
    status(200, body)
}

/// An answer with `code` and `body`.
pub fn status(code: u16, body: &str) -> Answer {
    Ok(HttpResponse {
        status: code,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
    })
}

/// A Google error answer of HTTP status `code` whose first error has the reason `reason`.
/// The message is text the client must never keep.
pub fn error_body(code: u16, reason: &str) -> String {
    format!(
        "{{\"error\":{{\"code\":{code},\"message\":\"private text\",\"status\":\"STATUS_TEXT\",\"errors\":[{{\"reason\":\"{reason}\",\"message\":\"private text\"}}]}}}}"
    )
}

/// The metadata server's answer to a token request: the token, valid for `seconds`.
pub fn token_answer(seconds: u64) -> Answer {
    answer(&format!(
        "{{\"access_token\":\"{TOKEN}\",\"expires_in\":{seconds},\"token_type\":\"Bearer\"}}"
    ))
}

/// The fixed clock.
pub fn clock() -> Arc<ManualWall> {
    Arc::new(ManualWall::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECONDS),
    ))
}

/// A client over the metadata server's credential and the fixed clock. The script gives
/// the token first (valid for an hour) and then `answers`, so the first answer the caller
/// gives answers the first request to the API.
pub fn over(answers: Vec<Answer>) -> (HttpGce, Arc<Script>) {
    let mut all = vec![token_answer(3600)];
    all.extend(answers);
    over_with(Credentials::MetadataServer, all, clock())
}

/// Installs a subscriber that takes every event and writes none: a disabled event does not
/// evaluate its fields, and the fields are code that must run under test.
fn trace_everything() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .try_init();
    });
}

/// A client with `credentials` and `wall`, over a script of exactly `answers`.
pub fn over_with(
    credentials: Credentials,
    answers: Vec<Answer>,
    wall: Arc<ManualWall>,
) -> (HttpGce, Arc<Script>) {
    trace_everything();
    let script = Script::new(answers);
    let client = HttpGce::new(credentials, script.clone(), wall);
    (client, script)
}

/// The requests sent other than those for a token.
pub fn api_calls(script: &Script) -> Vec<HttpRequest> {
    script
        .sent()
        .into_iter()
        .filter(|request| request.url != TOKEN_URL)
        .collect()
}
