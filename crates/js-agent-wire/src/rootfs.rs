//! A job's file system beyond its image (`docs/design/job-rootfs.md`, section 3): the
//! read-only [`Layer`]s of its root, what it sees at paths of its own ([`PathMount`], each
//! from a [`MountSource`] with a [`MountMode`]) and the ports of the host's loopback it may
//! reach ([`HostPort`]). A [`crate::JobStartSpec`] carries them in `layers`, `mounts` and
//! `host_ports`.
//!
//! Every item is a message, never a bare string or number, so each can grow additively: a
//! source or a layer is tagged by `type`, so a new kind (a `tmpfs` or an `image` source of a
//! mount, say) is a new variant; a mount or a host port is a struct, so a new option (mount
//! options, a protocol, a port in the job other than the host's) is a new optional field
//! whose absence keeps today's meaning. No enum here has a catch-all variant, as
//! [`crate::Placement`] has none: an agent that meets a kind or a mode it does not know
//! fails the decode of that one start, which inside a reply reads as
//! [`crate::Command::Unknown`], so it never runs a job on a root, or with a mount, other
//! than the one asked for. A field it does not know it ignores, as every receiver here does,
//! which is why a server sends these fields only to a host that reports
//! `layered-rootfs`.
//!
//! The path rules are `js-types`' ([`js_types::path`]), the ones the server holds a job to
//! when it takes it; the host's allow-lists are the agent's, which refuses a start that
//! names a path or port outside them ([`crate::refusal::INPUT_NOT_ALLOWED`]).

use js_types::path;
use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::image;
use crate::limits;
use crate::refusal;

/// One read-only layer of a job's root, stacked above the layers before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Layer {
    /// A directory on the host, by absolute path: in normal form (no empty, `.` or `..`
    /// part, no `/` at its end), at most [`limits::MAX_PATH_LEN`] bytes, no control or
    /// format character. The host's operator lists the prefixes a job may name.
    HostPath {
        /// The directory's path on the host.
        path: String,
    },
    /// An image pinned by digest, in the grammar of [`crate::JobStartSpec::image`]; its
    /// own layers are flattened into one tree, and its configuration is not used.
    Image {
        /// The image reference, `name[:tag]@sha256:<64 hex>`.
        image: String,
    },
}

/// How a host directory appears in the job. Read-only is the default, left out of the
/// text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    /// The job reads the directory and cannot write it.
    #[default]
    ReadOnly,
    /// The job's writes reach the host directory. The host's operator lists the prefixes a
    /// job may mount read-write.
    ReadWrite,
    /// The job may change the directory; its changes are its own and never reach the host
    /// directory.
    CopyOnWrite,
}

impl MountMode {
    /// Whether this is [`MountMode::ReadOnly`], the value a mount leaves out.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        *self == Self::ReadOnly
    }
}

/// What a mount shows. Tagged by `type`, so a later kind is a new variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MountSource {
    /// A directory on the host, by absolute path, held to the rules of a
    /// [`Layer::HostPath`]. The host's operator lists the prefixes a job may name.
    HostPath {
        /// The directory's path on the host.
        path: String,
    },
}

/// Something at a path in the job's file system: today a host directory. Not
/// [`crate::Mount`], which names one of the agent's roots and a path below it, and which
/// this does not replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathMount {
    /// What appears at `target`.
    pub source: MountSource,
    /// Where it appears: an absolute path in the job's file system, at most
    /// [`limits::MAX_PATH_LEN`] bytes with its leading `/`, ASCII, in normal form, not `/`,
    /// with no `:`, `,` or `\`, and outside the places the runtime keeps for itself
    /// ([`js_types::path::RUNTIME_PLACES`]). A target may lie below another mount's; two
    /// may not be equal, compared in lower case.
    pub target: String,
    /// Read-only (the default, left out), read-write or copy-on-write.
    #[serde(default, skip_serializing_if = "MountMode::is_read_only")]
    pub mode: MountMode,
}

/// The layers of a start: at most [`limits::MAX_LAYERS`] (`layers`, `invalid-job`); a host
/// path as [`path::absolute`] has it (`layers.path`, `invalid-job`); an image in the image
/// grammar (`layers.image`, `invalid-image`). Its digest is checked by [`pin_layers`].
pub(crate) fn check_layers(layers: &[Layer]) -> Result<(), ValidationError> {
    let at = At::new("layers", refusal::INVALID_JOB);
    check::count(at, layers.len(), limits::MAX_LAYERS)?;
    for layer in layers {
        match layer {
            Layer::HostPath { path } => {
                check::path(at.field("layers.path"), path::absolute(path))?;
            }
            Layer::Image { image } => {
                image::image(At::new("layers.image", refusal::INVALID_IMAGE), image)?;
            }
        }
    }
    Ok(())
}

/// Every image layer pinned by a `sha256:` digest (`layers.image`,
/// `image-digest-required`), after [`check_layers`] has passed.
pub(crate) fn pin_layers(layers: &[Layer]) -> Result<(), ValidationError> {
    let at = At::new("layers.image", refusal::IMAGE_DIGEST_REQUIRED);
    for layer in layers {
        if let Layer::Image { image } = layer {
            image::pinned(at, image)?;
        }
    }
    Ok(())
}

/// The path mounts of a start, refused `invalid-mount`: at most [`limits::MAX_MOUNTS`]
/// (`mounts`); each host-path source as [`path::absolute`] has it (`mounts.source.path`)
/// and each target as
/// [`path::target`] has it (`mounts.target`); and no two targets equal in lower case
/// ([`path::distinct`], `mounts.target`).
pub(crate) fn check_mounts(mounts: &[PathMount]) -> Result<(), ValidationError> {
    let at = At::new("mounts", refusal::INVALID_MOUNT);
    check::count(at, mounts.len(), limits::MAX_MOUNTS)?;
    let source = at.field("mounts.source.path");
    let target = at.field("mounts.target");
    for mount in mounts {
        let MountSource::HostPath { path } = &mount.source;
        check::path(source, path::absolute(path))?;
        check::path(target, path::target(&mount.target))?;
    }
    let targets: Vec<&str> = mounts.iter().map(|m| m.target.as_str()).collect();
    check::path(target, path::distinct(&targets))
}

/// A TCP port on the host's loopback that the job reaches at the same port on its own
/// loopback. A struct, not a bare number, so a protocol, a port in the job other than the
/// host's, or another kind of pass-through can be added as optional fields whose absence
/// keeps this meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPort {
    /// The port on the host's loopback, from 1 to 65535; the job reaches it at the same
    /// number on its own loopback.
    pub host_port: u16,
}

/// The host ports of a start, refused `invalid-job` on `host_ports`: at most
/// [`limits::MAX_PORTS`], each from 1 to 65535, no host port twice.
pub(crate) fn check_host_ports(ports: &[HostPort]) -> Result<(), ValidationError> {
    let at = At::new("host_ports", refusal::INVALID_JOB);
    check::count(at, ports.len(), limits::MAX_PORTS)?;
    if ports.iter().any(|port| port.host_port == 0) {
        return Err(at.fail(Rule::OutOfRange));
    }
    check::distinct(at, ports.iter().map(|port| port.host_port))
}
