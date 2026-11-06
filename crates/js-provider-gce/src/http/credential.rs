//! The credential the client calls with: the application-default credentials. A key file
//! of a service account, which `GOOGLE_APPLICATION_CREDENTIALS` names, is exchanged for an
//! access token with a signed assertion; with no key file, the metadata server of the
//! machine the scheduler runs on hands out the token of its service account.
//!
//! No `Debug` output, error or log line of this module holds a private key, an assertion
//! or an access token.

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use js_logstore::sigv4::canonical_query;
use js_logstore::{
    Clock as WallClock, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Map, Value, json};

use crate::error::ApiError;

/// The variable that names the service-account key file. It is the provider-standard
/// credential variable, so reading it is the exception the engineering rules allow for
/// provider-standard credentials.
pub const KEY_FILE_VARIABLE: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// Where the metadata server hands out the access token of the machine's service account:
/// the link-local address the cloud documents for it. It is reached over the transport the
/// client is given, so a test answers for it without a socket.
const METADATA_TOKEN_URL: &str =
    "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token";

/// The scopes an assertion asks for: Compute Engine, and the read-only Cloud Billing
/// catalog.
const SCOPES: &str = "https://www.googleapis.com/auth/compute https://www.googleapis.com/auth/cloud-billing.readonly";

/// How long an assertion is valid, in seconds: the longest the cloud allows.
const ASSERTION_LIFETIME: u64 = 3600;

/// The grant type of an assertion exchange.
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// A token is fetched again once it has this long to live or less.
const REFRESH_BEFORE: Duration = Duration::from_secs(300);

/// The longest a token is kept, in seconds, whatever lifetime the answer states.
const LONGEST_KEPT: u64 = 86_400;

/// The first line of a PEM private key.
const PEM_BEGIN: &str = "-----BEGIN PRIVATE KEY-----";

/// The last line of a PEM private key.
const PEM_END: &str = "-----END PRIVATE KEY-----";

/// Why a service-account key file cannot be used. The text names the variable and the
/// problem and never what the file holds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The file `GOOGLE_APPLICATION_CREDENTIALS` names cannot be read; the text is the
    /// reader's own.
    #[error("GOOGLE_APPLICATION_CREDENTIALS: the key file cannot be read ({0})")]
    Read(String),
    /// The file is not JSON.
    #[error("GOOGLE_APPLICATION_CREDENTIALS: the key file is not JSON")]
    NotJson,
    /// The file is JSON of another type of credential (its `type` is not
    /// `service_account`).
    #[error("GOOGLE_APPLICATION_CREDENTIALS: the key file is not a service-account key")]
    NotServiceAccount,
    /// The key lacks a field, or the field is not text.
    #[error("GOOGLE_APPLICATION_CREDENTIALS: the service-account key has no text field {0}")]
    Missing(&'static str),
    /// The `private_key` is not an RSA key in PKCS#8 PEM form.
    #[error("GOOGLE_APPLICATION_CREDENTIALS: the private_key is not an RSA PKCS#8 PEM key")]
    BadPrivateKey,
}

/// A service account's key: its email, the address its assertions are exchanged at, and
/// its signing key. The `Debug` text names the email and nothing else.
pub struct ServiceAccountKey {
    client_email: String,
    token_uri: String,
    key_id: Option<String>,
    pair: RsaKeyPair,
}

impl fmt::Debug for ServiceAccountKey {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_struct("ServiceAccountKey")
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

/// The DER bytes a PEM private key holds, or `None` when the text is not one.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let body = pem.trim().strip_prefix(PEM_BEGIN)?.strip_suffix(PEM_END)?;
    let packed: String = body.split_whitespace().collect();
    STANDARD.decode(packed).ok()
}

impl ServiceAccountKey {
    /// The key a service-account JSON key file holds (`type`, `client_email`,
    /// `private_key` and `token_uri`, and `private_key_id` when there is one).
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the bytes are not JSON, are a credential of another type,
    /// lack a field, or hold a private key that is not RSA PKCS#8.
    pub fn from_json(bytes: &[u8]) -> Result<Self, CredentialError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|_| CredentialError::NotJson)?;
        let text = |name: &'static str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .ok_or(CredentialError::Missing(name))
        };
        if text("type") != Ok("service_account") {
            return Err(CredentialError::NotServiceAccount);
        }
        let der = pem_body(text("private_key")?).ok_or(CredentialError::BadPrivateKey)?;
        let pair = RsaKeyPair::from_pkcs8(&der).map_err(|_| CredentialError::BadPrivateKey)?;
        Ok(Self {
            client_email: text("client_email")?.to_owned(),
            token_uri: text("token_uri")?.to_owned(),
            key_id: text("private_key_id").ok().map(str::to_owned),
            pair,
        })
    }

    /// The assertion to exchange for a token at `now`: a JWT signed RS256 whose claims
    /// are the account, the scopes, the token address, the time and the time an hour
    /// later. RSA PKCS#1 v1.5 signing is deterministic, so one key, one clock and one
    /// account always give the same text.
    fn assertion(&self, now: SystemTime) -> Result<String, ApiError> {
        let issued = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let mut header = Map::new();
        header.insert("alg".to_owned(), json!("RS256"));
        header.insert("typ".to_owned(), json!("JWT"));
        header.extend(
            self.key_id
                .as_deref()
                .map(|kid| ("kid".to_owned(), json!(kid))),
        );
        let claims = json!({
            "iss": self.client_email,
            "scope": SCOPES,
            "aud": self.token_uri,
            "iat": issued,
            "exp": issued + ASSERTION_LIFETIME,
        });
        let part = |value: &Value| URL_SAFE_NO_PAD.encode(value.to_string());
        let signing_input = format!("{}.{}", part(&Value::Object(header)), part(&claims));
        let mut signature = vec![0u8; self.pair.public().modulus_len()];
        let signed = self.pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        );
        signed
            .is_ok()
            .then(|| {
                format!(
                    "{signing_input}.{}",
                    URL_SAFE_NO_PAD.encode(signature.as_slice())
                )
            })
            .ok_or(ApiError::NoCredential)
    }
}

/// Where the credential comes from.
#[derive(Debug)]
pub enum Credentials {
    /// A service account's key, exchanged for tokens at the key's token address.
    ServiceAccount(ServiceAccountKey),
    /// The service account of the machine, read from the metadata server.
    MetadataServer,
}

impl Credentials {
    /// The credential `lookup` and `read` give: when the variable
    /// [`KEY_FILE_VARIABLE`] is set and not empty, the service-account key in the file it
    /// names (`read` reads a file), else the metadata server. `lookup` is asked for no
    /// other name.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] when the variable names a file that cannot be read or does not
    /// hold a service-account key.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        read: impl Fn(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<Self, CredentialError> {
        match lookup(KEY_FILE_VARIABLE).filter(|path| !path.is_empty()) {
            None => Ok(Self::MetadataServer),
            Some(path) => {
                let bytes = read(Path::new(&path)).map_err(CredentialError::Read)?;
                ServiceAccountKey::from_json(&bytes).map(Self::ServiceAccount)
            }
        }
    }

    /// The provider-standard credential: [`Credentials::from_lookup`] over the process
    /// environment and the file system. It is the one place this crate reads the
    /// environment: the variable names the application-default credential, which the
    /// engineering rules allow as a provider-standard credential and not a setting.
    ///
    /// # Errors
    ///
    /// As [`Credentials::from_lookup`].
    pub fn standard() -> Result<Self, CredentialError> {
        Self::from_lookup(Self::environment, Self::read_key_file)
    }

    /// The value of the environment variable `name`, when it is set to text.
    fn environment(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    /// The bytes of the file at `path`, or the reader's own words for why it cannot be
    /// read. It is the file reader of [`Credentials::standard`].
    ///
    /// # Errors
    ///
    /// The text of the I/O error.
    pub fn read_key_file(path: &Path) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|error| error.to_string())
    }
}

/// An access token and the first instant at which it is no longer worth using.
struct Token {
    value: String,
    expires_at: SystemTime,
}

/// The access token of a credential, with the one it last fetched.
pub(super) struct TokenSource {
    credentials: Credentials,
    cached: Mutex<Option<Token>>,
}

impl TokenSource {
    /// A source over `credentials`.
    pub(super) fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            cached: Mutex::new(None),
        }
    }

    /// The access token to call with now: the one kept while it has more than five
    /// minutes to live, else a new one. The lock is held across the fetch, so concurrent
    /// callers fetch once.
    ///
    /// # Errors
    ///
    /// [`ApiError::NoCredential`] when no token is handed out, and [`ApiError::Timeout`]
    /// when the token call timed out (it may have taken effect, but it changes nothing in
    /// the project).
    pub(super) fn access_token(
        &self,
        transport: &dyn Transport,
        wall: &dyn WallClock,
    ) -> Result<String, ApiError> {
        let now = wall.now();
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let kept = cached
            .as_ref()
            .filter(|token| token.expires_at > now + REFRESH_BEFORE)
            .map(|token| token.value.clone());
        if let Some(value) = kept {
            return Ok(value);
        }
        let fetched = self.fetch(transport, now)?;
        let value = fetched.value.clone();
        *cached = Some(fetched);
        Ok(value)
    }

    /// A new token.
    fn fetch(&self, transport: &dyn Transport, now: SystemTime) -> Result<Token, ApiError> {
        match &self.credentials {
            Credentials::MetadataServer => {
                let request = HttpRequest {
                    method: Method::Get,
                    url: METADATA_TOKEN_URL.to_owned(),
                    headers: vec![("metadata-flavor".to_owned(), "Google".to_owned())],
                    body: Vec::new(),
                };
                exchange(transport, request, now)
            }
            Credentials::ServiceAccount(key) => key.assertion(now).and_then(|assertion| {
                let form = [
                    ("grant_type".to_owned(), GRANT_TYPE.to_owned()),
                    ("assertion".to_owned(), assertion),
                ];
                let request = HttpRequest {
                    method: Method::Post,
                    url: key.token_uri.clone(),
                    headers: vec![(
                        "content-type".to_owned(),
                        "application/x-www-form-urlencoded".to_owned(),
                    )],
                    body: canonical_query(&form).into_bytes(),
                };
                exchange(transport, request, now)
            }),
        }
    }
}

/// One token request: the token its answer holds.
fn exchange(
    transport: &dyn Transport,
    request: HttpRequest,
    now: SystemTime,
) -> Result<Token, ApiError> {
    let answer = transport.send(request).map_err(|error| match error {
        TransportError::Timeout => ApiError::Timeout,
        _ => ApiError::NoCredential,
    })?;
    read_token(&answer, now).ok_or(ApiError::NoCredential)
}

/// The token a 200 answer holds (`access_token` and `expires_in`, in seconds).
fn read_token(answer: &HttpResponse, now: SystemTime) -> Option<Token> {
    let value: Value = serde_json::from_slice(&answer.body).ok()?;
    let lifetime = value.get("expires_in")?.as_u64()?.min(LONGEST_KEPT);
    let token = Token {
        value: value.get("access_token")?.as_str()?.to_owned(),
        expires_at: now + Duration::from_secs(lifetime),
    };
    (answer.status == 200).then_some(token)
}
