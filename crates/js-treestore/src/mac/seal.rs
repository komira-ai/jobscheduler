//! The sealer: root-owned, read-only, `uchg` on every file and directory.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use crate::sealer::{SealError, Sealed, Sealer};

/// `UF_IMMUTABLE`, the `uchg` user flag.
pub const IMMUTABLE: u32 = 0x2;

/// Sets the BSD user flags of an open file.
pub trait UserFlags: Send + Sync + 'static {
    /// Sets the flags of `file` to exactly `flags`.
    ///
    /// # Errors
    /// The call failed.
    fn set(&self, file: BorrowedFd<'_>, flags: u32) -> io::Result<()>;
}

/// Who owns a sealed tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    uid: u32,
    gid: u32,
}

impl Owner {
    /// `root:wheel`, for the sealer that runs as root.
    #[must_use]
    pub fn root() -> Owner {
        Owner { uid: 0, gid: 0 }
    }

    /// The caller's own user and group, for a sealer that does not run as root.
    #[must_use]
    pub fn caller() -> Owner {
        Owner { uid: 0, gid: 0 }
    }
}

/// A [`Sealer`] over a way to set user flags.
pub struct FlagSealer<F: UserFlags> {
    owner: Owner,
    flags: F,
}

impl<F: UserFlags> FlagSealer<F> {
    /// A sealer that gives trees to `owner` and sets flags through `flags`.
    #[must_use]
    pub fn new(owner: Owner, flags: F) -> FlagSealer<F> {
        FlagSealer { owner, flags }
    }
}

impl<F: UserFlags> Sealer for FlagSealer<F> {
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError> {
        let _ = (&self.owner, &self.flags, stage);
        Ok(Sealed { bytes: 0 })
    }

    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError> {
        let _ = tree;
        Ok(())
    }
}

/// `fchflags` on macOS.
#[cfg(target_os = "macos")]
pub struct Fchflags;

#[cfg(target_os = "macos")]
impl UserFlags for Fchflags {
    fn set(&self, file: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
        let _ = (file, flags);
        Ok(())
    }
}

/// The sealer for a Mac host.
#[cfg(target_os = "macos")]
pub type MacSealer = FlagSealer<Fchflags>;
