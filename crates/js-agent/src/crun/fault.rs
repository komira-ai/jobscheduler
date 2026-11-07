//! Why a run could not be made into a container.

use std::io;

use js_contain::oci::JobError;

use super::gpu;
use crate::image::ConfigError;

/// Why a run could not be made into a container.
#[derive(Debug, thiserror::Error)]
pub(super) enum Fault {
    #[error("a host job is refused: this agent was started without --allow-host-jobs")]
    NotContained,
    #[error("a host job is given no mount and no run files: it sees the host's own paths")]
    HostMounts,
    #[error("a host job books no GPU: it sees every device of the host")]
    HostGpus,
    #[error(transparent)]
    Gpu(gpu::StartError),
    #[error("this runtime runs container jobs only")]
    Native,
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Image(String),
    #[error("run id {0:?} cannot name a container")]
    Name(String),
    #[error("a container of run {0} exists")]
    InUse(String),
    #[error("this runtime has no run files directory")]
    NoFiles,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Book(#[from] js_types::TypeError),
    #[error("remove an image that has no config: {0}")]
    Evict(#[from] io::Error),
    #[error("the image's pin: {0}")]
    Pin(#[from] JobError),
    #[error("{what}: {why}")]
    Step { what: &'static str, why: String },
}

impl Fault {
    pub(super) fn step(what: &'static str, why: &dyn std::fmt::Display) -> Self {
        Self::Step {
            what,
            why: why.to_string(),
        }
    }
}
