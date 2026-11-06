//! The credential the client signs with: the provider-standard variables, or the role of
//! the machine the scheduler runs on, which the instance metadata service (IMDSv2) hands
//! out for a few hours at a time.
//!
//! No `Debug` output, error or log line of this module holds a secret access key or a
//! session token.

use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use js_logstore::{Clock as WallClock, Credential, HttpRequest, Method, Transport};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::ApiError;

/// The address of the instance metadata service: the link-local address the cloud
/// documents for it. It is reached over the transport the client is given, so a test
/// answers for it without a socket.
pub(crate) const IMDS: &str = "http://169.254.169.254";

/// The path that issues a session token for the metadata service.
const TOKEN_PATH: &str = "/latest/api/token";

/// The path that lists the role of the machine.
const ROLE_PATH: &str = "/latest/meta-data/iam/security-credentials/";

/// The header that asks for the lifetime of a session token, in seconds.
const TOKEN_TTL_HEADER: &str = "x-aws-ec2-metadata-token-ttl-seconds";

/// The header that carries a session token.
const TOKEN_HEADER: &str = "x-aws-ec2-metadata-token";

/// The lifetime asked for a session token: six hours, the longest the service grants.
const TOKEN_TTL: &str = "21600";

/// A role credential is fetched again once it has this long to live or less.
const REFRESH_BEFORE: Duration = Duration::from_secs(300);

/// The variable that holds the access key id.
const KEY_ID_VARIABLE: &str = "AWS_ACCESS_KEY_ID";
/// The variable that holds the secret access key.
const SECRET_VARIABLE: &str = "AWS_SECRET_ACCESS_KEY";
/// The variable that holds the session token of a temporary key.
const TOKEN_VARIABLE: &str = "AWS_SESSION_TOKEN";

/// A long-lived key: an access key id and its secret, and a session token when the key
/// is temporary. The `Debug` text names the access key id and nothing else.
#[derive(Clone)]
pub struct StaticKeys {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl fmt::Debug for StaticKeys {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("StaticKeys")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

impl StaticKeys {
    /// The key `access_key_id` with its `secret_access_key`, and a `session_token` when
    /// the key is temporary.
    pub fn new(access_key_id: &str, secret_access_key: &str, session_token: Option<&str>) -> Self {
        Self {
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: session_token.map(str::to_owned),
        }
    }

    /// The key the variables `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and, when the
    /// key is temporary, `AWS_SESSION_TOKEN` hold according to `lookup`. A variable that
    /// is empty counts as absent, and `None` is the answer when the key id or the secret
    /// is. `AWS_REGION` is not read: the region of the configuration is the one used.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let present = |name: &str| lookup(name).filter(|value| !value.is_empty());
        Some(Self {
            access_key_id: present(KEY_ID_VARIABLE)?,
            secret_access_key: present(SECRET_VARIABLE)?,
            session_token: present(TOKEN_VARIABLE),
        })
    }

    /// The key the process environment holds, as [`StaticKeys::from_lookup`] reads it.
    /// It is the one place this crate reads the environment: the variables are the
    /// provider-standard credential, which is secret material and not configuration.
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The key as the signer takes it. A long-lived key does not expire, so its expiry
    /// is a date far off.
    fn credential(&self) -> Credential {
        Credential {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: self.session_token.clone(),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(u32::MAX)),
        }
    }
}

/// Where the credential comes from.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// A key given up front.
    Keys(StaticKeys),
    /// The role of the machine, read from the instance metadata service and kept until
    /// five minutes before it expires.
    InstanceRole,
}

impl Credentials {
    /// The provider-standard credential: the key the environment holds
    /// ([`StaticKeys::from_env`]), else the role of the machine.
    pub fn standard() -> Self {
        StaticKeys::from_env().map_or(Self::InstanceRole, Self::Keys)
    }
}

/// A source of credentials with the role credential it last fetched.
pub(super) struct Resolver {
    source: Credentials,
    cached: Mutex<Option<Credential>>,
}

impl Resolver {
    /// A resolver over `source`.
    pub(super) fn new(source: Credentials) -> Self {
        Self {
            source,
            cached: Mutex::new(None),
        }
    }

    /// The credential to sign with now.
    ///
    /// # Errors
    ///
    /// [`ApiError::NoCredential`] when the source is the role of the machine and the
    /// metadata service does not hand it out.
    pub(super) fn credential(
        &self,
        transport: &dyn Transport,
        wall: &dyn WallClock,
    ) -> Result<Credential, ApiError> {
        match &self.source {
            Credentials::Keys(keys) => Ok(keys.credential()),
            Credentials::InstanceRole => self.role(transport, wall),
        }
    }

    /// The role credential: the one kept while it has more than five minutes to live,
    /// else a new one. The lock is held across the fetch, so concurrent callers fetch
    /// once.
    fn role(
        &self,
        transport: &dyn Transport,
        wall: &dyn WallClock,
    ) -> Result<Credential, ApiError> {
        let horizon = wall.now() + REFRESH_BEFORE;
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let kept = cached
            .as_ref()
            .filter(|credential| !credential.is_expired(horizon))
            .cloned();
        if let Some(credential) = kept {
            return Ok(credential);
        }
        let fetched = fetch(transport).ok_or(ApiError::NoCredential)?;
        *cached = Some(fetched.clone());
        Ok(fetched)
    }
}

/// One exchange with the metadata service: the body of a 200 answer as text.
fn imds(
    transport: &dyn Transport,
    method: Method,
    path: &str,
    header: (&str, &str),
) -> Option<String> {
    let request = HttpRequest {
        method,
        url: format!("{IMDS}{path}"),
        headers: vec![(header.0.to_owned(), header.1.to_owned())],
        body: Vec::new(),
    };
    let answer = transport.send(request).ok()?;
    let ok = answer.status == 200;
    String::from_utf8(answer.body).ok().filter(|_| ok)
}

/// The role credential of the machine: a session token, the role's name, then its
/// credential. `None` when any step does not answer.
fn fetch(transport: &dyn Transport) -> Option<Credential> {
    let token = imds(
        transport,
        Method::Put,
        TOKEN_PATH,
        (TOKEN_TTL_HEADER, TOKEN_TTL),
    )?;
    let session = (TOKEN_HEADER, token.as_str());
    let roles = imds(transport, Method::Get, ROLE_PATH, session)?;
    let role = roles.lines().next()?.trim();
    let document = imds(
        transport,
        Method::Get,
        &format!("{ROLE_PATH}{role}"),
        session,
    )?;
    role_credential(&document)
}

/// The credential the metadata service's JSON document holds: `AccessKeyId`,
/// `SecretAccessKey`, `Token` and `Expiration` (RFC 3339).
fn role_credential(document: &str) -> Option<Credential> {
    let value: Value = serde_json::from_str(document).ok()?;
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let expires = OffsetDateTime::parse(text("Expiration")?, &Rfc3339).ok()?;
    Some(Credential {
        access_key_id: text("AccessKeyId")?.to_owned(),
        secret_access_key: text("SecretAccessKey")?.to_owned(),
        session_token: Some(text("Token")?.to_owned()),
        expires_at: SystemTime::from(expires),
    })
}
