//! The admission of a VM start ([`js_agent_wire::StartKind::Vm`]): what a fresh VM gives a
//! job and what it does not. The runtime must run VMs and have a slot free
//! ([`crate::lifecycle::Runtime::check_vm`]); a VM runs a command, has no GPU and no
//! network device, and is given no run files. The rest (the shape, the free capacity)
//! is judged as for any start.
//!
//! A VM start is a job start, which carries no mounts, no bundle and no `isolation = host`
//! (the job keying has none of them), so there is nothing of those to refuse. Its live-log
//! sink, if any, is not used: a VM job's output reaches the result as its tail only.

use js_agent_wire::{JobSpec, NetworkCeiling, refusal};

use super::{Lifecycle, Refusal};
use crate::lifecycle::detail;

impl Lifecycle {
    /// Checks a VM start, before anything is made for it.
    pub(super) fn admit_vm(&self, spec: &JobSpec) -> Result<(), Refusal> {
        let no = |code: &'static str, why: &str| -> Refusal { (code, why.to_owned()) };
        self.runtime
            .check_vm()
            .map_err(|refused| (refused.code, refused.why))?;
        // A job start carries no legacy native body (only a run start can), so a `run` is the
        // one body a VM start may name.
        if spec.run.is_some() {
            return Err(no(
                refusal::RUN_UNSUPPORTED,
                "a VM start runs its command in the guest, not a run body",
            ));
        }
        if !spec.files.is_empty() {
            return Err(no(
                detail::FILES_UNSUPPORTED,
                "run files are not delivered into a VM",
            ));
        }
        if !spec.gpu_indices.is_empty() {
            return Err(no(detail::GPU_UNSUPPORTED, "a VM has no GPU"));
        }
        // A VM has no network device: a start that asks for any, or declares a port, is
        // refused as above the ceiling `none`, never run with less.
        crate::egress::admit(&NetworkCeiling::default(), spec)
            .map(|_| ())
            .map_err(|refused| (refused.code, refused.detail))
    }
}
