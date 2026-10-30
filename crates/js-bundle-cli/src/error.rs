//! What a command can fail with. The text of each variant is what the person at the
//! terminal reads, so it names the flag, tree or file involved.

use std::io;
use std::path::Path;

use js_bundle::{ConfigError, FieldParseError, PackError, TemplateError, UnpackError};

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A flag that takes `NAME=VALUE` names one name twice.
    #[error("{flag} names {key:?} twice")]
    Duplicate {
        /// The flag.
        flag: &'static str,
        /// The repeated name.
        key: String,
    },
    /// A tree has no value for a flag every tree needs.
    #[error("tree {tree:?} has no {flag}")]
    Missing {
        /// The flag.
        flag: &'static str,
        /// The tree.
        tree: String,
    },
    /// `--lock-env` names a variable no `--env` sets.
    #[error("--lock-env {0:?} names no variable that --env sets")]
    NoSuchEnv(String),
    /// A directory is not a tree the format admits.
    #[error("tree {tree:?}: {source}")]
    Tree {
        /// The tree.
        tree: String,
        /// The first rule it breaks.
        source: PackError,
    },
    /// A tree name, version or similar field is malformed.
    #[error(transparent)]
    Field(#[from] FieldParseError),
    /// A placeholder string is malformed.
    #[error(transparent)]
    Template(#[from] TemplateError),
    /// The config breaks a rule of the format.
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    /// The layout or an artifact in it is refused.
    #[error(transparent)]
    Oci(#[from] js_oci::Error),
    /// An annotation whose key is not a reverse-domain key or whose value is empty.
    #[error(
        "annotation {key:?} needs a reverse-domain key of at most 256 bytes (dot-separated labels of lowercase letters, digits, '-' and '_') and a non-empty value of at most 4096 bytes, without control or bidirectional characters"
    )]
    Annotation {
        /// The key.
        key: String,
    },
    /// A flag names a tree that no `--tree` flag does.
    #[error("{flag} names {key:?}, which is no --tree")]
    NoSuchTree {
        /// The flag.
        flag: &'static str,
        /// The name.
        key: String,
    },
    /// Something that must be a regular file is another kind of file.
    #[error("{path} is not a regular file")]
    NotRegular {
        /// The path.
        path: String,
    },
    /// A file or directory could not be read or made.
    #[error("{path}: {source}")]
    Path {
        /// The path.
        path: String,
        /// What the system said.
        source: io::Error,
    },
    /// A system call failed.
    #[error(transparent)]
    Errno(#[from] rustix::io::Errno),
    /// Standard output or the runtime failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A JSON document could not be written or read.
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The layout lists other than one manifest and no digest was given.
    #[error("the layout lists {count} manifests; name one with --digest")]
    Ambiguous {
        /// How many it lists.
        count: usize,
    },
    /// The manifest and the config disagree on how many layers there are.
    #[error("the manifest has {manifest} layers and the config describes {config}")]
    LayerCount {
        /// The manifest's layers.
        manifest: usize,
        /// The config's layers.
        config: usize,
    },
    /// A layer of a type no bundle layer has. The manifest check refuses these first, so
    /// this is a guard against that check changing.
    #[error("a layer is of a media type no bundle layer has")]
    LayerType,
    /// A layer is not the tree its config entry says.
    #[error("layer {index} (tree {tree:?}): {source}")]
    Layer {
        /// The layer's index.
        index: usize,
        /// The tree.
        tree: String,
        /// The first rule it breaks.
        source: UnpackError,
    },
}

/// The conversion of an I/O failure at `path` into an [`Error`] that names it.
pub(crate) fn at(path: &Path) -> impl FnOnce(io::Error) -> Error + use<> {
    let path = path.display().to_string();
    move |source| Error::Path { path, source }
}
