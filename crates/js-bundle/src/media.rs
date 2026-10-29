//! The media types of a bundle (format section 1.1). They are on the wire and in
//! registries, so changing one is a format break: the golden test pins every value.

/// The artifact type of a bundle manifest.
pub const ARTIFACT_TYPE: &str = "application/vnd.komira.js.bundle.v1";

/// The media type of the config blob.
pub const CONFIG: &str = "application/vnd.komira.js.bundle.config.v1+json";

/// A layer that is a zstd-compressed tar of one tree.
pub const LAYER_TAR_ZSTD: &str = "application/vnd.komira.js.bundle.layer.v1.tar+zstd";

/// A layer that is an uncompressed tar of one tree.
pub const LAYER_TAR: &str = "application/vnd.komira.js.bundle.layer.v1.tar";

/// The OCI image manifest media type a bundle manifest carries.
pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// How a layer's bytes are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerEncoding {
    /// A zstd-compressed tar.
    TarZstd,
    /// A plain tar.
    Tar,
}

impl LayerEncoding {
    /// The encoding a layer media type names; `None` for any other type, including
    /// the generic OCI layer types, which a bundle never uses (a generic tool might
    /// unpack them as a root filesystem).
    pub fn from_media_type(media_type: &str) -> Option<LayerEncoding> {
        match media_type {
            LAYER_TAR_ZSTD => Some(LayerEncoding::TarZstd),
            LAYER_TAR => Some(LayerEncoding::Tar),
            "application/vnd.oci.image.layer.v1.tar+zstd" => Some(LayerEncoding::TarZstd),
            "application/vnd.oci.image.layer.v1.tar" => Some(LayerEncoding::Tar),
            _ => None,
        }
    }

    /// The media type of a layer stored this way.
    pub fn media_type(self) -> &'static str {
        match self {
            LayerEncoding::TarZstd => LAYER_TAR_ZSTD,
            LayerEncoding::Tar => LAYER_TAR,
        }
    }
}
