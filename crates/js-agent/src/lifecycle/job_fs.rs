//! The layers and mounts a job start asks of its file system ([`JobFs`]), and the host paths
//! among them a runtime looks up ([`HostSource`]).

use js_agent_wire::{JobMount, Layer, MountSource};

/// What a job start asks of its file system beyond its image (`docs/design/job-rootfs.md`):
/// the read-only layers of its root and the trees at paths of its own, as the start carries
/// them ([`js_agent_wire::JobStartSpec::layers`], [`js_agent_wire::JobStartSpec::mounts`]).
/// Empty for a start that asks for neither, and for a run start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobFs {
    /// The layers of the root, base first.
    pub layers: Vec<Layer>,
    /// The trees at paths of the job's file system.
    pub mounts: Vec<JobMount>,
}

impl JobFs {
    /// Whether the start asks for no layer and no mount.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.mounts.is_empty()
    }

    /// The host paths the start names, each with the `sub_path` of its mount and whether
    /// the job may write through it (a read-write mount's), in the order a runtime counts
    /// its sources: the layers' first, in their order, then the mounts'. A source a
    /// provider makes has no path until the provider answers one, and is not here: the
    /// lifecycle replaces it by the path once the provider has answered it
    /// ([`JobFs::without_providers`] is what the runtime is asked about before that).
    #[must_use]
    pub fn host_paths(&self) -> Vec<HostSource<'_>> {
        let layers = self.layers.iter().filter_map(|layer| match layer {
            Layer::HostPath { path } => Some(HostSource {
                path,
                sub_path: None,
                writes: false,
            }),
            Layer::Image { .. } | Layer::Provider { .. } => None,
        });
        let mounts = self.mounts.iter().filter_map(|mount| match &mount.source {
            MountSource::HostPath { path } => Some(HostSource {
                path,
                sub_path: mount.sub_path.as_deref(),
                writes: mount.mode == js_agent_wire::MountMode::ReadWrite,
            }),
            MountSource::Provider { .. } => None,
        });
        layers.chain(mounts).collect()
    }

    /// The start's layers and mounts with every source a provider makes left out: the
    /// layers that are not [`Layer::Provider`] and the mounts that are not of a
    /// [`MountSource::Provider`], in their order. These are the paths a runtime can look up
    /// before the providers have answered.
    #[must_use]
    pub fn without_providers(&self) -> Self {
        Self {
            layers: self
                .layers
                .iter()
                .filter(|layer| !matches!(layer, Layer::Provider { .. }))
                .cloned()
                .collect(),
            mounts: self
                .mounts
                .iter()
                .filter(|mount| !matches!(mount.source, MountSource::Provider { .. }))
                .cloned()
                .collect(),
        }
    }

    /// The images of the start's image layers, base first.
    #[must_use]
    pub fn images(&self) -> Vec<&str> {
        self.layers.iter().filter_map(layer_image).collect()
    }
}

/// The image of an image layer.
fn layer_image(layer: &Layer) -> Option<&str> {
    match layer {
        Layer::Image { image } => Some(image),
        Layer::HostPath { .. } | Layer::Provider { .. } => None,
    }
}

/// One host path of a [`JobFs`] ([`JobFs::host_paths`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSource<'a> {
    /// The absolute path on the host.
    pub path: &'a str,
    /// The directory below it that the job sees instead (a mount's `sub_path`).
    pub sub_path: Option<&'a str>,
    /// The job writes through it: a read-write mount, which only a root the job may mount
    /// read-write allows.
    pub writes: bool,
}
