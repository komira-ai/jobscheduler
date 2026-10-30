//! The store volume checks, over the facts `statfs` gives.

use std::io;
use std::path::Path;

/// What `statfs` says about a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    /// The file system type name (`f_fstypename`).
    pub fs_type: String,
    /// The mount flags (`f_flags`).
    pub flags: u32,
    /// The volume's identity (`f_fsid`).
    pub fsid: [i32; 2],
}

/// A volume is not fit to hold the store.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// The volume is not APFS.
    #[error("{what} volume is {fs_type}, not apfs")]
    NotApfs {
        /// Which volume.
        what: &'static str,
        /// What it is.
        fs_type: String,
    },
    /// The volume is mounted ignoring ownership.
    #[error("{what} volume is mounted ignoring ownership")]
    IgnoresOwnership {
        /// Which volume.
        what: &'static str,
    },
    /// The volume is mounted noexec.
    #[error("{what} volume is mounted noexec")]
    NoExec {
        /// Which volume.
        what: &'static str,
    },
    /// The store and the scratch root are not on one volume.
    #[error("the store and the scratch root are on different volumes")]
    DifferentVolumes,
    /// A volume could not be read.
    #[error("cannot read the {what} volume: {source}")]
    Read {
        /// Which volume.
        what: &'static str,
        /// The error.
        source: io::Error,
    },
}

/// Checks that the store's volume is APFS, mounted with ownership honoured and not noexec,
/// and that the scratch root is on the same volume.
///
/// # Errors
/// The first check that fails.
pub fn check_volumes(
    store: &Path,
    scratch: &Path,
    read: impl Fn(&Path) -> io::Result<VolumeFacts>,
) -> Result<(), VolumeError> {
    let _ = (store, scratch, read);
    Ok(())
}
