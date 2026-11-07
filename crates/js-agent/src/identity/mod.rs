//! The cloud's signed identity of this machine, read from the cloud's metadata service once
//! at start and sent in the hello ([`js_agent_wire::Hello::identity`]), where a provider can
//! check that the machine is the instance it launched.
//!
//! `--identity-source` picks where it comes from:
//!
//! - `none` (the default): no identity; the hello is the one an agent without the flag sends.
//! - `aws`: IMDSv2. A `PUT` of `/latest/api/token` gets a session token, and two `GET`s with
//!   it read `/latest/dynamic/instance-identity/document` and `.../signature`. The hello
//!   carries both bodies exactly as served.
//! - `gce`: the metadata server's identity token,
//!   `/computeMetadata/v1/instance/service-accounts/default/identity` with the audience of
//!   `--identity-audience` and `format=full`, asked with `Metadata-Flavor: Google`. The token
//!   is the identity's `document` and the `signature` is empty: the token carries its own,
//!   which is how the provider's verifier reads it.
//!
//! The service is reached at `--identity-metadata-url`, by default the cloud's own address,
//! over plain HTTP (neither service speaks TLS), with no proxy and no redirect, and each
//! request is bounded by [`METADATA_LIMIT`]. A source that is configured and cannot be read
//! stops the agent before it says hello, naming the source and the URL: it never falls back
//! to a hello without an identity. The identity is read once and not refreshed.
//!
//! No transport header carries the identity: the providers read it from the hello alone.

use std::fmt;
use std::time::Duration;

use clap::{Args, ValueEnum};
use js_agent_wire::SignedIdentity;
use thiserror::Error;
use ureq::Agent;
use ureq::http::Response;

/// The longest one request to the metadata service may take, connect included. Both
/// services answer on the machine's own link in milliseconds; a host off the cloud never
/// answers, and the agent then stops with an error rather than wait.
pub(crate) const METADATA_LIMIT: Duration = Duration::from_secs(5);

/// The address of the EC2 instance metadata service.
pub(crate) const AWS_METADATA_URL: &str = "http://169.254.169.254";

/// The address of the Compute Engine metadata server.
pub(crate) const GCE_METADATA_URL: &str = "http://metadata.google.internal";

/// The largest body read from the metadata service. A document or a token is a few KiB;
/// the hello's own caps ([`js_agent_wire::limits::MAX_IDENTITY_DOCUMENT_LEN`]) are
/// checked after.
const BODY_CAP: u64 = 64 * 1024;

/// The lifetime asked for an IMDSv2 session token, in seconds. The token is used for the
/// two reads right after it is issued.
const IMDS_TOKEN_TTL_SECONDS: &str = "60";

/// Where the signed identity comes from (`--identity-source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum IdentitySourceKind {
    /// No identity.
    #[default]
    None,
    /// The EC2 instance identity document and its signature, through IMDSv2.
    Aws,
    /// The Compute Engine identity token.
    Gce,
}

impl IdentitySourceKind {
    /// The flag's spelling of the source.
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Aws => "aws",
            Self::Gce => "gce",
        }
    }
}

impl fmt::Display for IdentitySourceKind {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// The flags of the signed identity.
#[derive(Debug, Clone, Default, Args)]
pub struct IdentityFlags {
    /// Where the hello's signed identity of this machine comes from: `none` (the default)
    /// sends none; `aws` reads the instance identity document and its signature through
    /// IMDSv2; `gce` reads the metadata server's identity token for `--identity-audience`.
    /// Read once when the agent starts; a source that cannot be read stops the agent,
    /// naming the source and the URL, before it says hello.
    #[arg(
        long = "identity-source",
        value_enum,
        default_value_t = IdentitySourceKind::None,
        value_name = "none|aws|gce"
    )]
    pub source: IdentitySourceKind,
    /// The base URL of the metadata service, an `http://` URL. By default the cloud's own:
    /// `http://169.254.169.254` for `aws`, `http://metadata.google.internal` for `gce`.
    /// Needs `--identity-source aws` or `gce`.
    #[arg(long = "identity-metadata-url", value_name = "URL")]
    pub metadata_url: Option<String>,
    /// The audience the identity token is issued for (its `aud` claim). Required with
    /// `--identity-source gce`, refused with the others.
    #[arg(long = "identity-audience", value_name = "AUDIENCE")]
    pub audience: Option<String>,
}

/// A source configured and not read: the source, the URL that failed and why.
#[derive(Debug, Error)]
#[error("--identity-source {kind}: cannot read the signed identity from {url}: {reason}")]
pub(crate) struct FetchError {
    kind: IdentitySourceKind,
    url: String,
    reason: String,
}

/// Where the hello's signed identity comes from.
pub(crate) trait IdentitySource {
    /// The identity to send, `None` when this source sends none.
    ///
    /// # Errors
    ///
    /// The source is configured and its service could not be read, or answered with
    /// something other than 200 and a body.
    fn fetch(&self) -> Result<Option<SignedIdentity>, FetchError>;
}

/// `--identity-source none`.
struct NoIdentity;

impl IdentitySource for NoIdentity {
    fn fetch(&self) -> Result<Option<SignedIdentity>, FetchError> {
        Ok(None)
    }
}

/// `--identity-source aws`: IMDSv2.
struct Imds {
    service: Metadata,
}

impl IdentitySource for Imds {
    fn fetch(&self) -> Result<Option<SignedIdentity>, FetchError> {
        let service = &self.service;
        let token_url = service.url("/latest/api/token");
        let token = service.read(
            &token_url,
            service
                .agent
                .put(&token_url)
                .header(
                    "X-aws-ec2-metadata-token-ttl-seconds",
                    IMDS_TOKEN_TTL_SECONDS,
                )
                .send_empty(),
        )?;
        let signed = |path: &str| {
            let url = service.url(path);
            let answer = service
                .agent
                .get(&url)
                .header("X-aws-ec2-metadata-token", &token)
                .call();
            service.read(&url, answer)
        };
        let document = signed("/latest/dynamic/instance-identity/document")?;
        let signature = signed("/latest/dynamic/instance-identity/signature")?;
        Ok(Some(SignedIdentity {
            document,
            signature,
        }))
    }
}

/// `--identity-source gce`: the identity token.
struct GceMetadata {
    service: Metadata,
    audience: String,
}

impl IdentitySource for GceMetadata {
    fn fetch(&self) -> Result<Option<SignedIdentity>, FetchError> {
        let service = &self.service;
        let url = service.url(&format!(
            "/computeMetadata/v1/instance/service-accounts/default/identity?audience={}&format=full",
            percent_encode(&self.audience)
        ));
        let answer = service
            .agent
            .get(&url)
            .header("Metadata-Flavor", "Google")
            .call();
        let token = service.read(&url, answer)?;
        Ok(Some(SignedIdentity {
            document: token,
            signature: String::new(),
        }))
    }
}

/// One metadata service: its base URL and the client that reaches it.
struct Metadata {
    kind: IdentitySourceKind,
    base: String,
    agent: Agent,
}

impl Metadata {
    fn new(kind: IdentitySourceKind, base: String) -> Self {
        let agent: Agent = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(METADATA_LIMIT))
            .max_idle_connections(0)
            .build()
            .into();
        Self { kind, base, agent }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn fail(&self, url: &str, reason: String) -> FetchError {
        FetchError {
            kind: self.kind,
            url: url.to_owned(),
            reason,
        }
    }

    /// The body of a 200 answer from `url`, which must not be empty.
    fn read(
        &self,
        url: &str,
        answer: Result<Response<ureq::Body>, ureq::Error>,
    ) -> Result<String, FetchError> {
        let mut answer = answer.map_err(|why| self.fail(url, why.to_string()))?;
        let status = answer.status().as_u16();
        if status != 200 {
            return Err(self.fail(url, format!("it answered {status}")));
        }
        let body = answer
            .body_mut()
            .with_config()
            .limit(BODY_CAP)
            .read_to_string()
            .map_err(|why| self.fail(url, why.to_string()))?;
        if body.is_empty() {
            return Err(self.fail(url, "it answered an empty body".to_owned()));
        }
        Ok(body)
    }
}

/// `text` for a query value: every byte but the unreserved ones of RFC 3986
/// (`A-Z a-z 0-9 - . _ ~`) as `%XX`.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The base URL to use: the flag's, with a trailing `/` dropped, or the source's default.
/// The flag's must be `http://` and a host, and hold no user, query or fragment: the paths
/// are appended to it, and a query or fragment would swallow them. The refusal does not
/// repeat the value, which could hold a password.
fn base_url(given: Option<&str>, default: &str) -> Result<String, String> {
    let Some(given) = given else {
        return Ok(default.to_owned());
    };
    let host = given.strip_prefix("http://").filter(|rest| {
        !rest.is_empty() && !rest.starts_with('/') && !rest.contains(['@', '?', '#'])
    });
    match host {
        Some(_) => Ok(given.trim_end_matches('/').to_owned()),
        None => Err("--identity-metadata-url: not an http:// URL of a host \
             (with no user, query or fragment)"
            .to_owned()),
    }
}

/// The source the flags name, checked.
///
/// # Errors
///
/// A flag that its source does not take, a missing audience, or a metadata URL that is not
/// `http://` with a host; the text names the flag.
pub(crate) fn source(flags: &IdentityFlags) -> Result<Box<dyn IdentitySource>, String> {
    let url = flags.metadata_url.as_deref();
    let audience = flags.audience.as_deref();
    match (flags.source, audience) {
        (IdentitySourceKind::None, _) if url.is_some() => {
            Err("--identity-metadata-url: needs --identity-source aws or gce".to_owned())
        }
        (IdentitySourceKind::None | IdentitySourceKind::Aws, Some(_)) => {
            Err("--identity-audience: needs --identity-source gce".to_owned())
        }
        (IdentitySourceKind::None, None) => Ok(Box::new(NoIdentity)),
        (IdentitySourceKind::Aws, None) => {
            let base = base_url(url, AWS_METADATA_URL)?;
            Ok(Box::new(Imds {
                service: Metadata::new(IdentitySourceKind::Aws, base),
            }))
        }
        (IdentitySourceKind::Gce, None | Some("")) => {
            Err("--identity-audience: required with --identity-source gce".to_owned())
        }
        (IdentitySourceKind::Gce, Some(audience)) => {
            let base = base_url(url, GCE_METADATA_URL)?;
            Ok(Box::new(GceMetadata {
                service: Metadata::new(IdentitySourceKind::Gce, base),
                audience: audience.to_owned(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a query value keeps the unreserved bytes and writes every other byte,
    /// multi-byte UTF-8 included, as upper-case `%XX`. It catches an audience URL sent
    /// unencoded, whose `&` or `?` would end the parameter early.
    #[test]
    fn a_query_value_is_percent_encoded() {
        assert_eq!(percent_encode("Az09-._~"), "Az09-._~");
        assert_eq!(
            percent_encode("https://a.example/x?y=1&z"),
            "https%3A%2F%2Fa.example%2Fx%3Fy%3D1%26z"
        );
        assert_eq!(percent_encode("é "), "%C3%A9%20");
    }

    /// Asserts that without the flag the source's default address is used, that the flag's
    /// URL is used without a closing `/`, and that a URL that is not `http://` with a host,
    /// or holds a user, a query or a fragment, is refused without repeating the value. It
    /// catches a metadata URL that sends the requests somewhere the operator did not mean,
    /// and a refusal that prints a password given in the URL.
    #[test]
    fn the_metadata_url_is_the_default_or_a_plain_http_url() {
        assert_eq!(
            base_url(None, AWS_METADATA_URL).as_deref(),
            Ok("http://169.254.169.254")
        );
        assert_eq!(
            base_url(None, GCE_METADATA_URL).as_deref(),
            Ok("http://metadata.google.internal")
        );
        let given = base_url(Some("http://127.0.0.1:8080/base/"), AWS_METADATA_URL);
        assert_eq!(given.as_deref(), Ok("http://127.0.0.1:8080/base"));
        for bad in [
            "http://",
            "http:///path",
            "https://host",
            "host",
            "http://user:secret-pw@host",
            "http://host?x=1",
            "http://host#f",
        ] {
            assert_eq!(
                base_url(Some(bad), AWS_METADATA_URL),
                Err(
                    "--identity-metadata-url: not an http:// URL of a host (with no user, \
                     query or fragment)"
                        .to_owned()
                ),
                "{bad}"
            );
        }
    }

    /// Asserts that a source names itself as the flag spells it.
    #[test]
    fn a_source_prints_as_the_flag_spells_it() {
        let names: Vec<String> = [
            IdentitySourceKind::None,
            IdentitySourceKind::Aws,
            IdentitySourceKind::Gce,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(names, ["none", "aws", "gce"]);
    }
}
