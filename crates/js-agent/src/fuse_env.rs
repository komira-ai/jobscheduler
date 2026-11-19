//! What a host must have to give a job the FUSE device (`--allow-fuse-jobs`), and the probe
//! that checks it.
//!
//! A job that asks for the device is given `/dev/fuse` and `CAP_SYS_ADMIN` of its own user
//! namespace (`js_contain::ContainerSpec::fuse`). The agent reports it only on a host where
//! that can work, and says what is missing where it cannot:
//!
//! - `fuse-jobs` ([`js_caps::FUSE_JOBS`]): the kernel is at least 6.14
//!   (`MIN_KERNEL`), and `/dev/fuse` is a character device with the numbers of the kernel's
//!   FUSE device, 10:229.
//! - `fuse-uring` ([`js_caps::FUSE_URING`]): as `fuse-jobs`, and FUSE over io_uring is
//!   usable: the fuse module's `enable_uring` parameter is on (set on the kernel command
//!   line, `fuse.enable_uring=1`, at boot) and `kernel.io_uring_disabled` is 0.
//!
//! The probe reads four places, each through a seam so a test feeds text instead of the
//! machine: `/proc/sys/kernel/osrelease` (the kernel's version: its leading `<major>.<minor>`
//! is read, so `7.0.0-42-generic`, `6.14.0` and `6.8.0-1017-azure` are 7.0, 6.14 and 6.8),
//! `/dev/fuse` (through [`ReadNode`](crate::crun::ReadNode), which refuses anything but a
//! character device), `/sys/module/fuse/parameters/enable_uring` (`Y` or `1` is on; a file
//! that is not there is off, as on a kernel that has no such parameter) and
//! `/proc/sys/kernel/io_uring_disabled` (`0` is usable; a file that is not there is usable,
//! as on a kernel without the sysctl). A reason is a sentence for the operator's log.

use std::path::Path;

use crate::crun::ReadNode;

/// The least kernel, as (major, minor), that gives a job the FUSE device.
const MIN_KERNEL: (u32, u32) = (6, 14);

/// Where the kernel prints its release.
const OSRELEASE: &str = "/proc/sys/kernel/osrelease";

/// The FUSE device node.
const FUSE_DEVICE: &str = "/dev/fuse";

/// The fuse module's switch for FUSE over io_uring.
const ENABLE_URING: &str = "/sys/module/fuse/parameters/enable_uring";

/// The sysctl that turns io_uring off (1 or 2) or leaves it on (0).
const IO_URING_DISABLED: &str = "/proc/sys/kernel/io_uring_disabled";

/// The major number of the FUSE device.
const FUSE_MAJOR: u32 = 10;

/// The minor number of the FUSE device.
const FUSE_MINOR: u32 = 229;

/// What the probe found: each field is `Ok` or the reason the host cannot give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseEnv {
    /// Whether the host can give a job the FUSE device (`fuse-jobs`).
    pub jobs: Result<(), String>,
    /// Whether FUSE over io_uring is usable on top of that (`fuse-uring`): an error whenever
    /// [`FuseEnv::jobs`] is one, with the same reason.
    pub uring: Result<(), String>,
}

/// The probe over `read` (the text of a file, or why it cannot be read) and `node` (the
/// device node at a path, such as [`crate::crun::read_node`]). The agent passes its own file
/// reader; a test of a real host passes one over `std::fs`.
#[must_use]
pub fn probe(read: &dyn Fn(&str) -> Result<String, String>, node: ReadNode) -> FuseEnv {
    let jobs = jobs(read, node);
    let uring = match &jobs {
        Ok(()) => uring(read),
        Err(why) => Err(why.clone()),
    };
    FuseEnv { jobs, uring }
}

/// Whether the kernel and the device node fit.
fn jobs(read: &dyn Fn(&str) -> Result<String, String>, node: ReadNode) -> Result<(), String> {
    let text =
        read(OSRELEASE).map_err(|why| format!("the kernel's version cannot be read: {why}"))?;
    let text = text.trim();
    let (major, minor) = kernel(text).ok_or_else(|| {
        format!("the kernel's version {text:?} does not start with <major>.<minor>")
    })?;
    if (major, minor) < MIN_KERNEL {
        return Err(format!(
            "the kernel is {major}.{minor}, below {}.{}",
            MIN_KERNEL.0, MIN_KERNEL.1
        ));
    }
    let found = node(Path::new(FUSE_DEVICE)).map_err(|why| why.to_string())?;
    match (found.major, found.minor) == (FUSE_MAJOR, FUSE_MINOR) {
        true => Ok(()),
        false => Err(format!(
            "{FUSE_DEVICE} is not the FUSE device (char {FUSE_MAJOR}:{FUSE_MINOR})"
        )),
    }
}

/// Whether FUSE over io_uring is usable.
fn uring(read: &dyn Fn(&str) -> Result<String, String>) -> Result<(), String> {
    let on = read(ENABLE_URING).is_ok_and(|text| matches!(text.trim(), "Y" | "y" | "1"));
    if !on {
        return Err("the fuse module's enable_uring is off".to_owned());
    }
    match read(IO_URING_DISABLED) {
        Ok(text) if text.trim() != "0" => {
            Err(format!("kernel.io_uring_disabled is {}", text.trim()))
        }
        _ => Ok(()),
    }
}

/// The leading `<major>.<minor>` of a kernel release (`7.0.0-42-generic` is (7, 0)), or none
/// when the text does not start with two numbers around a dot.
fn kernel(release: &str) -> Option<(u32, u32)> {
    let (major, rest) = release.split_once('.')?;
    let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
#[path = "fuse_env_tests.rs"]
mod tests;
