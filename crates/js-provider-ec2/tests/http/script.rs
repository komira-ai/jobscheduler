//! A transport that answers from a script and keeps what it was sent, and a client over
//! it with fixed keys and a fixed clock.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use js_logstore::{
    HttpRequest, HttpResponse, ManualClock as ManualWall, Transport, TransportError,
};
use js_provider_ec2::{Credentials, HttpEc2, Region, StaticKeys};
use serde_json::Value;

/// What the transport answers.
pub type Answer = Result<HttpResponse, TransportError>;

/// The key id of the fixed keys.
pub const KEY_ID: &str = "AKIAEXAMPLE";

/// The secret of the fixed keys.
pub const SECRET: &str = "example-secret-access-key";

/// The session token of the fixed keys.
pub const SESSION: &str = "example-session-token";

/// The region of the client.
pub const REGION: &str = "region-a";

/// The seconds since the epoch of the fixed clock: 2026-11-05 01:01:01 UTC.
pub const NOW_SECONDS: u64 = 1_793_836_800 + 3_661;

/// The requests it was sent, and the answers it has left.
pub struct Script {
    sent: Mutex<Vec<HttpRequest>>,
    answers: Mutex<VecDeque<Answer>>,
}

impl Script {
    /// A transport that gives `answers` in order.
    pub fn new(answers: Vec<Answer>) -> Arc<Self> {
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

    /// Adds answers at the end of the script.
    pub fn push(&self, answers: Vec<Answer>) {
        self.answers.lock().unwrap().extend(answers);
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

/// The fixed clock.
pub fn clock() -> Arc<ManualWall> {
    Arc::new(ManualWall::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECONDS),
    ))
}

/// The region `name`.
pub fn region(name: &str) -> Region {
    Region::new(name).unwrap()
}

/// A client of region `region-a` with the fixed keys, over a script of `answers`.
pub fn rig(answers: Vec<Answer>) -> (HttpEc2, Arc<Script>) {
    let keys = StaticKeys::new(KEY_ID, SECRET, Some(SESSION));
    over(Credentials::Keys(keys), answers, clock())
}

/// A client with `credentials` and `clock`, over a script of `answers`.
pub fn over(
    credentials: Credentials,
    answers: Vec<Answer>,
    clock: Arc<ManualWall>,
) -> (HttpEc2, Arc<Script>) {
    let script = Script::new(answers);
    let client = HttpEc2::new(region(REGION), credentials, script.clone(), clock);
    (client, script)
}

/// The text `text` with its `%XX` escapes undone.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// The parameters of a form request, decoded, in the order sent.
pub fn form(request: &HttpRequest) -> Vec<(String, String)> {
    String::from_utf8(request.body.clone())
        .unwrap()
        .split('&')
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap();
            (unescape(name), unescape(value))
        })
        .collect()
}

/// The parameter `name` of a form request.
pub fn param(request: &HttpRequest, name: &str) -> Option<String> {
    form(request)
        .into_iter()
        .find(|(have, _)| have == name)
        .map(|(_, value)| value)
}

/// The body of a JSON request.
pub fn json(request: &HttpRequest) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

/// The names of the headers a request was signed with.
pub fn signed_headers(request: &HttpRequest) -> Vec<String> {
    let authorization = request.header("authorization").unwrap();
    let signed = authorization
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    signed.split(';').map(str::to_owned).collect()
}

/// An XML answer: `body` inside the root `root` with the cloud's namespace.
pub fn xml(root: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root}>\n<requestId>00000000-0000-0000-0000-000000000000</requestId>\n{body}\n</{root}>"
    )
}
