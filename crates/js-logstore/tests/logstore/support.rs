//! What the tests share: a fixed clock, endpoints, a rig of a fake store and its client, a
//! transport that answers scripted replies, and a signer of raw requests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use js_logstore::fake::{Action, FakeStore, Policy};
use js_logstore::sigv4::{self, Scope, Unsigned};
use js_logstore::{
    Credential, Endpoint, HttpRequest, HttpResponse, ManualClock, Method, Store, Transport,
    TransportError,
};

/// The time every test starts at, in seconds since the Unix epoch.
pub const START: u64 = 1_800_000_000;

/// The region of every test endpoint.
pub const REGION: &str = "us-east-1";

/// The bucket of every test endpoint.
pub const BUCKET: &str = "logs";

/// The instant `seconds` after the Unix epoch.
pub fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// The test endpoint, path-style or virtual-host-style.
pub fn endpoint(path_style: bool) -> Endpoint {
    Endpoint::parse("https://store.example.com", REGION, BUCKET, path_style).unwrap()
}

/// Every action.
pub const ALL: [Action; 4] = [Action::Put, Action::Delete, Action::Get, Action::List];

/// A policy of `actions` under `prefix`.
pub fn policy(actions: &[Action], prefix: &str) -> Policy {
    Policy {
        actions: actions.to_vec(),
        prefix: prefix.to_owned(),
    }
}

/// A fake store and a clock it shares with its clients.
pub struct Rig {
    pub clock: Arc<ManualClock>,
    pub fake: FakeStore,
    pub endpoint: Endpoint,
}

/// A client of the rig's fake store.
pub type FakeClient = Store<FakeStore, Arc<ManualClock>>;

impl Rig {
    /// A path-style rig.
    pub fn new() -> Self {
        Self::with_style(true)
    }

    /// A rig whose endpoint is path-style or not.
    pub fn with_style(path_style: bool) -> Self {
        let clock = Arc::new(ManualClock::new(at(START)));
        let endpoint = endpoint(path_style);
        let fake = FakeStore::new(endpoint.clone(), clock.clone());
        Self {
            clock,
            fake,
            endpoint,
        }
    }

    /// A client signing with `credential`.
    pub fn client(&self, credential: Credential) -> FakeClient {
        Store::new(
            self.fake.clone(),
            self.clock.clone(),
            self.endpoint.clone(),
            credential,
        )
    }

    /// A client whose credential may do everything anywhere.
    pub fn admin(&self) -> FakeClient {
        self.client(self.fake.issue(policy(&ALL, "")))
    }
}

/// A transport that answers the replies it was given, in order, and keeps what it was sent.
/// Clones share the replies and the record.
#[derive(Clone, Default)]
pub struct Scripted {
    replies: Arc<Mutex<VecDeque<Result<HttpResponse, TransportError>>>>,
    sent: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Scripted {
    /// A transport answering `replies`.
    pub fn new(replies: Vec<Result<HttpResponse, TransportError>>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            sent: Arc::default(),
        }
    }

    /// What was sent.
    pub fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }
}

impl Transport for Scripted {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.lock().unwrap().push(request);
        self.replies.lock().unwrap().pop_front().unwrap()
    }
}

/// An answer of `status` with `headers` and `body`.
pub fn reply(
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_vec(),
    })
}

/// An S3 error answer of `status` with the code `code`.
pub fn s3_error(status: u16, code: &str) -> Result<HttpResponse, TransportError> {
    let body = format!("<Error><Code>{code}</Code><Message>no</Message></Error>");
    reply(status, &[], body.as_bytes())
}

/// A client of the test endpoint over `transport`, with a credential valid for an hour.
pub fn scripted_client(transport: Scripted) -> Store<Scripted, Arc<ManualClock>> {
    let clock = Arc::new(ManualClock::new(at(START)));
    Store::new(
        transport,
        clock,
        endpoint(true),
        credential("AKIDTEST", Some("token")),
    )
}

/// A credential with `key_id`, a fixed secret and `token`, valid for an hour from [`START`].
pub fn credential(key_id: &str, token: Option<&str>) -> Credential {
    Credential {
        access_key_id: key_id.to_owned(),
        secret_access_key: "test-secret-access-key".to_owned(),
        session_token: token.map(str::to_owned),
        expires_at: at(START + 3600),
    }
}

/// A request signed for the test region at `now` with `credential`: `url` is the URL as sent,
/// with its query (whose pairs are `query`, in canonical order), and `host` its host header.
pub struct Raw<'a> {
    pub method: Method,
    pub host: &'a str,
    pub path: &'a str,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub sign_also: &'a [&'a str],
    pub body: Vec<u8>,
}

impl<'a> Raw<'a> {
    /// A bodiless request of `method` to `path` on the test endpoint's path-style host.
    pub fn new(method: Method, path: &'a str) -> Self {
        Self {
            method,
            host: "store.example.com",
            path,
            query: Vec::new(),
            headers: Vec::new(),
            sign_also: &[],
            body: Vec::new(),
        }
    }

    /// The same with the query parameter `name=value`.
    pub fn query(mut self, name: &str, value: &str) -> Self {
        self.query.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The same with the header `name: value`.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The request, signed with `credential` at `now` for the service `s3`.
    pub fn sign(self, credential: &Credential, now: SystemTime) -> HttpRequest {
        self.sign_for(credential, now, "s3")
    }

    /// The request, signed with `credential` at `now` for `service`.
    pub fn sign_for(self, credential: &Credential, now: SystemTime, service: &str) -> HttpRequest {
        let unsigned = Unsigned {
            method: self.method.as_str(),
            host: self.host,
            path: self.path,
            query: &self.query,
            headers: &self.headers,
            sign_also: self.sign_also,
            body: &self.body,
        };
        let scope = Scope {
            region: REGION,
            service,
        };
        let signed = sigv4::sign(&unsigned, credential, &scope, now);
        let query = sigv4::canonical_query(&self.query);
        let url = match query.is_empty() {
            true => format!("https://{}{}", self.host, self.path),
            false => format!("https://{}{}?{query}", self.host, self.path),
        };
        let mut headers = self.headers;
        headers.push(("host".to_owned(), self.host.to_owned()));
        headers.extend(signed.headers);
        HttpRequest {
            method: self.method,
            url,
            headers,
            body: self.body,
        }
    }
}

/// The S3 error code of an answer's body.
pub fn code(response: &HttpResponse) -> String {
    let text = String::from_utf8_lossy(&response.body);
    text.split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| code.to_owned())
        .unwrap_or_default()
}
