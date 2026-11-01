//! What a node needs to contain a job: the cgroup the job runs in, the delegated cgroup
//! subtree that holds it, NUMA placement, and the subordinate-id check a rootless
//! container needs.
//!
//! - [`delegate`]: the daemon's delegated cgroup subtree and what its leases may use of the
//!   node.
//! - [`cgroup`]: one lease's cgroup, its memory cap ([`memory_max`]) and CPU weight
//!   ([`cpu_weight`]), how the kernel's OOM killer ended it, and the swap watch
//!   ([`SwapKill`]).
//! - [`cgroup_usage`]: what a lease cgroup counted, read when the action ended.
//! - [`numa`]: placing each lease on the CPUs and memory of one NUMA node.
//! - [`subids`]: the node check for the daemon user's subordinate uid and gid ranges.
//! - [`container`]: what the caller tells the runtime about one container ([`ContainerSpec`],
//!   [`ContainerLimits`]) and how it ended ([`Ended`]).
//! - [`oci`]: the runner: a rootless `crun` container per job from a bundle this crate
//!   writes ([`Oci`]), helper containers, and the per-node image store. What it needs of
//!   the embedding process is given as plain data or a trait: the arguments of the helper
//!   modes ([`oci::helper::HelperModes`]), the registry client ([`oci::image::ImageSource`])
//!   and the environment scrub ([`oci::EnvScrub`]).
//! - [`unpack`], [`unpack_layers`], [`give`] and [`modes`]: the helper modes of the
//!   daemon's own binary that [`oci`]'s helper containers run: applying an image's layer
//!   tars to a root, and giving directories to the job's ids. The embedding process calls
//!   [`modes::dispatch`] first in `main`.
//! - [`running`]: one `crun run`, on a thread of its own.
//! - [`keeper`]: the handle of the process that kills a lease's cgroup if the daemon dies.
//! - [`remove`]: removing a scratch directory without recursion and without following
//!   a symlink; [`walk`] holds the descriptor-relative steps it is made of.
//!
//! These modules were written in the container driver of the first service that used this
//! repository and moved here unchanged apart from the types they named of that service
//! (see each module's header).

pub mod cgroup;
pub mod cgroup_usage;
pub mod container;
pub mod delegate;
pub mod give;
pub mod keeper;
pub mod modes;
pub mod numa;
pub mod oci;
pub mod remove;
pub mod running;
pub mod subids;
pub mod unpack;
pub mod unpack_layers;
pub mod walk;

#[cfg(test)]
mod test_dir;

pub use cgroup::{LeasePolicy, SwapKill, cpu_max, cpu_weight, hard_memory_max, memory_max};
pub use cgroup_usage::{CgroupUsage, NumaNodeBytes};
pub use container::{ContainerLimits, ContainerSpec, Dirs, Ended, HostMount, SharedMount};
pub use delegate::Capacity;
pub use numa::Spot;
pub use oci::{Oci, OciConfig, OciError};

/// A scratch directory for one unit test, beside the test binary (inside the target
/// directory), removed when it is dropped. Declare it before anything that runs in it.
#[cfg(test)]
pub(crate) fn unit_dir(name: &str) -> test_dir::TestDir {
    let exe = std::env::current_exe().expect("test binary");
    let root = exe
        .parent()
        .expect("deps directory")
        .join("js-contain-unit");
    test_dir::TestDir::under(root, name)
}
