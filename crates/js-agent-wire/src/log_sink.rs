//! The live-log fields (`docs/design/live-logs.md`, section 7): where an agent uploads one
//! job's output ([`LogSink`], in a job start), the host's upload credential ([`LogGrant`], in
//! a heartbeat reply), what the agent holds of it ([`LogGrantState`], in a heartbeat) and
//! what the capture saw of each stream ([`LogReport`], in a job's result).
//!
//! They ride in protocol major 1, announced by the hello capability
//! [`crate::LOG_UPLOAD_CAPABILITY`]: a server sends a `log_sink` or a `log_grant` only to an
//! agent that reported it, and such an agent sends `log_grant_state` and `logs`. Each field
//! is optional and left out when absent, so an older peer ignores it.
//!
//! **A log field never refuses a job or a heartbeat.** Each is decoded as a load is: a value
//! that does not decode (a wrong type, a missing member) reads as absent and the message
//! around it decodes as before. A value that decodes is checked apart from the message that
//! carries it: [`crate::JobStartSpec::validate`] does not look at `log_sink`
//! ([`crate::JobStartSpec::log_sink_checked`] does), a bad `log_grant` is reported in
//! [`crate::ReplyCheck::log_grant`] and costs nothing else of the reply, and
//! [`crate::JobResult::validate`] does not look at `logs` ([`crate::JobResult::logs_report`]
//! drops an inconsistent one). `log_grant_state` is not checked beyond decoding.
//!
//! A sink holds no secret, so an agent may keep one on disk. A grant's secret access key and
//! session token never appear in its `Debug` text, nor more of its access key id than the
//! first four characters; it has no `Display`.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::limits;

/// Where, and whether, the agent uploads one job's output: an S3-compatible store and the
/// job's own prefix in it. It holds no secret: the credential is the host's, sent in
/// [`crate::HeartbeatReply::log_grant`], never in a start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSink {
    /// The store's URL: `https://host[:port]`, or `http://` to a loopback host, with no
    /// user, path, query or fragment (a trailing `/` is allowed); at most
    /// [`limits::MAX_LOG_ENDPOINT_LEN`] bytes.
    pub endpoint: String,
    /// The region requests are signed for: `[A-Za-z0-9._-]`, leading letter or digit, at
    /// most [`limits::MAX_LOG_REGION_LEN`] bytes.
    pub region: String,
    /// The bucket: `[A-Za-z0-9._-]`, leading letter or digit, at most
    /// [`limits::MAX_LOG_BUCKET_LEN`] bytes.
    pub bucket: String,
    /// The job's prefix `P` of the object layout (`prefix/job_id/`, or `job_id/` when the
    /// configured prefix is empty), with its trailing `/`: segments of `[A-Za-z0-9._-]`, none
    /// empty, `.` or `..`, the last the start's own job id, which is at most
    /// [`limits::MAX_LOG_JOB_ID_LEN`] bytes (a longer id has no live logs); at most
    /// [`limits::MAX_LOG_PREFIX_LEN`] bytes.
    pub prefix: String,
    /// Whether the store needs `/bucket/key` URLs. Absent means `false`.
    #[serde(default)]
    pub path_style: bool,
}

/// The host's upload credential: a temporary S3 key that may put and delete under the log
/// prefix and nothing else, one per host for every job on it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogGrant {
    /// The access key id: at most [`limits::MAX_LOG_CREDENTIAL_FIELD`] bytes.
    pub access_key_id: String,
    /// The secret access key: at most [`limits::MAX_LOG_CREDENTIAL_FIELD`] bytes. Secret.
    pub secret_access_key: String,
    /// The session token of a temporary credential: at most
    /// [`limits::MAX_LOG_SESSION_TOKEN`] bytes. Secret. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// When the credential expires, in milliseconds since the Unix epoch; above zero.
    pub expires_at_ms: u64,
}

/// What the agent holds of the host's credential, sent in every heartbeat of an agent that
/// reported [`crate::LOG_UPLOAD_CAPABILITY`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogGrantState {
    /// The agent asks for a new credential: it holds none, or the one it holds has expired,
    /// is close to it, or was refused by the store.
    #[serde(default)]
    pub stale: bool,
    /// The [`LogGrant::expires_at_ms`] of the credential the agent holds, so that the server
    /// can tell which one it has; absent when it holds none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
}

/// What the capture saw of each stream of a job, in its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogReport {
    /// The length of the standard output: every byte the job printed to it.
    pub stdout_bytes: u64,
    /// The length of the standard error.
    pub stderr_bytes: u64,
    /// The bytes of the standard output the host's capture could not keep (past the local
    /// cap, or after a write of the file failed); never uploaded. At most `stdout_bytes`.
    pub stdout_dropped: u64,
    /// The bytes of the standard error the capture could not keep. At most `stderr_bytes`.
    pub stderr_dropped: u64,
}

impl LogReport {
    /// Whether neither stream dropped more bytes than it had.
    pub(crate) fn consistent(&self) -> bool {
        self.stdout_dropped <= self.stdout_bytes && self.stderr_dropped <= self.stderr_bytes
    }
}

/// A secret in a `Debug` text: `***`.
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// The key id's first four characters and `..`; the secret and the token as `***`.
impl fmt::Debug for LogGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown: String = self.access_key_id.chars().take(4).collect();
        f.debug_struct("LogGrant")
            .field("access_key_id", &format!("{shown}.."))
            .field("secret_access_key", &Redacted)
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| Redacted),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl LogSink {
    /// Checks the sink of the start of `job_id`; see [`crate::JobStartSpec::log_sink_checked`].
    pub(crate) fn check(&self, at: At, job_id: &str) -> Result<(), ValidationError> {
        endpoint(at.field("log_sink.endpoint"), &self.endpoint)?;
        check::token(
            at.field("log_sink.region"),
            &self.region,
            limits::MAX_LOG_REGION_LEN,
            check::ident_byte,
        )?;
        check::token(
            at.field("log_sink.bucket"),
            &self.bucket,
            limits::MAX_LOG_BUCKET_LEN,
            check::ident_byte,
        )?;
        prefix(at.field("log_sink.prefix"), &self.prefix, job_id)
    }
}

impl LogGrant {
    /// Checks every field: each text non-empty, within its cap and of printable ASCII with
    /// no space (it goes into a signed request's headers), and an expiry above zero.
    pub(crate) fn check(&self, at: At) -> Result<(), ValidationError> {
        let field = limits::MAX_LOG_CREDENTIAL_FIELD;
        header_text(
            at.field("log_grant.access_key_id"),
            &self.access_key_id,
            field,
        )?;
        header_text(
            at.field("log_grant.secret_access_key"),
            &self.secret_access_key,
            field,
        )?;
        if let Some(token) = &self.session_token {
            header_text(
                at.field("log_grant.session_token"),
                token,
                limits::MAX_LOG_SESSION_TOKEN,
            )?;
        }
        check::range(
            at.field("log_grant.expires_at_ms"),
            self.expires_at_ms,
            1,
            u64::MAX,
        )
    }
}

/// Non-empty, at most `max` bytes, every byte printable ASCII other than a space.
fn header_text(at: At, value: &str, max: usize) -> Result<(), ValidationError> {
    if value.is_empty() {
        return Err(at.fail(Rule::Empty));
    }
    if value.len() > max {
        return Err(at.fail(Rule::TooLong));
    }
    if !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(at.fail(Rule::BadChars));
    }
    Ok(())
}

/// The store's URL, as [`LogSink::endpoint`] says. An `http` URL to a host that is not
/// loopback breaks [`Rule::Escapes`]; every other fault of the form, [`Rule::BadChars`].
fn endpoint(at: At, url: &str) -> Result<(), ValidationError> {
    if url.len() > limits::MAX_LOG_ENDPOINT_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return Err(at.fail(Rule::BadChars)),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    let host = authority_host(authority).ok_or_else(|| at.fail(Rule::BadChars))?;
    if secure || is_loopback(host) {
        Ok(())
    } else {
        Err(at.fail(Rule::Escapes))
    }
}

/// The host of `authority`, `host` or `host:port`, or `None` when it is anything else. The
/// host is a name or an IPv4 address (`[A-Za-z0-9.-]`, non-empty) or an IPv6 address in
/// brackets; the port 1 to 65535 in decimal digits. A user part (`@`), a path (`/`), a
/// query (`?`) or a fragment (`#`) holds a byte neither allows, so it is refused here.
fn authority_host(authority: &str) -> Option<&str> {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !authority.ends_with(']') => (host, Some(port)),
        _ => (authority, None),
    };
    let port_ok = port.is_none_or(|port| {
        port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p| p > 0)
    });
    let name = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let v6 = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .is_some_and(|inner| inner.parse::<Ipv6Addr>().is_ok());
    (port_ok && (name || v6)).then_some(host)
}

/// `localhost`, an IPv4 address in `127.0.0.0/8` or `[::1]`.
fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The job's prefix, as [`LogSink::prefix`] says: its last segment must be `job_id`, so a
/// start cannot point one job's logs at another job's prefix.
fn prefix(at: At, value: &str, job_id: &str) -> Result<(), ValidationError> {
    if value.len() > limits::MAX_LOG_PREFIX_LEN || job_id.len() > limits::MAX_LOG_JOB_ID_LEN {
        return Err(at.fail(Rule::TooLong));
    }
    let Some(path) = value.strip_suffix('/') else {
        return Err(at.fail(Rule::BadChars));
    };
    for segment in path.split('/') {
        let alphabet = !segment.is_empty() && segment.bytes().all(check::ident_byte);
        if !alphabet || segment == "." || segment == ".." {
            return Err(at.fail(Rule::BadChars));
        }
    }
    let last = path.rsplit('/').next();
    if last != Some(job_id) {
        return Err(at.fail(Rule::Inconsistent));
    }
    Ok(())
}

/// Decodes an optional log field leniently: a value that is not a `T` reads as `None`, so
/// it never fails the message around it.
pub(crate) fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Value::deserialize(deserializer).map(|value| T::deserialize(value).ok())
}
