//! The host devices a job's container is given ([`ContainerSpec::devices`]: the GPUs its
//! run booked), as `config.json` names them: each node at its own path in `linux.devices`,
//! and the rules of the container's device cgroup in `linux.resources.devices`, which deny
//! every device first and then allow each given node, by its type, major and minor, to be
//! read and written. A container given no device gets neither section.
//!
//! The nodes every container has (`/dev/null`, `/dev/zero`, the terminals) are `crun`'s
//! own: it makes them and allows them in its device program whatever the bundle lists.

use oci_spec::runtime::{
    LinuxDevice, LinuxDeviceBuilder, LinuxDeviceCgroup, LinuxDeviceCgroupBuilder, LinuxDeviceType,
};

use super::spec::built;
use crate::container::{ContainerSpec, DeviceNode};

/// The widest permission bits a container's device node is given: read and write for
/// everyone. A node whose own bits are narrower keeps them (the two are intersected).
pub(crate) const DEVICE_MODE: u32 = 0o666;

/// What a rule lets the job do with a node it is given: read and write it (not `mknod`).
pub(crate) const DEVICE_ACCESS: &str = "rw";

/// The rule every device rule list starts with: every device, every access, denied.
const DENY_ALL: &str = "rwm";

/// The `linux.devices` entries of `spec`: one character device per node, at the node's
/// path, with its major and minor numbers, its owner and its permission bits within
/// [`DEVICE_MODE`].
pub(crate) fn nodes(spec: &ContainerSpec) -> Vec<LinuxDevice> {
    spec.devices
        .iter()
        .map(|node| {
            built(
                LinuxDeviceBuilder::default()
                    .path(&node.path)
                    .typ(LinuxDeviceType::C)
                    .major(i64::from(node.major))
                    .minor(i64::from(node.minor))
                    .file_mode(node.mode & DEVICE_MODE)
                    .uid(node.uid)
                    .gid(node.gid)
                    .build(),
            )
        })
        .collect()
}

/// The `linux.resources.devices` rules of `spec`: every device denied, then each node
/// allowed [`DEVICE_ACCESS`].
pub(crate) fn rules(spec: &ContainerSpec) -> Vec<LinuxDeviceCgroup> {
    let deny = built(
        LinuxDeviceCgroupBuilder::default()
            .allow(false)
            .access(DENY_ALL)
            .build(),
    );
    std::iter::once(deny)
        .chain(spec.devices.iter().map(allow))
        .collect()
}

/// The rule that lets the job read and write `node`.
fn allow(node: &DeviceNode) -> LinuxDeviceCgroup {
    built(
        LinuxDeviceCgroupBuilder::default()
            .allow(true)
            .typ(LinuxDeviceType::C)
            .major(i64::from(node.major))
            .minor(i64::from(node.minor))
            .access(DEVICE_ACCESS)
            .build(),
    )
}
