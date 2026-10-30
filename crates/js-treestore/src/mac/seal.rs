//! The sealer: owned by one user, read-only, `uchg` on every file and directory.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use js_bundle::{MAX_DEPTH, check_target};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::{Gid, Uid, getegid, geteuid};

use crate::sealer::{SealError, Sealed, Sealer};

/// `UF_IMMUTABLE`, the `uchg` user flag.
pub const IMMUTABLE: u32 = 0x2;

const DIR_OPEN: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_OPEN: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

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
    uid: Uid,
    gid: Gid,
}

impl Owner {
    /// `root:wheel`, for the sealer that runs as root.
    #[must_use]
    pub fn root() -> Owner {
        Owner {
            uid: Uid::ROOT,
            gid: Gid::ROOT,
        }
    }

    /// The caller's own user and group, for a sealer that does not run as root.
    #[must_use]
    pub fn caller() -> Owner {
        Owner {
            uid: geteuid(),
            gid: getegid(),
        }
    }

    /// The user and group ids.
    #[must_use]
    pub fn ids(self) -> (u32, u32) {
        (self.uid.as_raw(), self.gid.as_raw())
    }
}

/// A [`Sealer`] that gives a tree to one owner, takes its write bits and sets the immutable
/// flag on every file and directory, through a way to set user flags.
///
/// Sealing re-checks the stage by descriptor, never following a link: only directories,
/// regular files and symlinks with a relative target pass; a file with a second hard link
/// or with extended attributes is refused, and so is nesting deeper than
/// [`js_bundle::MAX_DEPTH`]. Directories end `0555`, files `0555` if the owner could
/// execute them and `0444` otherwise, and every file and directory is synced. The entry and
/// byte ceilings belong to the extraction, which has the limits; this walk does not repeat
/// them.
///
/// Unsealing clears the flags of everything under a path (and gives directories their
/// owner's `rwx` back so they can be emptied), carries on past a failure and reports the
/// first one. Links are skipped: they carry no flag.
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

    fn seal_dir(
        &self,
        dir: &OwnedFd,
        at: &Path,
        depth: usize,
        bytes: &mut u64,
    ) -> Result<(), SealError> {
        for name in list(dir)? {
            let here = at.join(&name);
            if depth + 1 > MAX_DEPTH {
                return Err(refuse(&here, "is deeper than the depth limit"));
            }
            let stat = rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child = rustix::fs::openat(dir, &name, DIR_OPEN, Mode::empty())?;
                    self.seal_dir(&child, &here, depth + 1, bytes)?;
                }
                FileType::RegularFile => self.seal_file(dir, &name, &here, bytes)?,
                FileType::Symlink => self.seal_link(dir, &name, &here, &stat, bytes)?,
                _ => return Err(refuse(&here, "has an unsupported file type")),
            }
        }
        let stat = rustix::fs::fstat(dir)?;
        self.settle(dir, &stat, true, at, bytes)
    }

    fn seal_file(
        &self,
        dir: &OwnedFd,
        name: &OsStr,
        at: &Path,
        bytes: &mut u64,
    ) -> Result<(), SealError> {
        let file = rustix::fs::openat(dir, name, FILE_OPEN, Mode::empty())?;
        let stat = rustix::fs::fstat(&file)?;
        if stat.st_nlink > 1 {
            return Err(refuse(
                at,
                &format!("has {} hard links, not one", stat.st_nlink),
            ));
        }
        self.settle(&file, &stat, false, at, bytes)
    }

    fn seal_link(
        &self,
        dir: &OwnedFd,
        name: &OsStr,
        at: &Path,
        stat: &Stat,
        bytes: &mut u64,
    ) -> Result<(), SealError> {
        let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
        check_target(target.to_bytes())
            .map_err(|defect| refuse(at, &format!("symlink target refused: {defect:?}")))?;
        rustix::fs::chownat(
            dir,
            name,
            Some(self.owner.uid),
            Some(self.owner.gid),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        *bytes += blocks(stat);
        Ok(())
    }

    /// Owner, mode, immutability and sync of one open file or directory. The flags are
    /// cleared first, so that a tree already sealed can be sealed again.
    fn settle(
        &self,
        fd: &OwnedFd,
        stat: &Stat,
        dir: bool,
        at: &Path,
        bytes: &mut u64,
    ) -> Result<(), SealError> {
        let no_buffer: &mut [u8] = &mut [];
        if rustix::fs::flistxattr(fd, no_buffer)? > 0 {
            return Err(refuse(at, "has extended attributes"));
        }
        let exec = Mode::from_raw_mode(stat.st_mode).contains(Mode::XUSR);
        let mode = if dir | exec { 0o555 } else { 0o444 };
        self.flags.set(fd.as_fd(), 0)?;
        rustix::fs::fchown(fd, Some(self.owner.uid), Some(self.owner.gid))?;
        rustix::fs::fchmod(fd, Mode::from_bits_truncate(mode))?;
        self.flags.set(fd.as_fd(), IMMUTABLE)?;
        rustix::fs::fsync(fd)?;
        *bytes += blocks(stat);
        Ok(())
    }
}

impl<F: UserFlags> Sealer for FlagSealer<F> {
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError> {
        let mut bytes = 0;
        rustix::fs::openat(CWD, stage, DIR_OPEN, Mode::empty())
            .map_err(SealError::from)
            .and_then(|root| self.seal_dir(&root, Path::new(""), 0, &mut bytes))
            .map(|()| Sealed { bytes })
            .map_err(|error| SealError(format!("seal {}: {error}", stage.display())))
    }

    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError> {
        let root = match rustix::fs::openat(CWD, tree, FILE_OPEN, Mode::empty()) {
            Ok(root) => root,
            Err(Errno::NOENT) => return Ok(()),
            Err(other) => return Err(refuse(tree, &format!("cannot be opened: {other}"))),
        };
        let mut run = Unsealing {
            flags: &self.flags,
            first: None,
        };
        let done = run.root(&root);
        run.note(done);
        run.first.map_or(Ok(()), |error| {
            Err(refuse(tree, &format!("not fully unsealed: {error}")))
        })
    }
}

/// One unsealing: clears what it can and keeps the first failure.
struct Unsealing<'a, F: UserFlags> {
    flags: &'a F,
    first: Option<SealError>,
}

impl<F: UserFlags> Unsealing<'_, F> {
    fn note(&mut self, outcome: Result<(), SealError>) {
        if let Err(error) = outcome {
            self.first.get_or_insert(error);
        }
    }

    fn root(&mut self, root: &OwnedFd) -> Result<(), SealError> {
        let stat = rustix::fs::fstat(root)?;
        if FileType::from_raw_mode(stat.st_mode).is_dir() {
            self.dir(root, 0)
        } else {
            Ok(self.flags.set(root.as_fd(), 0)?)
        }
    }

    /// Clears the flags of the directory and gives its owner `rwx` back, then goes through
    /// what is in it.
    fn dir(&mut self, dir: &OwnedFd, depth: usize) -> Result<(), SealError> {
        let own = self.open_up(dir);
        self.note(own);
        for name in list(dir)? {
            let done = self.entry(dir, &name, depth);
            self.note(done);
        }
        Ok(())
    }

    fn open_up(&self, dir: &OwnedFd) -> Result<(), SealError> {
        self.flags.set(dir.as_fd(), 0)?;
        let stat = rustix::fs::fstat(dir)?;
        rustix::fs::fchmod(dir, Mode::from_raw_mode(stat.st_mode) | Mode::RWXU)?;
        Ok(())
    }

    fn entry(&mut self, dir: &OwnedFd, name: &OsStr, depth: usize) -> Result<(), SealError> {
        if depth + 1 > MAX_DEPTH {
            return Err(refuse(Path::new(name), "is deeper than the depth limit"));
        }
        let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let child = rustix::fs::openat(dir, name, DIR_OPEN, Mode::empty())?;
                self.dir(&child, depth + 1)
            }
            FileType::RegularFile => {
                let file = rustix::fs::openat(dir, name, FILE_OPEN, Mode::empty())?;
                Ok(self.flags.set(file.as_fd(), 0)?)
            }
            _ => Ok(()),
        }
    }
}

fn refuse(at: &Path, what: &str) -> SealError {
    SealError(format!("{}: {what}", at.display()))
}

fn blocks(stat: &Stat) -> u64 {
    u64::try_from(stat.st_blocks).unwrap_or_default() * 512
}

/// The names in `dir`, read whole.
fn list(dir: &OwnedFd) -> Result<Vec<OsString>, SealError> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(names)
}

/// `fchflags` on macOS.
#[cfg(target_os = "macos")]
pub struct Fchflags;

#[cfg(target_os = "macos")]
impl UserFlags for Fchflags {
    fn set(&self, file: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // FFI-BOUNDARY: the descriptor is borrowed and open for the call; `fchflags` keeps
        // nothing and reads no memory of ours.
        // SAFETY: as above.
        if unsafe { libc::fchflags(file.as_raw_fd(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// The sealer for a Mac host.
#[cfg(target_os = "macos")]
pub type MacSealer = FlagSealer<Fchflags>;
