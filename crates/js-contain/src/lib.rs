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
//!
//! These modules were written in the container driver of the first service that used this
//! repository and moved here unchanged apart from the types they named of that service
//! (see each module's header). They name no job, image or runtime: the OCI runner that
//! starts the container is not part of this crate yet.

pub mod cgroup;
pub mod cgroup_usage;
pub mod delegate;
pub mod numa;
pub mod subids;

#[cfg(test)]
mod test_dir;

pub use cgroup::{SwapKill, cpu_weight, memory_max};
pub use cgroup_usage::{CgroupUsage, NumaNodeBytes};
pub use delegate::Capacity;
pub use numa::Spot;

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
