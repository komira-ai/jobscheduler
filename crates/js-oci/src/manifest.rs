//! Reading a manifest and a config: checked against the digest the artifact is pinned
//! by, and against what the caller's [`Kind`] of artifact takes.

use serde::Deserialize;

use crate::text::{clip, escape, shown};
use crate::{Digest, Error};

/// Media types of the formats this client reads and the artifact types it knows.
pub mod media {
    /// An OCI image manifest.
    pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    /// A Docker schema 2 image manifest.
    pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
    /// An OCI image index, which is always refused.
    pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
    /// A Docker manifest list, which is always refused.
    pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
    /// An OCI image config.
    pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
    /// A Docker image config.
    pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
    /// An uncompressed tar layer of an image.
    pub const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
    /// A gzip tar layer of an image.
    pub const LAYER_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    /// A gzip tar layer of a Docker image.
    pub const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
    /// The artifact type of a bundle.
    pub const BUNDLE_ARTIFACT_TYPE: &str = "application/vnd.komira.js.bundle.v1";
    /// The config of a bundle.
    pub const BUNDLE_CONFIG: &str = "application/vnd.komira.js.bundle.config.v1+json";
    /// A zstd-compressed tar layer of a bundle.
    pub const BUNDLE_LAYER_TAR_ZSTD: &str = "application/vnd.komira.js.bundle.layer.v1.tar+zstd";
    /// An uncompressed tar layer of a bundle.
    pub const BUNDLE_LAYER_TAR: &str = "application/vnd.komira.js.bundle.layer.v1.tar";
}

use media::{DOCKER_LIST, OCI_INDEX};

/// What a pull may read, so a wrong pin or a hostile registry cannot fill the memory or
/// the disk of a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The manifest, in bytes.
    pub manifest: u64,
    /// The config blob, in bytes.
    pub config: u64,
    /// The layers of an artifact.
    pub layers: usize,
    /// The sum of the config's and the layers' sizes, as the manifest declares them.
    pub total_bytes: u64,
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The platform an artifact is for, as its config names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    /// The operating system (`darwin`, `linux`).
    pub os: &'static str,
    /// The architecture as images name it (`arm64`, `amd64`).
    pub architecture: &'static str,
}

impl Platform {
    /// Apple silicon macOS, the platform of bundles.
    pub const DARWIN_ARM64: Self = Self {
        os: "darwin",
        architecture: "arm64",
    };

    /// The Linux platform of the Rust architecture `arch` (`std::env::consts::ARCH`).
    ///
    /// # Errors
    /// [`Error::Arch`] when the architecture has no image platform name here.
    pub fn linux(arch: &str) -> Result<Self, Error> {
        let architecture = match arch {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => return Err(Error::Arch(other.to_owned())),
        };
        Ok(Self {
            os: "linux",
            architecture,
        })
    }
}

/// A kind of artifact: what its manifest, config and layers may be, which platform it is
/// for, and how much of it may be read. A manifest of another kind is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Kind {
    /// The media types a manifest may have.
    pub manifest_types: &'static [&'static str],
    /// The `artifactType` the manifest must state; `None` takes any.
    pub artifact_type: Option<&'static str>,
    /// The media types a config may have.
    pub config_types: &'static [&'static str],
    /// The media types a layer may have. The client hands layer bytes to the caller's
    /// sink as they are; decoding them is the caller's.
    pub layer_types: &'static [&'static str],
    /// The platform the config must name.
    pub platform: Platform,
    /// Whether the config must state its `os` and `architecture`. If not, a field it
    /// leaves out is not checked.
    pub platform_stated: bool,
    /// What may be read.
    pub limits: Limits,
}

impl Kind {
    /// A container image for `platform`: Docker and OCI manifests, tar and gzip tar
    /// layers, and a config that may leave its platform out.
    #[must_use]
    pub fn image(platform: Platform) -> Self {
        Self {
            manifest_types: &[media::OCI_MANIFEST, media::DOCKER_MANIFEST],
            artifact_type: None,
            config_types: &[media::OCI_CONFIG, media::DOCKER_CONFIG],
            layer_types: &[
                media::LAYER_TAR,
                media::LAYER_TAR_GZIP,
                media::DOCKER_LAYER_GZIP,
            ],
            platform,
            platform_stated: false,
            limits: Limits {
                manifest: 4 * MIB,
                config: MIB,
                layers: 128,
                total_bytes: 16 * GIB,
            },
        }
    }

    /// A bundle: an OCI manifest of the bundle artifact type for darwin/arm64, whose
    /// config states its platform, with at most 32 layers and 16 GiB.
    #[must_use]
    pub fn bundle() -> Self {
        Self {
            manifest_types: &[media::OCI_MANIFEST],
            artifact_type: Some(media::BUNDLE_ARTIFACT_TYPE),
            config_types: &[media::BUNDLE_CONFIG],
            layer_types: &[media::BUNDLE_LAYER_TAR_ZSTD, media::BUNDLE_LAYER_TAR],
            platform: Platform::DARWIN_ARM64,
            platform_stated: true,
            limits: Limits {
                manifest: 4 * MIB,
                config: MIB,
                layers: 32,
                total_bytes: 16 * GIB,
            },
        }
    }

    /// The `Accept` header of a manifest request: the types this kind takes, and the
    /// index types, which a registry then answers with (so the refusal can say index)
    /// rather than with a 404.
    pub(crate) fn accept(&self) -> String {
        let mut types: Vec<&str> = self.manifest_types.to_vec();
        types.extend([OCI_INDEX, DOCKER_LIST]);
        types.join(", ")
    }
}

/// A blob a manifest names. It cannot be made outside this crate: the only descriptors
/// there are come out of a [`Manifest`] that passed the checks of its [`Kind`], so the
/// kind's media types and limits hold for every blob that is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    media_type: String,
    digest: Digest,
    size: u64,
}

impl Descriptor {
    /// The blob's media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The blob's digest.
    #[must_use]
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The blob's length in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// A manifest that passed the checks of its [`Kind`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    digest: Digest,
    bytes: Vec<u8>,
    artifact_type: Option<String>,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

impl Manifest {
    /// The digest the manifest's bytes were pinned by.
    #[must_use]
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The manifest exactly as received: it hashes to [`Manifest::digest`], so these
    /// are the bytes to store.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The `artifactType` the manifest states, if it does.
    #[must_use]
    pub fn artifact_type(&self) -> Option<&str> {
        self.artifact_type.as_deref()
    }

    /// The config blob.
    #[must_use]
    pub fn config(&self) -> &Descriptor {
        &self.config
    }

    /// The layers, in the manifest's order.
    #[must_use]
    pub fn layers(&self) -> &[Descriptor] {
        &self.layers
    }
}

#[derive(Deserialize)]
struct Document {
    #[serde(rename = "schemaVersion")]
    schema_version: Option<u64>,
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    #[serde(rename = "artifactType")]
    artifact_type: Option<String>,
    manifests: Option<serde_json::Value>,
    config: Option<Wire>,
    layers: Option<Vec<Wire>>,
}

#[derive(Deserialize)]
struct Wire {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    urls: Vec<String>,
}

impl Wire {
    fn descriptor(self) -> Result<Descriptor, Error> {
        Ok(Descriptor {
            digest: Digest::parse(&self.digest)?,
            media_type: self.media_type,
            size: self.size,
        })
    }
}

/// Reads `bytes`, the manifest pinned by `pinned`.
///
/// # Errors
/// The bytes do not hash to `pinned`, are an image index, state another artifact type
/// than `kind` takes, use a schema, media type or layer kind `kind` does not take
/// (foreign layers, layers served from elsewhere), are not a manifest, or are beyond
/// `kind`'s limits.
pub(crate) fn parse(bytes: &[u8], pinned: &Digest, kind: &Kind) -> Result<Manifest, Error> {
    let actual = Digest::of(bytes);
    if actual != *pinned {
        return Err(Error::ManifestDigest {
            pinned: pinned.to_string(),
            actual: actual.to_string(),
        });
    }
    let doc: Document = serde_json::from_slice(bytes).map_err(|e| {
        Error::Malformed(format!(
            "the manifest {pinned} is not a manifest: {}",
            escape(&e.to_string())
        ))
    })?;
    let media = doc.media_type.as_deref();
    if doc.manifests.is_some() || matches!(media, Some(OCI_INDEX | DOCKER_LIST)) {
        return Err(Error::Index(pinned.to_string()));
    }
    if doc.schema_version != Some(2) {
        return Err(Error::Unsupported(format!(
            "the manifest {pinned} has schema version {:?}; only version 2 is supported",
            doc.schema_version
        )));
    }
    if let Some(media) = media
        && !kind.manifest_types.contains(&media)
    {
        return Err(Error::Unsupported(format!(
            "the manifest {pinned} has media type {}",
            shown(media)
        )));
    }
    if let Some(want) = kind.artifact_type
        && doc.artifact_type.as_deref() != Some(want)
    {
        return Err(Error::ArtifactType {
            want: want.to_owned(),
            have: doc.artifact_type.as_deref().map(clip),
        });
    }
    let missing = |what: &str| Error::Malformed(format!("the manifest {pinned} has no {what}"));
    let config = doc.config.ok_or_else(|| missing("config"))?;
    if !kind.config_types.contains(&config.media_type.as_str()) {
        return Err(Error::Unsupported(format!(
            "the config has media type {}",
            shown(&config.media_type)
        )));
    }
    let layers = doc.layers.ok_or_else(|| missing("layers"))?;
    let limits = kind.limits;
    if layers.len() > limits.layers {
        return Err(Error::TooLarge {
            what: format!("the manifest's {} layers", layers.len()),
            limit: limits.layers as u64,
        });
    }
    if config.size > limits.config {
        return Err(Error::TooLarge {
            what: "the config".to_owned(),
            limit: limits.config,
        });
    }
    let total = layers
        .iter()
        .fold(config.size, |sum, layer| sum.saturating_add(layer.size));
    if total > limits.total_bytes {
        return Err(Error::TooLarge {
            what: "the artifact's blobs".to_owned(),
            limit: limits.total_bytes,
        });
    }
    for layer in &layers {
        let foreign = !layer.urls.is_empty() || layer.media_type.contains("foreign");
        if foreign || !kind.layer_types.contains(&layer.media_type.as_str()) {
            return Err(Error::Unsupported(format!(
                "the layer {} has media type {} (or is served from elsewhere than the registry), which is not taken here",
                shown(&layer.digest),
                shown(&layer.media_type)
            )));
        }
    }
    let layers = layers
        .into_iter()
        .map(Wire::descriptor)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Manifest {
        digest: pinned.clone(),
        bytes: bytes.to_vec(),
        artifact_type: doc.artifact_type,
        config: config.descriptor()?,
        layers,
    })
}

/// Refuses a config whose stated platform is not `kind`'s.
pub(crate) fn check_platform(kind: &Kind, config: &[u8]) -> Result<(), Error> {
    #[derive(Deserialize)]
    struct Stated {
        os: Option<String>,
        architecture: Option<String>,
    }
    let stated: Stated = serde_json::from_slice(config).map_err(|e| {
        Error::Malformed(format!(
            "the config is not JSON: {}",
            escape(&e.to_string())
        ))
    })?;
    let field_ok = |have: &Option<String>, want: &str| match have {
        Some(have) => have == want,
        None => !kind.platform_stated,
    };
    let want = kind.platform;
    if field_ok(&stated.os, want.os) && field_ok(&stated.architecture, want.architecture) {
        return Ok(());
    }
    let say = |field: &Option<String>| escape(field.as_deref().unwrap_or("unstated"));
    Err(Error::Platform {
        want: format!("{}/{}", want.os, want.architecture),
        have: format!("{}/{}", say(&stated.os), say(&stated.architecture)),
    })
}

#[cfg(test)]
mod tests {
    use super::media::{BUNDLE_ARTIFACT_TYPE, BUNDLE_CONFIG, BUNDLE_LAYER_TAR_ZSTD};
    use super::*;

    fn hex(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn bundle(artifact_type: &str, extra: &str) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","artifactType":"{artifact_type}",
            "config":{{"mediaType":"{BUNDLE_CONFIG}","digest":"{}","size":10}},
            "layers":[{{"mediaType":"{BUNDLE_LAYER_TAR_ZSTD}","digest":"{}","size":20}},
            {{"mediaType":"{BUNDLE_LAYER_TAR_ZSTD}","digest":"{}","size":30}}]{extra}}}"#,
            media::OCI_MANIFEST,
            hex(1),
            hex(2),
            hex(3),
        )
        .into_bytes()
    }

    fn parsed(bytes: &[u8], kind: &Kind) -> Result<Manifest, Error> {
        parse(bytes, &Digest::of(bytes), kind)
    }

    fn refused(bytes: &[u8], kind: &Kind) -> String {
        parsed(bytes, kind).expect_err("refused").to_string()
    }

    /// Catches a manifest read wrongly: the config and layers with their types, sizes
    /// and order, the artifact type, and the pin it was read by.
    #[test]
    fn a_bundle_manifest_gives_its_config_and_layers_in_order() {
        let bytes = bundle(BUNDLE_ARTIFACT_TYPE, "");
        let m = parsed(&bytes, &Kind::bundle()).expect("parses");
        assert_eq!(m.digest, Digest::of(&bytes));
        assert_eq!(m.artifact_type.as_deref(), Some(BUNDLE_ARTIFACT_TYPE));
        assert_eq!(m.config.digest.as_str(), hex(1));
        assert_eq!(
            (m.config.size, m.config.media_type.as_str()),
            (10, BUNDLE_CONFIG)
        );
        let layers: Vec<_> = m
            .layers
            .iter()
            .map(|l| (l.digest.to_string(), l.size))
            .collect();
        assert_eq!(layers, [(hex(2), 20), (hex(3), 30)]);
    }

    /// Catches a manifest accepted under a digest it does not have.
    #[test]
    fn bytes_that_do_not_hash_to_the_pin_are_refused() {
        let bytes = bundle(BUNDLE_ARTIFACT_TYPE, "");
        let other = Digest::of(b"other");
        let err = parse(&bytes, &other, &Kind::bundle()).expect_err("refused");
        assert!(matches!(err, Error::ManifestDigest { .. }), "{err}");
    }

    /// Catches an image index read as an image: both index spellings are refused as an
    /// index, for any kind, before schema or media type are looked at.
    #[test]
    fn an_index_is_refused_in_both_spellings() {
        let by_media = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
        let by_list = br#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json"}"#;
        let by_field = br#"{"schemaVersion":9,"manifests":[]}"#;
        for bytes in [&by_media[..], &by_list[..], &by_field[..]] {
            for kind in [Kind::bundle(), Kind::image(Platform::DARWIN_ARM64)] {
                assert!(matches!(parsed(bytes, &kind), Err(Error::Index(_))));
            }
        }
    }

    /// Catches a manifest of another artifact type, or none, taken for a bundle; an
    /// image kind takes any.
    #[test]
    fn the_artifact_type_must_be_the_kinds() {
        let wrong = bundle("application/vnd.example.other", "");
        let err = parsed(&wrong, &Kind::bundle()).expect_err("refused");
        assert_eq!(
            err.to_string(),
            "the manifest has artifact type Some(\"application/vnd.example.other\"), not application/vnd.komira.js.bundle.v1"
        );
        let none = br#"{"schemaVersion":2,"config":{"mediaType":"application/vnd.komira.js.bundle.config.v1+json","digest":"sha256:0101010101010101010101010101010101010101010101010101010101010101","size":1},"layers":[]}"#;
        let err = parsed(none, &Kind::bundle()).expect_err("refused");
        assert!(
            matches!(err, Error::ArtifactType { have: None, .. }),
            "{err}"
        );
    }

    /// Catches the other refusals of a manifest: schema, media type, a missing config or
    /// layers, config and layer types outside the kind's lists, foreign layers, and a
    /// digest that is not sha256 hex.
    #[test]
    fn a_manifest_outside_the_kinds_lists_is_refused() {
        let kind = Kind::bundle();
        let doc = |head: &str, config: &str, layers: &str| {
            format!(r#"{{{head}"artifactType":"{BUNDLE_ARTIFACT_TYPE}"{config}{layers}}}"#)
                .into_bytes()
        };
        let good_config = format!(
            r#","config":{{"mediaType":"{BUNDLE_CONFIG}","digest":"{}","size":1}}"#,
            hex(1)
        );
        let layer = |media: &str, extra: &str, digest: &str| {
            format!(r#","layers":[{{"mediaType":"{media}","digest":"{digest}","size":1{extra}}}]"#)
        };
        let ok_layer = layer(BUNDLE_LAYER_TAR_ZSTD, "", &hex(2));
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("not json", b"nope".to_vec(), "not a manifest"),
            (
                "schema",
                doc(r#""schemaVersion":1,"#, &good_config, &ok_layer),
                "schema version",
            ),
            (
                "no schema",
                doc("", &good_config, &ok_layer),
                "schema version",
            ),
            (
                "media type",
                doc(
                    r#""schemaVersion":2,"mediaType":"text/plain","#,
                    &good_config,
                    &ok_layer,
                ),
                "media type \"text/plain\"",
            ),
            (
                "no config",
                doc(r#""schemaVersion":2,"#, "", &ok_layer),
                "no config",
            ),
            (
                "config type",
                doc(
                    r#""schemaVersion":2,"#,
                    &format!(
                        r#","config":{{"mediaType":"text/plain","digest":"{}","size":1}}"#,
                        hex(1)
                    ),
                    &ok_layer,
                ),
                "config has media type",
            ),
            (
                "no layers",
                doc(r#""schemaVersion":2,"#, &good_config, ""),
                "no layers",
            ),
            (
                "layer type",
                doc(
                    r#""schemaVersion":2,"#,
                    &good_config,
                    &layer("application/vnd.oci.image.layer.v1.tar+gzip", "", &hex(2)),
                ),
                "not taken here",
            ),
            (
                "foreign media",
                doc(
                    r#""schemaVersion":2,"#,
                    &good_config,
                    &layer(
                        "application/vnd.oci.image.layer.nondistributable.v1.tar+foreign",
                        "",
                        &hex(2),
                    ),
                ),
                "not taken here",
            ),
            (
                "urls",
                doc(
                    r#""schemaVersion":2,"#,
                    &good_config,
                    &layer(
                        BUNDLE_LAYER_TAR_ZSTD,
                        r#","urls":["https://elsewhere/x"]"#,
                        &hex(2),
                    ),
                ),
                "elsewhere than the registry",
            ),
            (
                "layer digest",
                doc(
                    r#""schemaVersion":2,"#,
                    &good_config,
                    &layer(BUNDLE_LAYER_TAR_ZSTD, "", "sha256:XYZ"),
                ),
                "not sha256",
            ),
            (
                "config digest",
                doc(
                    r#""schemaVersion":2,"#,
                    &format!(
                        r#","config":{{"mediaType":"{BUNDLE_CONFIG}","digest":"md5:1","size":1}}"#
                    ),
                    &ok_layer,
                ),
                "not sha256",
            ),
        ];
        for (name, bytes, text) in cases {
            let said = refused(&bytes, &kind);
            assert!(said.contains(text), "{name}: {said}");
        }
    }

    /// Catches a limit not enforced: layer count, the declared config size, and the
    /// total of the declared sizes, saturating rather than wrapping.
    #[test]
    fn declared_sizes_and_counts_beyond_the_limits_are_refused() {
        let bytes = bundle(BUNDLE_ARTIFACT_TYPE, "");
        let mut kind = Kind::bundle();
        kind.limits.layers = 1;
        assert!(matches!(
            parsed(&bytes, &kind),
            Err(Error::TooLarge { limit: 1, .. })
        ));
        let mut kind = Kind::bundle();
        kind.limits.config = 9;
        assert!(matches!(
            parsed(&bytes, &kind),
            Err(Error::TooLarge { limit: 9, .. })
        ));
        let mut kind = Kind::bundle();
        kind.limits.total_bytes = 59;
        assert!(matches!(
            parsed(&bytes, &kind),
            Err(Error::TooLarge { limit: 59, .. })
        ));
        kind.limits.total_bytes = 60;
        parsed(&bytes, &kind).expect("exactly at the limit");
        let huge = format!(
            r#"{{"schemaVersion":2,"artifactType":"{BUNDLE_ARTIFACT_TYPE}","config":{{"mediaType":"{BUNDLE_CONFIG}","digest":"{}","size":1}},"layers":[{{"mediaType":"{BUNDLE_LAYER_TAR_ZSTD}","digest":"{}","size":18446744073709551615}},{{"mediaType":"{BUNDLE_LAYER_TAR_ZSTD}","digest":"{}","size":5}}]}}"#,
            hex(1), hex(2), hex(3)
        )
        .into_bytes();
        assert!(matches!(
            parsed(&huge, &Kind::bundle()),
            Err(Error::TooLarge { .. })
        ));
    }

    /// Catches an image kind that takes less than images are: Docker manifests, tar,
    /// gzip and Docker gzip layers, and the accept header listing the index types.
    #[test]
    fn an_image_kind_takes_docker_and_oci_images() {
        let text = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"{}","digest":"{}","size":1}},"layers":[{{"mediaType":"{}","digest":"{}","size":1}},{{"mediaType":"{}","digest":"{}","size":1}},{{"mediaType":"{}","digest":"{}","size":1}}]}}"#,
            media::DOCKER_MANIFEST,
            media::DOCKER_CONFIG,
            hex(1),
            media::LAYER_TAR,
            hex(2),
            media::LAYER_TAR_GZIP,
            hex(3),
            media::DOCKER_LAYER_GZIP,
            hex(4),
        );
        let kind = Kind::image(Platform::DARWIN_ARM64);
        assert_eq!(
            parsed(text.as_bytes(), &kind).expect("parses").layers.len(),
            3
        );
        let accept = kind.accept();
        assert!(accept.starts_with(media::OCI_MANIFEST));
        assert!(accept.contains(media::DOCKER_MANIFEST));
        assert!(accept.ends_with(media::DOCKER_LIST));
        assert!(accept.contains(media::OCI_INDEX));
        assert_eq!(Kind::bundle().accept().matches(", ").count(), 2);
    }

    /// Catches a platform check that lets a wrong or missing platform through: a bundle
    /// config must state darwin/arm64; an image config may leave a field out but not
    /// contradict.
    #[test]
    fn the_platform_is_checked_per_kind() {
        let bundle = Kind::bundle();
        let image = Kind::image(Platform::linux("x86_64").expect("platform"));
        let ok = |kind: &Kind, json: &str| check_platform(kind, json.as_bytes());
        ok(&bundle, r#"{"os":"darwin","architecture":"arm64"}"#).expect("bundle");
        ok(&image, r#"{"os":"linux","architecture":"amd64"}"#).expect("image");
        ok(&image, "{}").expect("an image config may state nothing");
        ok(&image, r#"{"os":"linux"}"#).expect("or one field");
        ok(&image, r#"{"architecture":"amd64"}"#).expect("or the other");
        for json in [
            r#"{"os":"linux","architecture":"arm64"}"#,
            r#"{"os":"darwin","architecture":"amd64"}"#,
            r#"{"os":"windows","architecture":"arm64"}"#,
            r#"{}"#,
            r#"{"os":"darwin"}"#,
            r#"{"architecture":"arm64"}"#,
        ] {
            let err = ok(&bundle, json).expect_err(json);
            assert!(matches!(err, Error::Platform { .. }), "{json}: {err}");
        }
        let err = ok(&bundle, r#"{"os":"linux"}"#).expect_err("linux");
        assert_eq!(
            err.to_string(),
            "the artifact is for linux/unstated, not darwin/arm64"
        );
        let err = ok(&image, r#"{"os":"linux","architecture":"arm64"}"#).expect_err("arm");
        assert!(matches!(err, Error::Platform { .. }), "{err}");
        assert!(matches!(ok(&bundle, "[1"), Err(Error::Malformed(_))));
    }

    /// Catches a platform name wrong for a Rust architecture.
    #[test]
    fn platforms_are_named_as_images_name_them() {
        assert_eq!(Platform::linux("x86_64").expect("p").architecture, "amd64");
        assert_eq!(Platform::linux("aarch64").expect("p").architecture, "arm64");
        assert_eq!(Platform::linux("x86_64").expect("p").os, "linux");
        assert!(matches!(Platform::linux("riscv64"), Err(Error::Arch(_))));
    }
}
