//! Pulling from a registry: manifests, configs and blobs, each checked against the
//! digest it is named by before anyone uses it.

use std::net::IpAddr;
use std::sync::Mutex;

use async_trait::async_trait;
use hyper::header::{ACCEPT, AUTHORIZATION, HeaderName, WWW_AUTHENTICATE};
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::http::{Http, Roots, Timeouts};
use crate::location::Authority;
use crate::manifest::{self, Descriptor, Kind, Manifest};
use crate::{Digest, Error, Location, Policy};

/// Where the bytes of a blob go as they arrive.
///
/// The bytes are not trusted while they are written: the client checks the length and the
/// digest after the last one, and says so by returning `Ok`. A sink must treat what it
/// received as unverified (a file in a private directory the caller removes, not a path
/// anyone reads) until [`Repository::fetch_blob`] has returned `Ok`.
#[async_trait]
pub trait BlobSink: Send {
    /// Takes the next `chunk` of the blob.
    ///
    /// # Errors
    /// The chunk could not be stored.
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()>;

    /// Called once, after the last chunk and after the length and digest were verified.
    ///
    /// # Errors
    /// The blob could not be completed (a file could not be flushed).
    async fn finish(&mut self) -> std::io::Result<()>;
}

#[async_trait]
impl BlobSink for Vec<u8> {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl BlobSink for tokio::fs::File {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.write_all(chunk).await
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        self.flush().await
    }
}

/// A registry client. It reaches only what its [`Policy`] allows, speaks https only, and
/// trusts the roots it was made with.
#[derive(Debug)]
pub struct Client {
    http: Http,
}

impl Client {
    /// A client that trusts the host's system roots.
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self {
            http: Http::new(policy, Roots::System),
        }
    }

    /// A client that trusts exactly `roots`, for a registry with a private authority.
    #[must_use]
    pub fn with_roots(policy: Policy, roots: Vec<CertificateDer<'static>>) -> Self {
        Self {
            http: Http::new(policy, Roots::Given(roots)),
        }
    }

    /// Replaces the timeouts.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.http.timeouts = timeouts;
        self
    }

    /// Resolves `host` to `ip` without asking the resolver. The address is still
    /// checked against the policy.
    #[must_use]
    pub fn with_resolved(mut self, host: &str, ip: IpAddr) -> Self {
        self.http.resolve_to(host, ip);
        self
    }

    /// The repository `location` names.
    ///
    /// # Errors
    /// [`Error::NotAllowed`] when the policy does not list the registry and namespace.
    pub fn repository(&self, location: Location) -> Result<Repository<'_>, Error> {
        self.http.policy.admit(&location)?;
        Ok(Repository {
            http: &self.http,
            location,
            token: Mutex::new(None),
        })
    }
}

/// One repository of a registry, with the token the registry gave for it. Its methods
/// take `&self`, so blobs can be fetched concurrently.
#[derive(Debug)]
pub struct Repository<'c> {
    http: &'c Http,
    location: Location,
    token: Mutex<Option<String>>,
}

impl Repository<'_> {
    fn url(&self, kind: &str, reference: &str) -> String {
        format!(
            "https://{}/v2/{}/{kind}/{reference}",
            self.location.endpoint().header(),
            self.location.repository()
        )
    }

    fn token(&self) -> Option<String> {
        self.token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The manifest pinned by `digest`: its bytes hash to `digest`, it is not an index,
    /// and it is of `kind`.
    ///
    /// # Errors
    /// The registry cannot be reached or refuses, or the manifest is not an acceptable
    /// manifest of `kind` with that digest.
    pub async fn fetch_manifest(&self, digest: &Digest, kind: &Kind) -> Result<Manifest, Error> {
        let url = self.url("manifests", digest.as_str());
        let response = self.get(&url, Some(&kind.accept())).await?;
        let bytes = self
            .http
            .read_limited(&url, response, kind.limits.manifest)
            .await?;
        manifest::parse(&bytes, digest, kind)
    }

    /// The config blob of `manifest`, verified, and checked to be for `kind`'s platform.
    ///
    /// # Errors
    /// The blob fails its digest or size, is not JSON, or states another platform.
    pub async fn fetch_config(&self, manifest: &Manifest, kind: &Kind) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        self.fetch_blob(&manifest.config, &mut bytes).await?;
        manifest::check_platform(kind, &bytes)?;
        Ok(bytes)
    }

    /// The blob `blob` into `sink`, hashed as it arrives. The sink is finished only when
    /// the blob is exactly `blob.size` bytes and hashes to `blob.digest`; on any error
    /// the caller discards what the sink received.
    ///
    /// # Errors
    /// The registry cannot be reached or refuses, the blob is longer or shorter than the
    /// manifest says or does not hash to its digest, or the sink fails.
    pub async fn fetch_blob(
        &self,
        blob: &Descriptor,
        sink: &mut dyn BlobSink,
    ) -> Result<(), Error> {
        let url = self.url("blobs", blob.digest.as_str());
        let response = self.get(&url, None).await?;
        let mut body = response.into_body();
        let mut hasher = Sha256::new();
        let mut got = 0u64;
        let size_error = |got| Error::BlobSize {
            digest: blob.digest.to_string(),
            declared: blob.size,
            got,
        };
        while let Some(data) = self.http.chunk(&url, &mut body).await? {
            got += data.len() as u64;
            if got > blob.size {
                return Err(size_error(got));
            }
            hasher.update(&data);
            sink.write(&data).await.map_err(Error::Sink)?;
        }
        if got != blob.size {
            return Err(size_error(got));
        }
        let actual = Digest::finish(hasher.finalize());
        if actual != blob.digest {
            return Err(Error::BlobDigest {
                digest: blob.digest.to_string(),
                actual: actual.to_string(),
            });
        }
        sink.finish().await.map_err(Error::Sink)
    }

    /// GETs `url`, answering a Bearer challenge once, and returns a 2xx response.
    async fn get(
        &self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, Error> {
        let mut challenged = false;
        loop {
            let mut headers: Vec<(HeaderName, String)> = Vec::new();
            if let Some(accept) = accept {
                headers.push((ACCEPT, accept.to_owned()));
            }
            if let Some(token) = self.token() {
                headers.push((AUTHORIZATION, format!("Bearer {token}")));
            }
            let response = self.http.get(url, &headers).await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status != hyper::StatusCode::UNAUTHORIZED {
                return Err(Error::Status {
                    url: url.to_owned(),
                    status: status.as_u16(),
                });
            }
            if challenged {
                return Err(Error::Auth(format!(
                    "{url}: the registry refused the token it issued"
                )));
            }
            challenged = true;
            let challenge = response
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Auth(format!("{url}: 401 with no WWW-Authenticate")))?
                .to_owned();
            let token = self.fetch_token(&challenge).await?;
            *self
                .token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token);
        }
    }

    /// The anonymous pull token a `Bearer` challenge asks for, from an endpoint on the
    /// registry's host or another allow-listed one.
    async fn fetch_token(&self, challenge: &str) -> Result<String, Error> {
        let params = Challenge::parse(challenge)?;
        let realm = params
            .realm
            .parse::<hyper::Uri>()
            .ok()
            .filter(|uri| uri.scheme_str() == Some("https"))
            .and_then(|uri| Authority::of_uri(&uri))
            .filter(|host| self.http.policy.realm_ok(host, self.location.endpoint()));
        if realm.is_none() {
            return Err(Error::Realm(params.realm));
        }
        let scope = params
            .scope
            .unwrap_or_else(|| format!("repository:{}:pull", self.location.repository()));
        let mut url = params.realm;
        url.push(if url.contains('?') { '&' } else { '?' });
        if let Some(service) = params.service {
            url.push_str(&format!("service={}&", query_escape(&service)));
        }
        url.push_str(&format!("scope={}", query_escape(&scope)));
        let response = self.http.get(&url, &[]).await?;
        if !response.status().is_success() {
            return Err(Error::Status {
                url,
                status: response.status().as_u16(),
            });
        }
        let body = self.http.read_limited(&url, response, 64 << 10).await?;
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = serde_json::from_slice(&body)
            .map_err(|e| Error::Auth(format!("{url}: the token answer is not JSON: {e}")))?;
        token
            .token
            .or(token.access_token)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::Auth(format!("{url}: the answer holds no token")))
    }
}

/// The parameters of a `WWW-Authenticate: Bearer` challenge.
#[derive(Debug, PartialEq, Eq)]
struct Challenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl Challenge {
    fn parse(header: &str) -> Result<Self, Error> {
        let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(Error::Auth(format!(
                "the registry asks for {scheme:?} authentication; only anonymous Bearer tokens are supported"
            )));
        }
        let (mut realm, mut service, mut scope) = (None, None, None);
        for (key, value) in parameters(rest) {
            match key.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value),
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
        }
        let realm = realm.ok_or_else(|| Error::Auth("the challenge has no realm".to_owned()))?;
        Ok(Self {
            realm,
            service,
            scope,
        })
    }
}

/// The `key=value` and `key="quoted, value"` pairs of a challenge's parameters.
fn parameters(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = text.trim_start();
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            quoted.split_once('"').unwrap_or((quoted, ""))
        } else {
            after.split_once(',').unwrap_or((after, ""))
        };
        pairs.push((key.to_owned(), value.trim().to_owned()));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    pairs
}

/// `value` as a URL query component: everything but unreserved characters and `:` `/`
/// percent-encoded.
fn query_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a challenge read wrongly: quoted values with commas and spaces,
    /// unquoted values, any case of the scheme and keys, and what is not Bearer.
    #[test]
    fn a_bearer_challenge_is_read() {
        let hub = Challenge::parse(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:library/x:pull,push""#,
        )
        .unwrap();
        assert_eq!(hub.realm, "https://auth.example/token");
        assert_eq!(hub.service.as_deref(), Some("registry.example"));
        assert_eq!(hub.scope.as_deref(), Some("repository:library/x:pull,push"));
        let loose = Challenge::parse("bearer Realm=https://a/t, service=s").unwrap();
        assert_eq!(
            loose,
            Challenge {
                realm: "https://a/t".to_owned(),
                service: Some("s".to_owned()),
                scope: None
            }
        );
        let extra = Challenge::parse(r#"Bearer realm="r",error="invalid_token""#).unwrap();
        assert_eq!(extra.realm, "r");
        assert!(
            Challenge::parse("Basic realm=\"x\"")
                .unwrap_err()
                .to_string()
                .contains("Basic")
        );
        assert!(
            Challenge::parse("Bearer service=\"x\"")
                .unwrap_err()
                .to_string()
                .contains("realm")
        );
        assert!(
            Challenge::parse("Bearer")
                .unwrap_err()
                .to_string()
                .contains("realm")
        );
    }

    /// Catches a scope or service escaped wrongly in the token request.
    #[test]
    fn query_values_are_escaped() {
        assert_eq!(
            query_escape("repository:library/busybox:pull"),
            "repository:library/busybox:pull"
        );
        assert_eq!(query_escape("a b&c=d,é"), "a%20b%26c%3Dd%2C%C3%A9");
    }
}
