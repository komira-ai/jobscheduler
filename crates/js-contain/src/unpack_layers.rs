//! The unpack mode of the daemon's binary: applies an image's layer tars to a root
//! filesystem, run inside a helper container of the OCI runner ([`crate::oci`]).
//!
//! `<unpack> <stage> <blobs> <layer>...` makes `<stage>/rootfs` from the layer files in
//! `<blobs>`, lowest first (`<n>.tar` or `<n>.tar.gz`: the names the image source fetches
//! them under), then the mount points the runtime binds over. The helper has a job's user
//! namespace, so id 0 of the tars is the first subordinate id and every file ends up
//! owned by the host id the job sees as the image's own, with no shifting pass. Its user
//! namespace maps neither the daemon user nor anything outside the stage and blob
//! directories it is given, so a hostile tar can write nothing the daemon user owns.
//!
//! No check made here relies on the name of the exec root: it is only a directory the
//! unpack makes inside the root, and the runtime makes any other as it mounts.
//!
//! The binary's `main` reaches it through [`crate::modes::dispatch`], before it parses
//! its flags. The argument list is an interface between daemon versions: a node may run
//! a newer binary after an update renamed it over the old one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::oci::helper::HelperModes;
use crate::oci::image::ROOTFS;
use crate::unpack::{Compression, Ownership, Unpack, ensure_paths};

/// The mount points the runner binds over in an unpacked image, as directories. The
/// exec root is the one the embedder names in the container spec; the helper cannot see
/// it, so the directory made here is the conventional `job/root`, and a runtime makes
/// any other as it mounts.
pub const MOUNT_DIRS: [&str; 13] = [
    "dev",
    "proc",
    "sys",
    "etc",
    "tmp",
    "var/tmp",
    "run",
    "root",
    "job/root",
    "dev/pts",
    "dev/shm",
    "dev/mqueue",
    "sys/fs/cgroup",
];

/// The mount points the runner binds over as files (`/etc/hosts`, `/etc/hostname`,
/// `/etc/resolv.conf`).
pub const MOUNT_FILES: [&str; 3] = ["etc/hosts", "etc/hostname", "etc/resolv.conf"];

/// What the mode was asked.
struct Request {
    stage: PathBuf,
    layers: Vec<(PathBuf, Compression)>,
}

/// Reads `<stage> <blobs> <layer>...`.
fn parse(args: &[OsString]) -> Option<Request> {
    let [stage, blobs, layers @ ..] = args else {
        return None;
    };
    let blobs = Path::new(blobs);
    let mut out = Vec::new();
    for layer in layers {
        let name = layer.to_str()?;
        // A bare file name of the blob directory: nothing to walk out of it with.
        if Path::new(name).file_name() != Some(layer.as_os_str()) {
            return None;
        }
        let compression = if name.ends_with(".tar.gz") {
            Compression::Gzip
        } else if name.ends_with(".tar") {
            Compression::None
        } else {
            return None;
        };
        out.push((blobs.join(name), compression));
    }
    Some(Request {
        stage: PathBuf::from(stage),
        layers: out,
    })
}

/// The mode, its arguments after the mode word (`modes.unpack`) being
/// `<stage> <blobs> <layer>...`: 0 once the root filesystem is made, 1 when a layer or a
/// mount point could not be applied (the reason on stderr), 2 for arguments it cannot read.
#[must_use]
pub fn run(modes: &HelperModes, args: &[OsString]) -> u8 {
    let mode = &modes.unpack;
    let Some(request) = parse(args) else {
        eprintln!("{mode}: usage: {mode} <stage> <blobs> <NAME.tar|NAME.tar.gz>..., not {args:?}");
        return 2;
    };
    let rootfs = request.stage.join(ROOTFS);
    // Each owner is the tar header's; a header outside the container's ids is an error.
    if let Err(e) = Unpack::new(Ownership::Header).run(&rootfs, &request.layers) {
        eprintln!("{mode}: {e}");
        return 1;
    }
    if let Err(e) = ensure_paths(&rootfs, &MOUNT_DIRS, &MOUNT_FILES) {
        eprintln!("{mode}: mount points: {e}");
        return 1;
    }
    0
}
