//! The admission of a start's file system beyond its image (`docs/design/job-rootfs.md`,
//! section 4.7): what can be judged from the start's text and the agent's configuration,
//! with no I/O. What lies beneath a root is looked up later, off the worker, while the start
//! waits ([`Runtime::resolve_fs`](crate::lifecycle::Runtime::resolve_fs)).

use js_agent_wire::{JobSpec, refusal};

use super::job::Given;
use super::{Lifecycle, Refusal};

impl Lifecycle {
    /// Whether the layers and mounts of a start may be given here: the runtime gives them
    /// (not a VM start, not a runtime without [`Runtime::layered_rootfs`]:
    /// `rootfs-unsupported-here`), each host path lies beneath a root of the agent's
    /// (`--mount-root`, `--mount-root-ro`), a read-write mount's beneath one it may write
    /// (`input-not-allowed`), and the image of each image layer is one the runtime may pull
    /// (`image-not-allowed`). A runtime that binds no mount of a named root refuses a start
    /// that names one (`invalid-mount`), so no path the lifecycle resolves by its text is
    /// ever bound there.
    ///
    /// [`Runtime::layered_rootfs`]: crate::lifecycle::Runtime::layered_rootfs
    pub(super) fn admit_fs(&self, spec: &JobSpec, given: &Given) -> Result<(), Refusal> {
        if !spec.mounts.is_empty() && !self.runtime.binds_named_mounts() {
            return Err((
                refusal::INVALID_MOUNT,
                "this runtime binds no mount of a named root: a job start names its host paths"
                    .to_owned(),
            ));
        }
        if given.fs.is_empty() {
            return Ok(());
        }
        if given.vm || !self.runtime.layered_rootfs() {
            return Err((
                refusal::ROOTFS_UNSUPPORTED_HERE,
                "this runtime gives a job no layers and no mounts of host paths".to_owned(),
            ));
        }
        for source in given.fs.host_paths() {
            let Some(beneath) = self.config.mounts.beneath(source.path) else {
                return Err((
                    refusal::INPUT_NOT_ALLOWED,
                    format!(
                        "{} lies beneath no --mount-root or --mount-root-ro",
                        source.path
                    ),
                ));
            };
            if source.writes && beneath.read_only {
                return Err((
                    refusal::INPUT_NOT_ALLOWED,
                    format!(
                        "{} lies beneath the root {}, which jobs may mount read-only only",
                        source.path, beneath.name
                    ),
                ));
            }
        }
        for image in given.fs.images() {
            self.runtime
                .check_layer_image(image)
                .map_err(|why| (refusal::IMAGE_NOT_ALLOWED, why))?;
        }
        Ok(())
    }
}
