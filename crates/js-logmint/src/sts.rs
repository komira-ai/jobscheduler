//! The production minter: one STS `AssumeRole` call per mint.
//!
//! The call is `POST {sts_endpoint}/` with the content type `application/x-www-form-urlencoded`
//! and the body
//! `Action=AssumeRole&Version=..&RoleArn=..&RoleSessionName=..&DurationSeconds=..&Policy=..`,
//! every value percent-encoded, signed with AWS Signature Version 4 for the service `sts` in the
//! configured region with the mint key. The session name is `js-` and the first 32 hex digits of
//! the SHA-256 of the host's agent id (an agent id may be longer than STS takes or hold
//! characters it refuses), so the store's audit trail names the host.
//!
//! The answer is read by a small strict scanner, not an XML parser: the one `<Credentials>`
//! element, and in it exactly one each of `AccessKeyId`, `SecretAccessKey`, `SessionToken` and
//! `Expiration` (RFC 3339), each a run of printable ASCII without `<` or `&`. A missing or
//! malformed field, an `Expiration` less than [`EXPIRY_MARGIN`] from now, or a lifetime shorter
//! than half of what was asked is [`MintError::Refused`]: the minter never hands out a
//! credential about to die. The credential's `expires_at` is the answer's `Expiration`.

use std::fmt;
use std::time::{Duration, SystemTime};

use js_logstore::sigv4::{Scope, Unsigned, sha256_hex, sign, uri_encode};
use js_logstore::{
    Clock, Credential, Endpoint, EndpointError, HttpRequest, Method, Transport, Unavailable,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::names::RoleArn;
use crate::policy::policy;
use crate::{MintError, MintRequest, Minter, Refusal};

/// The version of the STS query API the call is made in. A third-party constant: the identifier
/// AWS defines for the API, which has the form of a calendar date and must be sent exactly as
/// AWS spells it.
pub const STS_API_VERSION: &str = "2011-06-15";

/// The least time a minted credential must have left when it arrives.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// The content type of the call's body.
const FORM: &str = "application/x-www-form-urlencoded";

/// What every session name begins with.
const SESSION_PREFIX: &str = "js-";

/// The hex digits of the agent id's SHA-256 a session name keeps.
const SESSION_DIGITS: usize = 32;

/// The longest STS error code kept.
const MAX_CODE_LEN: usize = 64;

/// Why an answer that claims success is refused when a field is missing or malformed.
const MISSING: &str = "the answer lacks a well-formed credential";

/// Why an answer is refused when its credential expires too soon.
const TOO_SHORT: &str =
    "the credential expires within a minute or lives less than half of what was asked";

/// Where the STS is: `https://host[:port]`, or `http://` to a loopback address literal, with no
/// path, query or user, and the region calls are signed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StsEndpoint(Endpoint);

impl StsEndpoint {
    /// The STS at `base_url`, signed for `region`, under the URL and region rules of
    /// [`Endpoint::parse`].
    ///
    /// # Errors
    ///
    /// [`EndpointError`] names the first rule the arguments break.
    pub fn parse(base_url: &str, region: &str) -> Result<Self, EndpointError> {
        // An endpoint is parsed for a bucket; the STS has none, so a placeholder that passes
        // the bucket rule stands in. Path style keeps it out of the host and the path `/`
        // never names it.
        Endpoint::parse(base_url, region, "sts", true).map(Self)
    }

    /// The URL calls are POSTed to: the base URL and `/`.
    pub fn url(&self) -> String {
        self.0.url("/")
    }

    /// The region calls are signed for.
    pub fn region(&self) -> &str {
        self.0.region()
    }
}

/// The minter that calls the store's STS: one `AssumeRole` per mint, never retried.
pub struct StsMinter<T, C> {
    transport: T,
    clock: C,
    endpoint: StsEndpoint,
    role_arn: RoleArn,
    key: Credential,
}

impl<T, C> fmt::Debug for StsMinter<T, C> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("StsMinter")
            .field("endpoint", &self.endpoint)
            .field("role_arn", &self.role_arn)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl<T: Transport, C: Clock> StsMinter<T, C> {
    /// A minter that assumes `role_arn` at `endpoint` over `transport`, signing with the mint
    /// key `key` at the times `clock` reads.
    pub fn new(
        transport: T,
        clock: C,
        endpoint: StsEndpoint,
        role_arn: RoleArn,
        key: Credential,
    ) -> Self {
        Self {
            transport,
            clock,
            endpoint,
            role_arn,
            key,
        }
    }

    /// The signed `AssumeRole` request for `request`.
    fn request(&self, request: &MintRequest) -> HttpRequest {
        let session = session_name(&request.host);
        let seconds = request.ttl.as_secs().to_string();
        let policy = policy(&request.bucket, &request.prefix);
        let body = form(&[
            ("Action", "AssumeRole"),
            ("Version", STS_API_VERSION),
            ("RoleArn", self.role_arn.as_str()),
            ("RoleSessionName", &session),
            ("DurationSeconds", &seconds),
            ("Policy", &policy),
        ]);
        let host = self.endpoint.0.host();
        let mut headers = vec![("content-type".to_owned(), FORM.to_owned())];
        let unsigned = Unsigned {
            method: Method::Post.as_str(),
            host: &host,
            path: "/",
            query: &[],
            headers: &headers,
            sign_also: &["content-type"],
            body: body.as_bytes(),
        };
        let scope = Scope {
            region: self.endpoint.region(),
            service: "sts",
        };
        let signed = sign(&unsigned, &self.key, &scope, self.clock.now());
        headers.push(("host".to_owned(), host));
        headers.extend(signed.headers);
        HttpRequest {
            method: Method::Post,
            url: self.endpoint.url(),
            headers,
            body: body.into_bytes(),
        }
    }
}

impl<T: Transport, C: Clock> Minter for StsMinter<T, C> {
    fn mint(&self, request: &MintRequest) -> Result<Credential, MintError> {
        let answer = self
            .transport
            .send(self.request(request))
            .map_err(|error| MintError::Unavailable(Unavailable::Transport(error)))?;
        if answer.status != 200 {
            return Err(refusal(answer.status, &answer.body));
        }
        let credential = credential(&answer.body).map_err(MintError::Refused)?;
        let left = credential
            .expires_at
            .duration_since(self.clock.now())
            .unwrap_or_default();
        match left < EXPIRY_MARGIN || left < request.ttl / 2 {
            true => Err(MintError::Refused(Refusal::Answer(TOO_SHORT))),
            false => Ok(credential),
        }
    }
}

/// The session name of the host `host`: `js-` and the first 32 hex digits of its SHA-256.
fn session_name(host: &str) -> String {
    let digest = sha256_hex(host.as_bytes());
    format!("{SESSION_PREFIX}{}", &digest[..SESSION_DIGITS])
}

/// The form body of `pairs`, in their order, each name and value percent-encoded.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", uri_encode(name, false), uri_encode(value, false)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The error an answer other than 200 is, from its status and STS error code.
fn refusal(status: u16, body: &[u8]) -> MintError {
    let code = error_code(body);
    match (status, code.as_str()) {
        (400, "Throttling") | (429 | 500..=599, _) => {
            MintError::Unavailable(Unavailable::Status { status, code })
        }
        _ => MintError::Refused(Refusal::Status { status, code }),
    }
}

/// The `<Code>` of an STS error body when it is 1 to [`MAX_CODE_LEN`] ASCII letters and digits,
/// and otherwise empty.
fn error_code(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let code = text
        .split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map_or("", |(code, _)| code);
    let kept = code.len() <= MAX_CODE_LEN && code.bytes().all(|b| b.is_ascii_alphanumeric());
    match kept {
        true => code.to_owned(),
        false => String::new(),
    }
}

/// The credential of a successful answer's body.
fn credential(body: &[u8]) -> Result<Credential, Refusal> {
    let missing = || Refusal::Answer(MISSING);
    let text = std::str::from_utf8(body).map_err(|_| missing())?;
    let inner = element(text, "Credentials").ok_or_else(missing)?;
    let field = |name: &str| {
        element(inner, name)
            .filter(|value| plain(value))
            .ok_or_else(missing)
    };
    let expires_at =
        OffsetDateTime::parse(field("Expiration")?, &Rfc3339).map_err(|_| missing())?;
    Ok(Credential {
        access_key_id: field("AccessKeyId")?.to_owned(),
        secret_access_key: field("SecretAccessKey")?.to_owned(),
        session_token: Some(field("SessionToken")?.to_owned()),
        expires_at: SystemTime::from(expires_at),
    })
}

/// What the one element `name` of `text` holds; `None` when `text` has none or more than one.
fn element<'t>(text: &'t str, name: &str) -> Option<&'t str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let (_, rest) = text.split_once(open.as_str())?;
    let (inner, after) = rest.split_once(close.as_str())?;
    (!after.contains(open.as_str())).then_some(inner)
}

/// Whether `value` is a field's value the scanner takes: printable ASCII without `<` or `&`,
/// at least one byte.
fn plain(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'<' && b != b'&')
}
