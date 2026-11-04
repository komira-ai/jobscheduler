//! Taking the manifest of one platform out of an image index, for a [`Kind`] that asks
//! for it ([`IndexPolicy::SelectPlatform`]). Both sources ([`crate::Repository`] and
//! [`crate::Layout`]) read a pin through [`read`], so they choose, and refuse, alike.
//!
//! The order of the checks, each with its own error:
//! 1. the index's bytes hash to the pin, before anything in them is believed
//!    ([`Error::ManifestDigest`]);
//! 2. the bytes are an index ([`Error::Malformed`]);
//! 3. the candidates are the entries whose `platform` is the kind's operating system and
//!    architecture, with a `variant` that is absent or `v8`; an entry with no `platform`
//!    is skipped;
//! 4. no candidate is [`Error::NoPlatform`];
//! 5. candidates with more than one distinct digest are [`Error::AmbiguousPlatform`]
//!    (the first is never picked); the same digest listed twice counts once;
//! 6. the candidate's own media type is one of the kind's manifest types; an index there
//!    is [`Error::Index`] (one level only) and anything else [`Error::Unsupported`].
//!
//! The caller then fetches the candidate by the digest the index gives and reads it with
//! [`crate::manifest::parse`], exactly as a manifest pinned by that digest.

use serde::Deserialize;

use crate::manifest::media::{DOCKER_LIST, OCI_INDEX};
use crate::manifest::{self, IndexPolicy, Kind, Manifest, Platform};
use crate::text::{escape, shown};
use crate::{Digest, Error};

/// The most platforms an error names; the rest are counted.
const LISTED: usize = 16;
/// The `variant` of an arm64 entry that is taken besides one with no variant.
const VARIANT: &str = "v8";

/// What a pin's bytes turned out to be.
#[derive(Debug)]
pub(crate) enum Read {
    /// A manifest of the kind, read from the bytes of the pin.
    Manifest(Manifest),
    /// An index; the manifest of the kind's platform is this digest, still to be
    /// fetched and read.
    Child(Digest),
}

/// Reads `bytes`, the document pinned by `pinned`: a manifest, or (only when `kind`
/// selects a platform) an index, whose platform manifest is then named.
///
/// # Errors
/// Those of [`manifest::parse`] for a manifest, and the ones listed in the module
/// documentation for an index.
pub(crate) fn read(bytes: &[u8], pinned: &Digest, kind: &Kind) -> Result<Read, Error> {
    if kind.index == IndexPolicy::SelectPlatform && names_index(bytes) {
        manifest::check_digest(bytes, pinned)?;
        return select(bytes, pinned, kind).map(Read::Child);
    }
    manifest::parse(bytes, pinned, kind).map(Read::Manifest)
}

/// Whether `bytes` are JSON that calls itself an index, by media type or by listing
/// `manifests`. Anything else (including what is not JSON) is left to
/// [`manifest::parse`] to say.
fn names_index(bytes: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "mediaType")]
        media_type: Option<String>,
        manifests: Option<serde_json::Value>,
    }
    serde_json::from_slice::<Probe>(bytes).is_ok_and(|probe| {
        probe.manifests.is_some()
            || matches!(probe.media_type.as_deref(), Some(OCI_INDEX | DOCKER_LIST))
    })
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    platform: Option<Listed>,
}

/// The platform an index entry states.
#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    os: String,
    #[serde(default)]
    architecture: String,
    variant: Option<String>,
}

impl Listed {
    /// Whether this is `want`, with a variant that is absent or `v8`.
    fn is(&self, want: Platform) -> bool {
        self.os == want.os
            && self.architecture == want.architecture
            && self
                .variant
                .as_deref()
                .is_none_or(|variant| variant == VARIANT)
    }

    /// `os/architecture`, and `/variant` when there is one.
    fn name(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}/{variant}", self.os, self.architecture),
            None => format!("{}/{}", self.os, self.architecture),
        }
    }
}

/// The platforms `entries` list, each cut and escaped, at most [`LISTED`] of them and a
/// count of the rest; `nothing` when none lists a platform.
fn listed(entries: &[Entry]) -> String {
    let names: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry.platform.as_ref())
        .map(Listed::name)
        .collect();
    if names.is_empty() {
        return "nothing".to_owned();
    }
    let mut parts: Vec<String> = names.iter().take(LISTED).map(|name| shown(name)).collect();
    if names.len() > LISTED {
        parts.push(format!("and {} more", names.len() - LISTED));
    }
    parts.join(", ")
}

/// The digest of the one manifest of `kind`'s platform that the index `bytes`, pinned by
/// `index` and already hashed to it, lists.
fn select(bytes: &[u8], index: &Digest, kind: &Kind) -> Result<Digest, Error> {
    let doc: Index = serde_json::from_slice(bytes).map_err(|e| {
        Error::Malformed(format!(
            "the index {index} is not an index: {}",
            escape(&e.to_string())
        ))
    })?;
    let want = kind.platform;
    let want_text = format!("{}/{}", want.os, want.architecture);
    let found: Vec<&Entry> = doc
        .manifests
        .iter()
        .filter(|entry| entry.platform.as_ref().is_some_and(|p| p.is(want)))
        .collect();
    if found.is_empty() {
        return Err(Error::NoPlatform {
            index: index.to_string(),
            want: want_text,
            listed: listed(&doc.manifests),
        });
    }
    let mut digests: Vec<Digest> = Vec::new();
    for entry in &found {
        let digest = Digest::parse(&entry.digest)?;
        if !digests.contains(&digest) {
            digests.push(digest);
        }
    }
    if digests.len() > 1 {
        let digests: Vec<String> = digests.iter().map(|d| shown(d.as_str())).collect();
        return Err(Error::AmbiguousPlatform {
            index: index.to_string(),
            want: want_text,
            digests: digests.join(", "),
        });
    }
    for entry in &found {
        check_media(entry, kind)?;
    }
    Ok(digests.swap_remove(0))
}

/// Refuses an index entry whose own media type is not a manifest of `kind`.
fn check_media(entry: &Entry, kind: &Kind) -> Result<(), Error> {
    let media = entry.media_type.as_str();
    if kind.manifest_types.contains(&media) {
        return Ok(());
    }
    if matches!(media, OCI_INDEX | DOCKER_LIST) {
        return Err(Error::Index(entry.digest.clone()));
    }
    Err(Error::Unsupported(format!(
        "the index entry {} has media type {}",
        shown(&entry.digest),
        shown(media)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::media::{DOCKER_MANIFEST, OCI_MANIFEST};

    fn hex(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn entry(media: &str, digest: &str, platform: &str) -> String {
        format!(r#"{{"mediaType":"{media}","digest":"{digest}","size":1{platform}}}"#)
    }

    fn on(os: &str, arch: &str) -> String {
        format!(r#","platform":{{"architecture":"{arch}","os":"{os}"}}"#)
    }

    fn index(entries: &[String]) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_INDEX}","manifests":[{}]}}"#,
            entries.join(",")
        )
        .into_bytes()
    }

    /// The digest `bytes`, pinned by their own digest, resolve to under `kind`: the
    /// platform manifest an index names, or a manifest's own.
    fn resolved(bytes: &[u8], kind: &Kind) -> Result<Digest, Error> {
        match read(bytes, &Digest::of(bytes), kind)? {
            Read::Child(digest) => Ok(digest),
            Read::Manifest(manifest) => Ok(manifest.digest().clone()),
        }
    }

    fn selected(bytes: &[u8]) -> Result<Digest, Error> {
        resolved(bytes, &Kind::bundle_from_index())
    }

    fn refused(bytes: &[u8]) -> String {
        selected(bytes).expect_err("refused").to_string()
    }

    /// Asserts the entry of the darwin/arm64 manifest is the one selected from an index
    /// that also lists Linux ones, in any order, and that the same digest listed twice
    /// is one candidate. Catches a selection that takes the first entry or counts a
    /// repeated digest as ambiguous.
    #[test]
    fn the_darwin_arm64_entry_is_selected() {
        let bytes = index(&[
            entry(OCI_MANIFEST, &hex(1), &on("linux", "amd64")),
            entry(OCI_MANIFEST, &hex(2), &on("darwin", "arm64")),
            entry(OCI_MANIFEST, &hex(3), &on("linux", "arm64")),
            entry(OCI_MANIFEST, &hex(2), &on("darwin", "arm64")),
        ]);
        assert_eq!(selected(&bytes).expect("selected").as_str(), hex(2));
    }

    /// Asserts a variant of `v8` is accepted, an absent one too, and any other variant
    /// is not a candidate. Catches an arm64 entry of another variant taken for the host.
    #[test]
    fn only_variant_v8_or_none_is_a_candidate() {
        let with = |variant: &str| {
            entry(
                OCI_MANIFEST,
                &hex(4),
                &format!(
                    r#","platform":{{"os":"darwin","architecture":"arm64","variant":"{variant}"}}"#
                ),
            )
        };
        assert_eq!(
            selected(&index(&[with("v8")])).expect("v8").as_str(),
            hex(4)
        );
        let said = refused(&index(&[with("v9")]));
        assert!(said.contains(r#"lists "darwin/arm64/v9""#), "{said}");
    }

    /// Asserts an entry with no platform is skipped, one with the right system and
    /// another architecture (or the reverse) is not a candidate, and the error says
    /// what is listed. Catches an entry with no platform taken as a match.
    #[test]
    fn entries_without_the_platform_are_not_candidates() {
        let bytes = index(&[
            entry(OCI_MANIFEST, &hex(1), ""),
            entry(OCI_MANIFEST, &hex(2), &on("darwin", "amd64")),
            entry(OCI_MANIFEST, &hex(3), &on("linux", "arm64")),
        ]);
        let said = refused(&bytes);
        assert!(said.contains("lists no darwin/arm64 manifest"), "{said}");
        assert!(
            said.ends_with(r#"it lists "darwin/amd64", "linux/arm64""#),
            "{said}"
        );
    }

    /// Asserts an index that lists no platform at all says `nothing`. Catches an empty
    /// listing printed as a dangling sentence.
    #[test]
    fn an_index_listing_no_platform_says_nothing() {
        let said = refused(&index(&[entry(OCI_MANIFEST, &hex(1), "")]));
        assert!(said.ends_with("it lists nothing"), "{said}");
        let said = refused(&index(&[]));
        assert!(said.ends_with("it lists nothing"), "{said}");
    }

    /// Asserts the listed platforms are cut at 16, with a count of the rest, and that
    /// control characters in a platform are escaped. Catches an error that grows with
    /// the index or prints a registry's text raw.
    #[test]
    fn the_listed_platforms_are_bounded_and_escaped() {
        let many: Vec<String> = (0..20u8)
            .map(|n| entry(OCI_MANIFEST, &hex(n), &on("linux", &format!("a{n}\\n"))))
            .collect();
        let said = refused(&index(&many));
        assert!(said.contains(r#""linux/a15\n""#), "{said}");
        assert!(!said.contains("a16"), "{said}");
        assert!(said.ends_with(", and 4 more"), "{said}");
        let exactly: Vec<String> = (0..16u8)
            .map(|n| entry(OCI_MANIFEST, &hex(n), &on("linux", "amd64")))
            .collect();
        assert!(!refused(&index(&exactly)).contains("more"));
    }

    /// Asserts two candidates with different digests are ambiguous and both are named.
    /// Catches the first of them being picked.
    #[test]
    fn two_different_candidates_are_ambiguous() {
        let bytes = index(&[
            entry(OCI_MANIFEST, &hex(5), &on("darwin", "arm64")),
            entry(OCI_MANIFEST, &hex(6), &on("darwin", "arm64")),
            entry(OCI_MANIFEST, &hex(5), &on("darwin", "arm64")),
        ]);
        let said = refused(&bytes);
        assert!(
            said.contains("more than one darwin/arm64 manifest"),
            "{said}"
        );
        assert!(
            said.ends_with(&format!(r#""{}", "{}""#, hex(5), hex(6))),
            "{said}"
        );
    }

    /// Asserts a candidate that is itself an index is `Error::Index` in either spelling,
    /// and any other media type is unsupported. Catches nested indexes being followed.
    #[test]
    fn a_candidate_must_be_a_manifest() {
        for nested in [OCI_INDEX, DOCKER_LIST] {
            let bytes = index(&[entry(nested, &hex(7), &on("darwin", "arm64"))]);
            let err = selected(&bytes).expect_err("nested");
            assert_eq!(
                err.to_string(),
                format!(
                    "{} names an image index; pin the manifest of one platform",
                    hex(7)
                )
            );
        }
        let bytes = index(&[entry("text/plain", &hex(7), &on("darwin", "arm64"))]);
        let err = selected(&bytes).expect_err("other");
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert!(err.to_string().contains(r#"media type "text/plain""#));
        let docker = index(&[entry(DOCKER_MANIFEST, &hex(7), &on("darwin", "arm64"))]);
        assert!(matches!(selected(&docker), Err(Error::Unsupported(_))));
    }

    /// Asserts a Docker manifest list is read like an OCI index, by its media type or
    /// by its `manifests`, and that an image kind takes the Docker manifest type.
    /// Catches the list spelling being refused or read as a manifest.
    #[test]
    fn a_docker_manifest_list_is_an_index() {
        let list = format!(
            r#"{{"schemaVersion":2,"mediaType":"{DOCKER_LIST}","manifests":[{}]}}"#,
            entry(DOCKER_MANIFEST, &hex(8), &on("darwin", "arm64"))
        )
        .into_bytes();
        let mut kind = Kind::image(Platform::DARWIN_ARM64);
        kind.index = IndexPolicy::SelectPlatform;
        assert_eq!(resolved(&list, &kind).expect("list").as_str(), hex(8));
        let bare = format!(
            r#"{{"manifests":[{}]}}"#,
            entry(OCI_MANIFEST, &hex(8), &on("darwin", "arm64"))
        );
        assert_eq!(selected(bare.as_bytes()).expect("bare").as_str(), hex(8));
        let by_media = format!(r#"{{"mediaType":"{DOCKER_LIST}"}}"#);
        assert!(refused(by_media.as_bytes()).contains("is not an index"));
    }

    /// Asserts index bytes that do not hash to the pin are refused as such, before
    /// they are read as an index. Catches an index believed under a digest it lacks.
    #[test]
    fn index_bytes_are_hashed_before_they_are_read() {
        let bytes = index(&[entry(OCI_MANIFEST, &hex(2), &on("darwin", "arm64"))]);
        let err =
            read(&bytes, &Digest::of(b"other"), &Kind::bundle_from_index()).expect_err("refused");
        assert!(matches!(err, Error::ManifestDigest { .. }), "{err}");
    }

    /// Asserts a malformed index, and a candidate with a bad digest, are refused with
    /// their own errors. Catches a panic or a silent skip on hostile index JSON.
    #[test]
    fn a_malformed_index_is_refused() {
        let said = refused(br#"{"mediaType":"application/vnd.oci.image.index.v1+json"}"#);
        assert!(said.contains("is not an index"), "{said}");
        let bytes = index(&[entry(OCI_MANIFEST, "sha256:XYZ", &on("darwin", "arm64"))]);
        assert!(matches!(selected(&bytes), Err(Error::Digest(_))));
    }

    /// Asserts a kind that refuses indexes refuses one, a selecting kind reads a plain
    /// manifest as a manifest, and bytes that are not JSON go to the manifest reader.
    /// Catches the policy being ignored in either direction.
    #[test]
    fn the_policy_decides_what_an_index_is() {
        let bytes = index(&[entry(OCI_MANIFEST, &hex(2), &on("darwin", "arm64"))]);
        let err = read(&bytes, &Digest::of(&bytes), &Kind::bundle()).expect_err("refused");
        assert!(matches!(err, Error::Index(_)), "{err}");
        let kind = Kind::bundle_from_index();
        let err = read(b"nope", &Digest::of(b"nope"), &kind).expect_err("refused");
        assert!(err.to_string().contains("not a manifest"), "{err}");
        let manifest = format!(
            r#"{{"schemaVersion":2,"artifactType":"application/vnd.komira.js.bundle.v1","config":{{"mediaType":"application/vnd.komira.js.bundle.config.v1+json","digest":"{}","size":1}},"layers":[]}}"#,
            hex(1)
        )
        .into_bytes();
        let own = resolved(&manifest, &kind).expect("manifest");
        assert_eq!(own, Digest::of(&manifest));
    }
}
