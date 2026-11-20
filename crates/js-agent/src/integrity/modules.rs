//! The kernel modules the agent's own job runtime loads, which the `kernel-module-changed`
//! check does not call a job's.
//!
//! The kernel loads a module on its first use, so the first job on a host loads the ones its
//! runtime needs, and the baseline (taken when the agent starts, or when a job starts on an
//! idle host) may be older than that load. Without this list the first job would taint a host
//! only because the agent did what it is for. The names are those of the code paths that load
//! them:
//!
//! | module | loaded by | when |
//! |---|---|---|
//! | [`OVERLAY`] | the layered root file system and the overlays of a job's file system (`mount_overlay` of `js-contain`, called when the file system of a run is assembled), and the root overlay of the exec spec | always |
//! | [`VETH`] | the veth pair of a wired job's network (`ip link add ... type veth`) | the agent wires jobs: egress `allow-list` or `open` |
//! | [`NFT_NAT`] | the `dnat` rules of a wired job's declared ports | the agent wires jobs |
//!
//! The netfilter modules of the egress ruleset itself (its filter, `ct`, `fib`, `counter` and
//! masquerade) are loaded when the agent starts its egress, which is before the baseline is
//! taken, so they are not on the list.
//!
//! A module of the list that is loaded and was not in the baseline is not a finding; the
//! baseline takes it at the next check that finds nothing, and from then on its unload is one
//! (an unload is a finding whatever the module). A module that is not on the list is a finding
//! when it is loaded, by name and alone, even when a runtime module was loaded in the same
//! job (`ss` loads `inet_diag`, for one).

use std::collections::BTreeSet;

use super::Integrity;

/// The module of the overlay file system, which the runtime mounts for a job's file system.
pub const OVERLAY: &str = "overlay";

/// The module of the virtual Ethernet pair, which a wired job's network is made of.
pub const VETH: &str = "veth";

/// The module of the `snat` and `dnat` expressions of nftables, which the forward of a wired
/// job's declared port uses.
pub const NFT_NAT: &str = "nft_nat";

/// The modules the job runtime of an agent loads: [`OVERLAY`] always, and [`VETH`] and
/// [`NFT_NAT`] when `wired`, that is when the agent wires jobs' networks (the egress ceiling
/// is `allow-list` or `open`, not `none`).
#[must_use]
pub fn runtime_modules(wired: bool) -> BTreeSet<String> {
    let wired_only = [VETH, NFT_NAT].into_iter().filter(|_| wired);
    [OVERLAY]
        .into_iter()
        .chain(wired_only)
        .map(str::to_owned)
        .collect()
}

impl Integrity {
    /// The same checks, which do not call a module of `names` that is loaded and was not in
    /// the baseline a job's (see [`runtime_modules`]): the job runtime loads it. Any other
    /// module that is loaded, and any module that is unloaded, is a finding. With no names
    /// (what [`Integrity::start`] gives) every new module is one.
    #[must_use]
    pub fn with_runtime_modules(self, names: BTreeSet<String>) -> Self {
        Self {
            runtime_modules: names,
            ..self
        }
    }

    /// What the modules `now` differ in from the `baseline`, as the details of the
    /// `kernel-module-changed` finding: each module loaded that the runtime does not load
    /// (`loaded <name>`), then each one unloaded (`unloaded <name>`).
    pub(super) fn changed_modules<'a>(
        &'a self,
        now: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = String> + 'a {
        let baseline = &self.baseline.modules;
        now.difference(baseline)
            .filter(|name| !self.runtime_modules.contains(*name))
            .map(|name| format!("loaded {name}"))
            .chain(
                baseline
                    .difference(now)
                    .map(|name| format!("unloaded {name}")),
            )
    }
}
