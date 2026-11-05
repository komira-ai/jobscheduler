//! An in-memory S3 bucket for tests (feature `fake`), behind the [`Transport`] port.
//!
//! [`FakeStore`] keeps objects with their metadata and serves put, delete, ranged get and
//! ListObjectsV2 (`prefix`, `start-after`, `max-keys`). It checks every request as a store
//! does, in this order:
//!
//! 1. a scripted failure ([`FakeStore::fail_next`]) answers first;
//! 2. the access key must be one the fake issued (403 `InvalidAccessKeyId`), unexpired by the
//!    fake's clock (403 `ExpiredToken`);
//! 3. the Signature Version 4 signature must be the one the fake computes from the request it
//!    received, with every `x-amz-*` header and the session token signed (403
//!    `SignatureDoesNotMatch`);
//! 4. the call must be one the fake serves (400 `InvalidRequest`, 404 `NoSuchBucket`, 405
//!    `MethodNotAllowed`);
//! 5. the credential's [`Policy`] must allow the call's action on its key, or a listing's
//!    prefix (403 `AccessDenied`).
//!
//! A refused request changes nothing. [`FakeStore::operations`] lists the accepted ones in
//! order. A put can be held ([`FakeStore::block_puts`]) until the test releases it, never for a
//! time, and the answer of a stored put can be lost
//! ([`FakeStore::lose_response_of_next_put`]).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use time::{Date, Month, PrimitiveDateTime, Time};

use crate::client::Metadata;
use crate::clock::Clock;
use crate::credential::{Credential, Endpoint};
use crate::http::{HttpRequest, HttpResponse, Method, Transport, TransportError};
use crate::sigv4::{Scope, Unsigned, sign};

/// How long a credential the fake issues is valid, from the fake's clock at issue.
pub const ISSUED_TTL: Duration = Duration::from_secs(3600);

/// The headers the signer adds; the fake computes them again rather than reading them.
const SIGNER_HEADERS: [&str; 5] = [
    "host",
    "authorization",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
];

/// What a credential may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `s3:PutObject`.
    Put,
    /// `s3:DeleteObject`.
    Delete,
    /// `s3:GetObject`.
    Get,
    /// `s3:ListBucket`.
    List,
}

/// A credential's session policy: its actions, on keys (or listing prefixes) under `prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// The actions allowed.
    pub actions: Vec<Action>,
    /// The prefix every key, or every listing's prefix, must begin with.
    pub prefix: String,
}

/// A failure the fake answers in place of the next requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 503 `SlowDown`.
    SlowDown,
    /// 500 `InternalError`.
    InternalError,
    /// 429 `TooManyRequests`.
    TooManyRequests,
    /// 403 `AccessDenied`.
    AccessDenied,
    /// 403 `ExpiredToken`.
    ExpiredToken,
}

impl Status {
    /// The answer.
    fn answer(self) -> HttpResponse {
        match self {
            Self::SlowDown => error(503, "SlowDown"),
            Self::InternalError => error(500, "InternalError"),
            Self::TooManyRequests => error(429, "TooManyRequests"),
            Self::AccessDenied => error(403, "AccessDenied"),
            Self::ExpiredToken => error(403, "ExpiredToken"),
        }
    }
}

/// An operation the fake accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A put of `len` bytes at `key`.
    Put {
        /// The key.
        key: String,
        /// The body's length.
        len: usize,
    },
    /// A delete of `key`.
    Delete {
        /// The key.
        key: String,
    },
    /// A ranged read of `key`.
    Get {
        /// The key.
        key: String,
    },
    /// A listing under `prefix`.
    List {
        /// The prefix.
        prefix: String,
    },
}

/// The fake store. Clones share one bucket.
#[derive(Clone)]
pub struct FakeStore {
    shared: Arc<Shared>,
}

struct Shared {
    endpoint: Endpoint,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    keys: BTreeMap<String, Issued>,
    issued: u64,
    faults: VecDeque<Status>,
    puts_blocked: bool,
    puts_waiting: usize,
    lose_next_put: bool,
    operations: Vec<Op>,
}

struct Stored {
    bytes: Vec<u8>,
    metadata: Metadata,
    last_modified: SystemTime,
}

struct Issued {
    credential: Credential,
    policy: Policy,
}

/// A call the fake serves, as parsed from a request.
enum Call {
    Put {
        key: String,
        metadata: Metadata,
    },
    Delete {
        key: String,
    },
    Get {
        key: String,
        first: u64,
        last: u64,
    },
    List {
        prefix: String,
        start_after: Option<String>,
        max_keys: usize,
    },
}

impl Call {
    /// The action the call needs, and the name the policy's prefix must begin.
    fn needs(&self) -> (Action, &str) {
        match self {
            Self::Put { key, .. } => (Action::Put, key.as_str()),
            Self::Delete { key } => (Action::Delete, key.as_str()),
            Self::Get { key, .. } => (Action::Get, key.as_str()),
            Self::List { prefix, .. } => (Action::List, prefix.as_str()),
        }
    }
}

impl fmt::Debug for FakeStore {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        out.debug_struct("FakeStore")
            .field("endpoint", &self.shared.endpoint)
            .field("objects", &state.objects.len())
            .field("credentials", &state.keys.len())
            .finish_non_exhaustive()
    }
}

impl FakeStore {
    /// An empty bucket at `endpoint` (its host, bucket, path style and region), reading time
    /// from `clock`.
    pub fn new(endpoint: Endpoint, clock: Arc<dyn Clock>) -> Self {
        Self {
            shared: Arc::new(Shared {
                endpoint,
                clock,
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        }
    }

    /// A new temporary credential (with a session token) that may do what `policy` allows,
    /// until [`ISSUED_TTL`] after the fake's clock now.
    pub fn issue(&self, policy: Policy) -> Credential {
        let mut state = self.lock();
        state.issued += 1;
        let n = state.issued;
        let credential = Credential {
            access_key_id: format!("FAKEKEY{n:012}"),
            secret_access_key: format!("fake-secret-{n}"),
            session_token: Some(format!("fake-session-token-{n}")),
            expires_at: self.shared.clock.now() + ISSUED_TTL,
        };
        let issued = Issued {
            credential: credential.clone(),
            policy,
        };
        state.keys.insert(credential.access_key_id.clone(), issued);
        credential
    }

    /// Answers the next `n` requests with `status`, whatever they are; they change nothing.
    pub fn fail_next(&self, n: usize, status: Status) {
        self.lock().faults.extend(std::iter::repeat_n(status, n));
    }

    /// Holds every later put, before it is checked, until [`FakeStore::release_puts`].
    pub fn block_puts(&self) {
        self.lock().puts_blocked = true;
    }

    /// Lets the held puts, and every later one, go on.
    pub fn release_puts(&self) {
        self.lock().puts_blocked = false;
        self.shared.changed.notify_all();
    }

    /// Waits until at least `n` puts are held by [`FakeStore::block_puts`].
    pub fn wait_for_held_puts(&self, n: usize) {
        let state = self.lock();
        let _held = self
            .shared
            .changed
            .wait_while(state, |state| state.puts_waiting < n)
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Stores the next accepted put as usual, and then answers it with a transport error, as a
    /// connection lost after the store took the object.
    pub fn lose_response_of_next_put(&self) {
        self.lock().lose_next_put = true;
    }

    /// Every operation the fake accepted, in order.
    pub fn operations(&self) -> Vec<Op> {
        self.lock().operations.clone()
    }

    /// The bucket's objects, by key.
    pub fn objects(&self) -> BTreeMap<String, Vec<u8>> {
        self.lock()
            .objects
            .iter()
            .map(|(key, stored)| (key.clone(), stored.bytes.clone()))
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The call `request` is, when the request is signed by a valid credential whose policy
    /// allows it; otherwise the refusal.
    fn authorize(&self, state: &State, request: &HttpRequest) -> Result<Call, HttpResponse> {
        let mismatch = || error(403, "SignatureDoesNotMatch");
        let (key_id, signed_names) = request
            .header("authorization")
            .and_then(parse_authorization)
            .ok_or_else(mismatch)?;
        let issued = state
            .keys
            .get(key_id)
            .ok_or_else(|| error(403, "InvalidAccessKeyId"))?;
        if issued.credential.is_expired(self.shared.clock.now()) {
            return Err(error(403, "ExpiredToken"));
        }
        let at = request
            .header("x-amz-date")
            .and_then(parse_stamp)
            .ok_or_else(mismatch)?;
        let (path, query) = split_url(&request.url).ok_or_else(|| error(400, "InvalidURI"))?;
        let headers: Vec<(String, String)> = request
            .headers
            .iter()
            .filter(|(name, _)| !SIGNER_HEADERS.contains(&name.as_str()))
            .cloned()
            .collect();
        let sign_also: Vec<&str> = signed_names
            .into_iter()
            .filter(|name| !name.starts_with("x-amz-") && *name != "host")
            .collect();
        let unsigned = Unsigned {
            method: request.method.as_str(),
            host: request.header("host").unwrap_or_default(),
            path,
            query: &query,
            headers: &headers,
            sign_also: &sign_also,
            body: &request.body,
        };
        let scope = Scope {
            region: self.shared.endpoint.region(),
            service: "s3",
        };
        let expected = sign(&unsigned, &issued.credential, &scope, at);
        let genuine = expected
            .headers
            .iter()
            .all(|(name, value)| request.header(name) == Some(value.as_str()));
        if !genuine {
            return Err(mismatch());
        }
        let call = self.call(request, path, &query)?;
        let (action, name) = call.needs();
        let allowed =
            issued.policy.actions.contains(&action) && name.starts_with(&issued.policy.prefix);
        match allowed {
            true => Ok(call),
            false => Err(error(403, "AccessDenied")),
        }
    }

    /// The call a signed request asks for.
    fn call(
        &self,
        request: &HttpRequest,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Call, HttpResponse> {
        let endpoint = &self.shared.endpoint;
        let invalid = || error(400, "InvalidRequest");
        let in_bucket = match request.header("host") == Some(endpoint.host().as_str()) {
            true => match endpoint.path_style() {
                true => path.strip_prefix(endpoint.bucket_path().as_str()),
                false => Some(path),
            },
            false => None,
        };
        let rest = in_bucket.ok_or_else(|| error(404, "NoSuchBucket"))?;
        let key = match rest {
            "" | "/" => None,
            _ => Some(
                rest.strip_prefix('/')
                    .and_then(percent_decode)
                    .ok_or_else(|| error(404, "NoSuchBucket"))?,
            ),
        };
        match (request.method, key) {
            (Method::Put, Some(key)) => Ok(Call::Put {
                key,
                metadata: request
                    .headers
                    .iter()
                    .filter_map(|(name, value)| {
                        let name = name.strip_prefix("x-amz-meta-")?;
                        Some((name.to_owned(), value.clone()))
                    })
                    .collect(),
            }),
            (Method::Delete, Some(key)) => Ok(Call::Delete { key }),
            (Method::Get, Some(key)) => {
                let (first, last) = request
                    .header("range")
                    .and_then(parse_range)
                    .ok_or_else(invalid)?;
                Ok(Call::Get { key, first, last })
            }
            (Method::Get, None) => {
                let max_keys = param(query, "max-keys")
                    .and_then(|text| text.parse::<usize>().ok())
                    .filter(|max| (1..=1000).contains(max));
                match (param(query, "list-type"), max_keys) {
                    (Some("2"), Some(max_keys)) => Ok(Call::List {
                        prefix: param(query, "prefix").unwrap_or_default().to_owned(),
                        start_after: param(query, "start-after").map(str::to_owned),
                        max_keys,
                    }),
                    _ => Err(invalid()),
                }
            }
            _ => Err(error(405, "MethodNotAllowed")),
        }
    }

    /// Serves an allowed call.
    fn serve(&self, state: &mut State, call: Call, body: Vec<u8>) -> HttpResponse {
        match call {
            Call::Put { key, metadata } => {
                state.operations.push(Op::Put {
                    key: key.clone(),
                    len: body.len(),
                });
                let stored = Stored {
                    bytes: body,
                    metadata,
                    last_modified: self.shared.clock.now(),
                };
                state.objects.insert(key, stored);
                answer(200, Vec::new(), Vec::new())
            }
            Call::Delete { key } => {
                state.operations.push(Op::Delete { key: key.clone() });
                state.objects.remove(&key);
                answer(204, Vec::new(), Vec::new())
            }
            Call::Get { key, first, last } => {
                state.operations.push(Op::Get { key: key.clone() });
                match state.objects.get(&key) {
                    None => error(404, "NoSuchKey"),
                    Some(stored) => ranged(stored, first, last),
                }
            }
            Call::List {
                prefix,
                start_after,
                max_keys,
            } => {
                state.operations.push(Op::List {
                    prefix: prefix.clone(),
                });
                let bucket = self.shared.endpoint.bucket();
                listing(state, bucket, &prefix, start_after.as_deref(), max_keys)
            }
        }
    }
}

impl Transport for FakeStore {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut state = self.lock();
        let is_put = request.method == Method::Put;
        if is_put {
            state.puts_waiting += 1;
            self.shared.changed.notify_all();
            state = self
                .shared
                .changed
                .wait_while(state, |state| state.puts_blocked)
                .unwrap_or_else(PoisonError::into_inner);
            state.puts_waiting -= 1;
        }
        if let Some(status) = state.faults.pop_front() {
            return Ok(status.answer());
        }
        let call = match self.authorize(&state, &request) {
            Ok(call) => call,
            Err(refusal) => return Ok(refusal),
        };
        let answer = self.serve(&mut state, call, request.body);
        let lost = is_put && state.lose_next_put;
        state.lose_next_put &= !lost;
        match lost {
            true => Err(TransportError::Io(io::ErrorKind::ConnectionReset)),
            false => Ok(answer),
        }
    }
}

/// The 206 answer of `stored`'s bytes `first..=last`, cut at its end; 416 when `first` is past
/// it.
fn ranged(stored: &Stored, first: u64, last: u64) -> HttpResponse {
    let total = u64::try_from(stored.bytes.len()).unwrap_or(u64::MAX);
    if first >= total {
        return error(416, "InvalidRange");
    }
    let last = last.min(total - 1);
    let bytes = usize::try_from(first)
        .ok()
        .zip(usize::try_from(last).ok())
        .and_then(|(from, to)| stored.bytes.get(from..=to))
        .unwrap_or_default()
        .to_vec();
    let mut headers = vec![(
        "content-range".to_owned(),
        format!("bytes {first}-{last}/{total}"),
    )];
    headers.extend(
        stored
            .metadata
            .iter()
            .map(|(name, value)| (format!("x-amz-meta-{name}"), value.clone())),
    );
    answer(206, headers, bytes)
}

/// The ListObjectsV2 answer for keys under `prefix`, after `start_after`, at most `max_keys`.
fn listing(
    state: &State,
    bucket: &str,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
) -> HttpResponse {
    let mut found = state.objects.iter().filter(|(key, _)| {
        key.starts_with(prefix) && start_after.is_none_or(|after| key.as_str() > after)
    });
    let page: Vec<_> = found.by_ref().take(max_keys).collect();
    let truncated = found.next().is_some();
    let contents: String = page
        .iter()
        .map(|(key, stored)| {
            format!(
                "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;fake&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                escape(key),
                rfc3339(stored.last_modified),
                stored.bytes.len()
            )
        })
        .collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult><Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{max_keys}</MaxKeys><IsTruncated>{truncated}</IsTruncated>{contents}</ListBucketResult>",
        escape(bucket),
        escape(prefix),
        page.len(),
    );
    answer(200, vec![xml()], body.into_bytes())
}

/// An S3 error answer.
fn error(status: u16, code: &str) -> HttpResponse {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>refused by the fake store</Message></Error>"
    );
    answer(status, vec![xml()], body.into_bytes())
}

/// An answer.
fn answer(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers,
        body,
    }
}

/// The content type of an XML answer.
fn xml() -> (String, String) {
    ("content-type".to_owned(), "application/xml".to_owned())
}

/// The access key id and the signed header names of an `Authorization` value.
fn parse_authorization(value: &str) -> Option<(&str, Vec<&str>)> {
    let rest = value.strip_prefix("AWS4-HMAC-SHA256 Credential=")?;
    let (key_id, rest) = rest.split_once('/')?;
    let (_, rest) = rest.split_once(", SignedHeaders=")?;
    let (names, _) = rest.split_once(", Signature=")?;
    Some((key_id, names.split(';').collect()))
}

/// The instant of an `x-amz-date` stamp, `YYYYMMDDTHHMMSSZ`.
fn parse_stamp(text: &str) -> Option<SystemTime> {
    let shape = text.len() == 16 && text.get(8..9) == Some("T") && text.get(15..) == Some("Z");
    let number = |from: usize, to: usize| -> Option<u16> {
        let digits = text.get(from..to)?;
        digits
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| digits.parse().ok())
            .flatten()
    };
    shape.then_some(())?;
    let month = Month::try_from(u8::try_from(number(4, 6)?).ok()?).ok()?;
    let day = u8::try_from(number(6, 8)?).ok()?;
    let date = Date::from_calendar_date(i32::from(number(0, 4)?), month, day).ok()?;
    let hour = u8::try_from(number(9, 11)?).ok()?;
    let minute = u8::try_from(number(11, 13)?).ok()?;
    let second = u8::try_from(number(13, 15)?).ok()?;
    let time = Time::from_hms(hour, minute, second).ok()?;
    Some(PrimitiveDateTime::new(date, time).assume_utc().into())
}

/// The path and the decoded query of a URL.
fn split_url(url: &str) -> Option<(&str, Vec<(String, String)>)> {
    let (_, rest) = url.split_once("://")?;
    let at = rest.find('/')?;
    let (path, query) = rest[at..].split_once('?').unwrap_or((&rest[at..], ""));
    let pairs = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(name)?, percent_decode(value)?))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((path, pairs))
}

/// The value of the query parameter `name`.
fn param<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(have, _)| have == name)
        .map(|(_, value)| value.as_str())
}

/// `text` with each `%XX` replaced by its byte; `None` for a bad escape or bytes that are not
/// UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    while let Some((plain, after)) = rest.split_once('%') {
        out.extend_from_slice(plain.as_bytes());
        let hex = after
            .get(..2)
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
        out.push(u8::from_str_radix(hex, 16).ok()?);
        rest = &after[2..];
    }
    out.extend_from_slice(rest.as_bytes());
    String::from_utf8(out).ok()
}

/// The first and last byte of a `range` value, `bytes=first-last` with `first <= last`.
fn parse_range(value: &str) -> Option<(u64, u64)> {
    let (first, last) = value.strip_prefix("bytes=")?.split_once('-')?;
    let first: u64 = first.parse().ok()?;
    let last: u64 = last.parse().ok()?;
    (first <= last).then_some((first, last))
}

/// `text` escaped for an XML element.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `at` in RFC 3339 with milliseconds, in UTC, as S3 writes `LastModified`.
fn rfc3339(at: SystemTime) -> String {
    let at = time::OffsetDateTime::from(at);
    let (year, month, day) = at.to_calendar_date();
    let (hour, minute, second, milli) = at.to_hms_milli();
    format!(
        "{year:04}-{:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z",
        u8::from(month)
    )
}
