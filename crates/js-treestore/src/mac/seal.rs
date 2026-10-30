//! The sealer: owned by one user, read-only, `uchg` on every file and directory.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use js_bundle::{MAX_DEPTH, check_target};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::sealer::{Owner, SealError, Sealed, Sealer};

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

/// The per-file metadata a sealed tree carries beyond its mode: the BSD user flags and the
/// access control list, on an open file.
pub trait UserFlags: Send + Sync + 'static {
    /// Sets the flags of `file` to exactly `flags`.
    ///
    /// # Errors
    /// The call failed.
    fn set(&self, file: BorrowedFd<'_>, flags: u32) -> io::Result<()>;

    /// Whether `file` has an access control list (which `clonefile` copies and a listing of
    /// extended attributes does not show).
    ///
    /// # Errors
    /// The call failed.
    fn has_acl(&self, file: BorrowedFd<'_>) -> io::Result<bool>;

    /// Removes the access control list of `file`.
    ///
    /// # Errors
    /// The call failed.
    fn clear_acl(&self, file: BorrowedFd<'_>) -> io::Result<()>;
}

/// Copy-on-write copies, as `clonefile(2)` makes them: the copy has its own inodes and
/// shares the data blocks, and keeps the owner, mode and flags of what it copies.
pub trait Cloner: Send + Sync + 'static {
    /// Clones the directory tree at `src` to `name` in the directory `dst_dir`, which must
    /// not hold `name`. The destination is named by descriptor and leaf so that it is
    /// resolved once.
    ///
    /// # Errors
    /// The call failed.
    fn clone_dir(&self, src: &Path, dst_dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()>;

    /// Clones the open file `src` to `name` in the directory `dst_dir`, which must not
    /// hold `name`.
    ///
    /// # Errors
    /// The call failed.
    fn clone_file(
        &self,
        src: BorrowedFd<'_>,
        dst_dir: BorrowedFd<'_>,
        name: &OsStr,
    ) -> io::Result<()>;
}

/// A [`Sealer`] that gives a tree to one owner, takes its write bits and sets the immutable
/// flag on every file and directory, through a way to set user flags.
///
/// Sealing re-checks the stage by descriptor, never following a link: only directories,
/// regular files and symlinks with a relative target pass (that a target stays inside the
/// tree is the extractor's check, not repeated here); a file with a second hard link
/// or with extended attributes is refused, and so is nesting deeper than
/// [`js_bundle::MAX_DEPTH`]. Directories end `0555`, files `0555` if the owner could
/// execute them and `0444` otherwise, and every file and directory is synced. The entry and
/// byte ceilings belong to the extraction, which has the limits; this walk does not repeat
/// them.
///
/// Unsealing clears the flags of everything under a path (and gives directories their
/// owner's `rwx` back so they can be emptied), carries on past a failure and reports the
/// first one. Links are skipped: they carry no flag.
///
/// A writable clone is made with `cloner`, which copies owner, mode and flags along with the
/// data, so the copy of a sealed tree is as sealed as the original. Then, top down and by
/// descriptor, for each entry: the flags are cleared (a directory first, since an immutable
/// directory refuses new entries and changes), the entry is given to the new owner, and it
/// gets mode `0700` (directories, executables) or `0600`. Links are given over without
/// being followed. Only metadata changes, so the blocks stay shared. A hard link, a device
/// or pipe, or nesting past [`js_bundle::MAX_DEPTH`] in the copy is refused.
pub struct FlagSealer<F: UserFlags, C: Cloner> {
    owner: Owner,
    flags: F,
    cloner: C,
}

impl<F: UserFlags, C: Cloner> FlagSealer<F, C> {
    /// A sealer that gives trees to `owner`, sets flags through `flags` and makes clones
    /// through `cloner`.
    #[must_use]
    pub fn new(owner: Owner, flags: F, cloner: C) -> FlagSealer<F, C> {
        FlagSealer {
            owner,
            flags,
            cloner,
        }
    }

    fn clone_to(&self, src: &Path, dst: &Path, owner: Owner) -> Result<(), SealError> {
        let source = rustix::fs::openat(CWD, src, FILE_OPEN, Mode::empty())?;
        let kind = FileType::from_raw_mode(rustix::fs::fstat(&source)?.st_mode);
        if !matches!(kind, FileType::Directory | FileType::RegularFile) {
            return Err(refuse(src, "is neither a directory nor a regular file"));
        }
        let (Some(parent), Some(name)) = (dst.parent(), dst.file_name()) else {
            return Err(refuse(dst, "has no parent directory and name"));
        };
        // The parent is opened once; the clone is made in it, found in it and, if the walk
        // fails, removed from it, so a path above `dst` swapped meanwhile changes nothing.
        let dir = rustix::fs::openat(CWD, parent, DIR_OPEN, Mode::empty())?;
        if kind.is_dir() {
            self.cloner.clone_dir(src, dir.as_fd(), name)?;
        } else {
            self.cloner.clone_file(source.as_fd(), dir.as_fd(), name)?;
        }
        // From here `dst` exists and is ours alone: a failure takes it away again.
        self.convert(&dir, name, kind.is_dir(), owner)
            .map_err(|error| self.discard(&dir, name, kind.is_dir(), error))
    }

    fn convert(
        &self,
        dir: &OwnedFd,
        name: &OsStr,
        is_dir: bool,
        owner: Owner,
    ) -> Result<(), SealError> {
        if is_dir {
            let root = rustix::fs::openat(dir, name, DIR_OPEN, Mode::empty())?;
            self.writable_dir(&root, owner, Path::new(""), 0)
        } else {
            let file = rustix::fs::openat(dir, name, FILE_OPEN, Mode::empty())?;
            self.writable_file(&file, owner, Path::new(""))
        }
    }

    /// Removes the clone `name` of `dir`, part made, by descriptor: flags first, then the
    /// entries. Returns `error`, with what went wrong in removing appended.
    fn discard(&self, dir: &OwnedFd, name: &OsStr, is_dir: bool, error: SealError) -> SealError {
        match self.remove_clone(dir, name, is_dir) {
            Ok(()) => error,
            Err(left) => SealError(format!("{error}; then removing the copy failed: {left}")),
        }
    }

    fn remove_clone(&self, dir: &OwnedFd, name: &OsStr, is_dir: bool) -> Result<(), SealError> {
        if is_dir {
            let root = rustix::fs::openat(dir, name, DIR_OPEN, Mode::empty())?;
            let mut run = Unsealing {
                flags: &self.flags,
                first: None,
            };
            let done = run.root(&root);
            run.note(done);
            remove_below(&root)?;
            rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?;
        } else {
            let file = rustix::fs::openat(dir, name, FILE_OPEN, Mode::empty())?;
            // As in unsealing: an inode with another name may not be ours to unflag.
            let links = rustix::fs::fstat(&file)?.st_nlink;
            if links > 1 {
                return Err(refuse(
                    Path::new(name),
                    &format!("has {links} hard links, so its flags are left"),
                ));
            }
            self.flags.set(file.as_fd(), 0)?;
            rustix::fs::unlinkat(dir, name, AtFlags::empty())?;
        }
        Ok(())
    }

    /// Hands a cloned directory and everything in it to `owner`.
    fn writable_dir(
        &self,
        dir: &OwnedFd,
        owner: Owner,
        at: &Path,
        depth: usize,
    ) -> Result<(), SealError> {
        self.writable(dir, owner, Mode::RWXU)?;
        for name in list(dir)? {
            let here = at.join(&name);
            if depth + 1 > MAX_DEPTH {
                return Err(refuse(&here, "is deeper than the depth limit"));
            }
            let stat = rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child = rustix::fs::openat(dir, &name, DIR_OPEN, Mode::empty())?;
                    self.writable_dir(&child, owner, &here, depth + 1)?;
                }
                FileType::RegularFile => {
                    let file = rustix::fs::openat(dir, &name, FILE_OPEN, Mode::empty())?;
                    self.writable_file(&file, owner, &here)?;
                }
                FileType::Symlink => rustix::fs::chownat(
                    dir,
                    &name,
                    Some(owner.uid()),
                    Some(owner.gid()),
                    AtFlags::SYMLINK_NOFOLLOW,
                )?,
                _ => return Err(refuse(&here, "has an unsupported file type")),
            }
        }
        Ok(())
    }

    fn writable_file(&self, file: &OwnedFd, owner: Owner, at: &Path) -> Result<(), SealError> {
        let stat = rustix::fs::fstat(file)?;
        if stat.st_nlink > 1 {
            return Err(refuse(
                at,
                &format!("has {} hard links, not one", stat.st_nlink),
            ));
        }
        let exec = Mode::from_raw_mode(stat.st_mode).contains(Mode::XUSR);
        let mode = if exec {
            Mode::RWXU
        } else {
            Mode::RUSR | Mode::WUSR
        };
        self.writable(file, owner, mode)
    }

    /// Clears the flags of one open file or directory (before anything else, which an
    /// immutable one would refuse) and its access control list, gives it to `owner` and
    /// sets the exact mode.
    fn writable(&self, fd: &OwnedFd, owner: Owner, mode: Mode) -> Result<(), SealError> {
        self.flags.set(fd.as_fd(), 0)?;
        if self.flags.has_acl(fd.as_fd())? {
            self.flags.clear_acl(fd.as_fd())?;
        }
        rustix::fs::fchown(fd, Some(owner.uid()), Some(owner.gid()))?;
        rustix::fs::fchmod(fd, mode)?;
        Ok(())
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
            Some(self.owner.uid()),
            Some(self.owner.gid()),
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
        if self.flags.has_acl(fd.as_fd())? {
            return Err(refuse(at, "has an ACL"));
        }
        let exec = Mode::from_raw_mode(stat.st_mode).contains(Mode::XUSR);
        let mode = if dir | exec { 0o555 } else { 0o444 };
        self.flags.set(fd.as_fd(), 0)?;
        rustix::fs::fchown(fd, Some(self.owner.uid()), Some(self.owner.gid()))?;
        rustix::fs::fchmod(fd, Mode::from_bits_truncate(mode))?;
        self.flags.set(fd.as_fd(), IMMUTABLE)?;
        sync_to_disk(fd)?;
        *bytes += blocks(stat);
        Ok(())
    }
}

impl<F: UserFlags, C: Cloner> Sealer for FlagSealer<F, C> {
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

    fn clone_writable(&self, src: &Path, dst: &Path, owner: Owner) -> Result<(), SealError> {
        self.clone_to(src, dst, owner).map_err(|error| {
            SealError(format!(
                "clone {} to {}: {error}",
                src.display(),
                dst.display()
            ))
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
                // An inode with another name may belong to someone else: its flags stay.
                if stat.st_nlink > 1 {
                    return Err(refuse(
                        Path::new(name),
                        &format!("has {} hard links, so its flags are left", stat.st_nlink),
                    ));
                }
                let file = rustix::fs::openat(dir, name, FILE_OPEN, Mode::empty())?;
                Ok(self.flags.set(file.as_fd(), 0)?)
            }
            _ => Ok(()),
        }
    }
}

/// Removes everything under `dir`, by descriptor, following nothing.
fn remove_below(dir: &OwnedFd) -> Result<(), SealError> {
    for name in list(dir)? {
        let stat = rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode).is_dir() {
            let child = rustix::fs::openat(dir, &name, DIR_OPEN, Mode::empty())?;
            remove_below(&child)?;
            rustix::fs::unlinkat(dir, &name, AtFlags::REMOVEDIR)?;
        } else {
            rustix::fs::unlinkat(dir, &name, AtFlags::empty())?;
        }
    }
    Ok(())
}

/// Flushes the file or directory to the drive, as `File::sync_all` does: on macOS plain
/// `fsync` leaves the data in the drive's cache and `F_FULLFSYNC` does not, and the store
/// writes a tree's record on the strength of this call. (Seen to compile and pass on the
/// macOS job; a test cannot tell the two apart.)
fn sync_to_disk(fd: &OwnedFd) -> Result<(), SealError> {
    #[cfg(target_os = "macos")]
    rustix::fs::fcntl_fullfsync(fd)?;
    #[cfg(not(target_os = "macos"))]
    rustix::fs::fsync(fd)?;
    Ok(())
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

/// `clonefile` and `fclonefileat` on macOS.
#[cfg(target_os = "macos")]
pub struct Clonefile;

/// `CLONE_NOFOLLOW` of `<sys/clonefile.h>`: a link at the source is cloned as a link.
#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x0001;

#[cfg(target_os = "macos")]
impl Cloner for Clonefile {
    fn clone_dir(&self, src: &Path, dst_dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let (src, name) = (c_path(src.as_os_str())?, c_path(name)?);
        // FFI-BOUNDARY: the descriptor is borrowed and open, both strings are NUL-terminated
        // and outlive the call, which keeps none of them.
        // SAFETY: as above.
        check(unsafe {
            libc::clonefileat(
                libc::AT_FDCWD,
                src.as_ptr(),
                dst_dir.as_raw_fd(),
                name.as_ptr(),
                CLONE_NOFOLLOW,
            )
        })
    }

    fn clone_file(
        &self,
        src: BorrowedFd<'_>,
        dst_dir: BorrowedFd<'_>,
        name: &OsStr,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let name = c_path(name)?;
        // FFI-BOUNDARY: both descriptors are borrowed and open, the name is NUL-terminated
        // and outlives the call, which keeps none of them.
        // SAFETY: as above.
        check(unsafe {
            libc::fclonefileat(
                src.as_raw_fd(),
                dst_dir.as_raw_fd(),
                name.as_ptr(),
                CLONE_NOFOLLOW,
            )
        })
    }
}

#[cfg(target_os = "macos")]
fn c_path(path: &OsStr) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_bytes()).map_err(io::Error::other)
}

#[cfg(target_os = "macos")]
fn check(done: libc::c_int) -> io::Result<()> {
    if done == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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

    fn has_acl(&self, file: BorrowedFd<'_>) -> io::Result<bool> {
        use std::os::fd::AsRawFd as _;
        // FFI-BOUNDARY: the descriptor is borrowed and open for the call. A non-null
        // `acl_t` it returns is ours and is freed here, once, on the one path that gets it.
        // SAFETY: as above.
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        // SAFETY: `acl` came from `acl_get_fd_np` and is freed only here.
        unsafe { acl_free(acl) };
        Ok(true)
    }

    fn clear_acl(&self, file: BorrowedFd<'_>) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: `acl_init` allocates an empty list or returns null.
        let empty = unsafe { acl_init(0) };
        if empty.is_null() {
            return Err(io::Error::last_os_error());
        }
        // FFI-BOUNDARY: the descriptor is borrowed and open for the call; `empty` is ours,
        // read by `acl_set_fd` (an empty list removes the ACL) and freed once after it.
        // SAFETY: as above.
        let done = unsafe { acl_set_fd(file.as_raw_fd(), empty) };
        let error = io::Error::last_os_error();
        // SAFETY: `empty` came from `acl_init` and is freed only here.
        unsafe { acl_free(empty) };
        if done == 0 { Ok(()) } else { Err(error) }
    }
}

/// The ACL calls of `<sys/acl.h>` (in libSystem; the `libc` crate has none).
#[cfg(target_os = "macos")]
type AclT = *mut std::ffi::c_void;

/// `ACL_TYPE_EXTENDED`.
#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> AclT;
    fn acl_free(object: AclT) -> libc::c_int;
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> AclT;
    fn acl_set_fd(fd: libc::c_int, acl: AclT) -> libc::c_int;
}

/// The sealer for a Mac host.
#[cfg(target_os = "macos")]
pub type MacSealer = FlagSealer<Fchflags, Clonefile>;
