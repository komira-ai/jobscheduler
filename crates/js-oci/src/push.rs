//! Publishing to a registry: an artifact read from a [`BundleSource`] (an image-layout
//! directory a pack wrote, say) is put into a repository under a tag, and a tag that names a
//! different manifest is never written over.
//!
//! What a publish sends, in order:
//! 1. a GET of the tag's manifest, with a token asked for to pull and push: a tag that names
//!    the same manifest digest ends the publish with nothing sent ([`Published::AlreadyThere`]),
//!    one that names another digest is refused ([`Error::TagHeld`]), and a 404 goes on;
//! 2. for the config and each layer in turn, `POST /v2/<name>/blobs/uploads/` and a `PUT` of
//!    the whole blob to the upload's `Location` with `digest=` added to its query (the
//!    monolithic upload of the distribution spec). The `Location` must be on the registry's
//!    own scheme, host and port, since the token goes with the `PUT`;
//! 3. a `PUT` of the manifest's bytes, exactly as the source held them, to the tag, with the
//!    manifest's media type. A registry that says it stored another digest
//!    (`Docker-Content-Digest`) is refused.
//!
//! Every byte sent was checked on the way out of the source as on the way into a pull: the
//! manifest hashes to the digest asked for and is of the [`Kind`] asked for, and each blob has
//! its manifest's length and digest. One blob is held in memory at a time, while it is sent,
//! so a publish is for artifacts whose largest blob fits in memory.
//!
//! What it does not do: it mounts no blob from another repository, and it does not ask whether
//! a blob is there before sending it (the registry keeps one copy of a blob it already holds).
//! Between the tag's GET and its `PUT` another writer could tag something else; the `PUT`
//! would then replace it. A registry with one writer per tag, such as a release workflow's,
//! never meets that.

use std::fmt;

use bytes::Bytes;
use hyper::Method;
use hyper::StatusCode;
use hyper::header::LOCATION;

use crate::client::{Access, Call, Repository, query_escape, success};
use crate::http::resolve;
use crate::location::Authority;
use crate::manifest::{Descriptor, Kind, Manifest};
use crate::source::BundleSource;
use crate::text;
use crate::{Digest, Error};

/// The response header in which a registry says which digest it stored a manifest as.
const CONTENT_DIGEST: &str = "docker-content-digest";

/// The longest tag the distribution spec allows.
const MAX_TAG: usize = 128;

/// A tag of a repository: `[A-Za-z0-9_][A-Za-z0-9._-]*`, at most 128 characters, as the
/// distribution spec allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag(String);

impl Tag {
    /// `text` as a tag.
    ///
    /// # Errors
    /// [`Error::Tag`] when it is empty, longer than 128 characters, starts with `.` or `-`,
    /// or holds anything but ASCII letters, digits, `_`, `.` and `-`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut bytes = text.bytes();
        let first = bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
        let rest = bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if first && rest && text.len() <= MAX_TAG {
            Ok(Self(text.to_owned()))
        } else {
            Err(Error::Tag(text::shown(text)))
        }
    }

    /// The tag's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What [`Repository::publish`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Published {
    /// The tag named nothing: every blob and the manifest were sent, and the tag names the
    /// manifest now.
    Pushed,
    /// The tag already named this manifest: nothing was sent.
    AlreadyThere,
}

impl Repository<'_> {
    /// Puts the artifact `digest` names in `source` into this repository under `tag`: the
    /// steps are the module's. The repository's entry needs credentials that may push
    /// ([`crate::Allow::with_credentials`]) unless the registry lets anyone push.
    ///
    /// # Errors
    /// What the source says of the artifact (a manifest that does not hash to `digest` or is
    /// not of `kind`, a blob that does not match its descriptor); [`Error::TagHeld`] when
    /// `tag` names another manifest; [`Error::Upload`] when the registry's answer to an upload
    /// is not one a publish can go on from; and the errors of a pull for each request
    /// ([`Error::Status`], [`Error::Auth`], [`Error::Transport`] and the rest).
    pub async fn publish(
        &self,
        source: &dyn BundleSource,
        digest: &Digest,
        kind: &Kind,
        tag: &Tag,
    ) -> Result<Published, Error> {
        let manifest = source.fetch_manifest(digest, kind).await?;
        match self.tagged(tag, kind).await? {
            Some(held) if held == *manifest.digest() => return Ok(Published::AlreadyThere),
            Some(held) => {
                return Err(Error::TagHeld {
                    tag: tag.to_string(),
                    held: held.to_string(),
                    wanted: manifest.digest().to_string(),
                });
            }
            None => {}
        }
        let config = source.fetch_config(&manifest, kind).await?;
        self.upload(manifest.config(), config).await?;
        for layer in manifest.layers() {
            let mut bytes = Vec::new();
            source.fetch_blob(layer, &mut bytes).await?;
            self.upload(layer, bytes).await?;
        }
        self.put_manifest(tag, &manifest).await?;
        Ok(Published::Pushed)
    }

    /// The digest of the manifest the registry holds under `tag`, or `None` when it answers
    /// 404 (no such tag, or no such repository yet).
    async fn tagged(&self, tag: &Tag, kind: &Kind) -> Result<Option<Digest>, Error> {
        let url = self.url("manifests", tag.as_str());
        let accept = kind.accept();
        let http = self.http();
        http.within(http.timeouts.manifest, &url, async {
            let call = Call {
                accept: Some(&accept),
                ..write(Method::GET, None, Bytes::new())
            };
            let response = self.call(&url, &call).await?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let response = success(&url, response)?;
            let bytes = http
                .read_limited(&url, response, kind.limits.manifest)
                .await?;
            Ok(Some(Digest::of(&bytes)))
        })
        .await
    }

    /// Sends `bytes`, the blob `blob` describes, as one monolithic upload.
    async fn upload(&self, blob: &Descriptor, bytes: Vec<u8>) -> Result<(), Error> {
        let start = self.url("blobs", "uploads/");
        let http = self.http();
        http.within(http.timeouts.blob, &start, async {
            let began = self
                .call(&start, &write(Method::POST, None, Bytes::new()))
                .await?;
            let began = success(&start, began)?;
            let at = began
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| upload(&start, "the upload's answer has no Location"))?;
            let at = self.own(&start, at)?;
            let separator = if at.contains('?') { '&' } else { '?' };
            let url = format!(
                "{at}{separator}digest={}",
                query_escape(blob.digest().as_str())
            );
            let octets = write(Method::PUT, Some(OCTET_STREAM), Bytes::from(bytes));
            let done = self.call(&url, &octets).await?;
            success(&url, done).map(drop)
        })
        .await
    }

    /// The upload URL `location` names, resolved against `from`, when it is on this
    /// repository's scheme, host and port.
    fn own(&self, from: &str, location: &str) -> Result<String, Error> {
        let url = resolve(from, location)?;
        let (scheme, default) = self.scheme();
        let ours = url
            .parse::<hyper::Uri>()
            .ok()
            .filter(|uri| uri.scheme_str() == Some(scheme))
            .and_then(|uri| Authority::of_uri_or(&uri, default))
            .is_some_and(|at| at == *self.location().endpoint());
        if ours {
            Ok(url)
        } else {
            Err(upload(
                from,
                &format!(
                    "the upload goes on at {}, not on the registry's own host",
                    text::url(&url)
                ),
            ))
        }
    }

    /// PUTs `manifest`'s bytes under `tag`.
    async fn put_manifest(&self, tag: &Tag, manifest: &Manifest) -> Result<(), Error> {
        let url = self.url("manifests", tag.as_str());
        let http = self.http();
        http.within(http.timeouts.manifest, &url, async {
            let body = Bytes::copy_from_slice(manifest.bytes());
            let call = write(Method::PUT, Some(manifest.media_type()), body);
            let response = success(&url, self.call(&url, &call).await?)?;
            let stored = response
                .headers()
                .get(CONTENT_DIGEST)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
            match stored {
                Some(stored) if stored != manifest.digest().as_str() => Err(upload(
                    &url,
                    &format!(
                        "the registry stored the manifest as {}, not {}",
                        text::shown(&stored),
                        manifest.digest()
                    ),
                )),
                _ => Ok(()),
            }
        })
        .await
    }
}

/// The media type of a blob's bytes as an upload sends them.
const OCTET_STREAM: &str = "application/octet-stream";

/// A request of a publish: it carries a token asked for to pull and push.
fn write(method: Method, content_type: Option<&str>, body: Bytes) -> Call<'_> {
    Call {
        method,
        accept: None,
        content_type,
        body,
        access: Access::Push,
    }
}

/// [`Error::Upload`] for the request to `url`.
fn upload(url: &str, why: &str) -> Error {
    Error::Upload {
        what: text::url(url),
        why: why.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a tag rule looser or stricter than the distribution spec's: the first
    /// character, the characters after it, and the length limit on each side of it.
    #[test]
    fn a_tag_is_what_the_distribution_spec_allows() {
        for good in ["1", "_x", "v1.2.3_rc-1", "A", &"a".repeat(MAX_TAG)] {
            assert_eq!(Tag::parse(good).unwrap().as_str(), good);
        }
        for bad in [
            "",
            ".x",
            "-x",
            "a/b",
            "a:b",
            "a b",
            "é",
            &"a".repeat(MAX_TAG + 1),
        ] {
            assert!(
                matches!(Tag::parse(bad), Err(Error::Tag(_))),
                "{bad:?} was taken"
            );
        }
        assert_eq!(
            Tag::parse("a/b").unwrap_err().to_string(),
            r#""a/b" is not a tag: ASCII letters, digits, `_`, `.` and `-`, not starting with `.` or `-`, at most 128 characters"#
        );
        assert_eq!(Tag::parse("v1").unwrap().to_string(), "v1");
    }
}
