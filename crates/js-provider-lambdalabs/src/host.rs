//! What a machine on Lambda's stock image needs before the agent starts, as boot steps.
//!
//! The steps are data: each is one program and its arguments, with no shell line in them,
//! and the service's renderer runs them first, before the join token is written and before
//! the agent's unit exists. A step that fails ends the script (it runs with `set -e`), so
//! the agent never starts on a machine these steps did not finish: the machine says no
//! hello and the scheduler releases it at the boot timeout.
//!
//! In order:
//!
//! 1. `nvidia-smi` exits 0: the image carries a working GPU driver.
//! 2. The container program exists at [`js_elastic::CONTAINER_BIN`].
//! 3. A scratch file system of its own is made under [`js_elastic::SCRATCH_ROOT`]. The
//!    machine has one disk, and the agent refuses to start when its state root and its
//!    scratch root share a device, so the scratch root is an ext4 image file of the
//!    configured size, allocated in full (it cannot grow into the space the state root and
//!    the container images need) and mounted `nodev,nosuid,noatime` on the loop device.
//!    It is not a tmpfs, which would take the memory the GPU jobs need. The agent's unit
//!    requires this mount, so it does not start without it.
//!
//! Nothing is installed: a missing piece fails its step. What the stock image lacks is
//! known only after a live run (the design's open question 5), and the steps do not guess.
//! If the disk of the offer cannot hold the image and the reserve, the steps log why and
//! fail, rather than fill the disk.

use js_elastic::{BootStep, CONTAINER_BIN, SCRATCH_ROOT};

/// Where the scratch file system's image file lives, on the machine's own disk.
pub const SCRATCH_IMAGE: &str = "/var/lib/js-agent/scratch.img";

/// The GiB of the disk that must stay free beyond the scratch image, for the system, the
/// container images and the agent's state.
pub const DISK_RESERVE_GIB: u32 = 50;

fn run(argv: &[&str]) -> BootStep {
    BootStep::Run(argv.iter().map(|arg| (*arg).to_owned()).collect())
}

/// The steps for a machine with `disk_bytes` of disk and a scratch file system of
/// `scratch_gib` GiB.
pub(crate) fn steps(scratch_gib: u32, disk_bytes: u64) -> Vec<BootStep> {
    let needed = u64::from(scratch_gib) + u64::from(DISK_RESERVE_GIB);
    let disk_gib = disk_bytes >> 30;
    if disk_gib < needed {
        let why = format!(
            "the disk is {disk_gib} GiB; the scratch image needs {scratch_gib} GiB and {DISK_RESERVE_GIB} GiB must stay free"
        );
        return vec![
            run(&["logger", "-p", "user.err", "-t", "js-bootstrap", &why]),
            run(&["false"]),
        ];
    }
    let size = format!("{scratch_gib}G");
    vec![
        run(&["nvidia-smi"]),
        run(&["test", "-x", CONTAINER_BIN]),
        run(&["install", "-d", "-m", "0700", "--", SCRATCH_ROOT]),
        run(&["fallocate", "-l", &size, "--", SCRATCH_IMAGE]),
        run(&["chmod", "0600", "--", SCRATCH_IMAGE]),
        run(&[
            "mkfs.ext4",
            "-q",
            "-F",
            "-m",
            "0",
            "-L",
            "js-scratch",
            SCRATCH_IMAGE,
        ]),
        run(&[
            "mount",
            "-o",
            "loop,nodev,nosuid,noatime",
            SCRATCH_IMAGE,
            SCRATCH_ROOT,
        ]),
        run(&["chmod", "0700", "--", SCRATCH_ROOT]),
    ]
}
