//! The kernel calls of the XFS backend, and nothing else: `ioctl` for a directory's
//! project id, `quotactl` for the limits and the state, and the reads of the mount table
//! and of a device node. The logic that uses them is in `xfs` and runs against a scripted
//! [`Sys`] in tests; the real one, [`Kernel`], is a straight line per call, so a test that
//! calls it where it can only fail still runs every line.
//!
//! All the unsafe code of the quota is here. Each block says what it lends the kernel.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// `FS_XFLAG_PROJINHERIT`: a file created below the directory takes its project id.
pub(super) const XFLAG_PROJINHERIT: u32 = 0x0000_0200;
/// `FS_QUOTA_PDQ_ACCT | FS_QUOTA_PDQ_ENFD`: project quota is counted and enforced.
pub(super) const PROJECT_ACCT_AND_ENFD: u16 = 0x10 | 0x20;

/// `_IOR('X', 31, struct fsxattr)` and `_IOW('X', 32, struct fsxattr)`, 28 bytes.
const FS_IOC_FSGETXATTR: u32 = 0x801C_581F;
const FS_IOC_FSSETXATTR: u32 = 0x401C_5820;

/// `XQM_CMD(x)` is `('X' << 8) + x`; `QCMD(cmd, type)` shifts it up by 8 and adds the type.
const Q_XGETQUOTA: u32 = 0x5803;
const Q_XSETQLIM: u32 = 0x5804;
const Q_XGETQSTAT: u32 = 0x5805;
const XQM_PRJQUOTA: u32 = 2;
/// `FS_DQUOT_VERSION`, `FS_PROJ_QUOTA`, `FS_DQ_BHARD | FS_DQ_IHARD`.
const FS_DQUOT_VERSION: i8 = 1;
const FS_PROJ_QUOTA: i8 = 2;
const FS_DQ_HARD_LIMITS: u16 = 0x8 | 0x2;

/// `struct fsxattr` of `<linux/fs.h>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
pub(super) struct Fsx {
    pub xflags: u32,
    pub extsize: u32,
    pub nextents: u32,
    pub projid: u32,
    pub cowextsize: u32,
    pub pad: [u8; 8],
}

/// `struct fs_disk_quota` of `<linux/dqblk_xfs.h>`: 112 bytes, blocks in units of 512.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
struct DiskQuota {
    version: i8,
    flags: i8,
    fieldmask: u16,
    id: u32,
    blk_hardlimit: u64,
    blk_softlimit: u64,
    ino_hardlimit: u64,
    ino_softlimit: u64,
    bcount: u64,
    icount: u64,
    itimer: i32,
    btimer: i32,
    iwarns: u16,
    bwarns: u16,
    timer_hi: [i8; 4],
    rtb_hardlimit: u64,
    rtb_softlimit: u64,
    rtbcount: u64,
    rtbtimer: i32,
    rtbwarns: u16,
    padding3: i16,
    padding4: [u8; 8],
}

/// `struct fs_qfilestat`.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
struct QFileStat {
    ino: u64,
    nblks: u64,
    nextents: u32,
}

/// `struct fs_quota_stat`: 80 bytes.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
#[allow(
    dead_code,
    reason = "a mirror of a kernel struct: the fields are there for the layout"
)]
struct QuotaStat {
    version: i8,
    flags: u16,
    pad: i8,
    uquota: QFileStat,
    gquota: QFileStat,
    incoredqs: u32,
    btimelimit: i32,
    itimelimit: i32,
    rtbtimelimit: i32,
    bwarnlimit: u16,
    iwarnlimit: u16,
}

/// One project's limits and usage, in the kernel's units (blocks of 512 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Dq {
    pub blocks_used: u64,
    pub inodes_used: u64,
    pub blocks_limit: u64,
    pub inodes_limit: u64,
}

/// What the XFS backend asks of the kernel.
pub(super) trait Sys: Send + Sync {
    /// The text of `/proc/self/mountinfo`.
    fn mountinfo(&self) -> io::Result<String>;
    /// The device number of the block device node `path`; none if it is something else.
    fn block_rdev(&self, path: &Path) -> io::Result<Option<u64>>;
    /// The device number (`st_dev`) of the file system that holds `path`.
    fn dev_of(&self, path: &Path) -> io::Result<u64>;
    /// `qs_flags` of `Q_XGETQSTAT` for the file system on `device`.
    fn quota_flags(&self, device: &Path) -> io::Result<u16>;
    /// Opens the directory `path` for the calls below: `O_DIRECTORY | O_NOFOLLOW |
    /// O_CLOEXEC`, so a link in the last component is refused and not followed. A caller
    /// opens a run's directory once and does everything to that descriptor, so the checks
    /// and the changes cannot land on different directories.
    fn open_dir(&self, path: &Path) -> io::Result<File>;
    /// Whether the open directory holds no entry.
    fn is_empty(&self, dir: &File) -> io::Result<bool>;
    /// `FS_IOC_FSGETXATTR` of the open directory.
    fn get_attr(&self, dir: &File) -> io::Result<Fsx>;
    /// `FS_IOC_FSSETXATTR` of the open directory.
    fn set_attr(&self, dir: &File, attr: Fsx) -> io::Result<()>;
    /// `Q_XGETQUOTA` for a project.
    fn get_quota(&self, device: &Path, project: u32) -> io::Result<Dq>;
    /// `Q_XSETQLIM`: the hard limits of a project, in blocks of 512 and inodes; 0 is none.
    fn set_limits(&self, device: &Path, project: u32, blocks: u64, inodes: u64) -> io::Result<()>;
}

/// The real calls.
pub(super) struct Kernel;

/// `Ok` for a call that returned 0, the error of `errno` for one that returned -1.
fn check(ret: libc::c_int) -> io::Result<()> {
    match ret < 0 {
        true => Err(io::Error::last_os_error()),
        false => Ok(()),
    }
}

/// Runs a call that fills `arg` and returns -1 with `errno` set when it fails.
fn syscall<T>(mut arg: T, call: impl FnOnce(&mut T) -> libc::c_int) -> io::Result<T> {
    check(call(&mut arg))?;
    Ok(arg)
}

/// `qs_flags` of what `Q_XGETQSTAT` filled in.
fn flags_of(stat: QuotaStat) -> u16 {
    stat.flags
}

/// The project's limits and usage out of what `Q_XGETQUOTA` filled in.
fn dq_of(quota: DiskQuota) -> Dq {
    Dq {
        blocks_used: quota.bcount,
        inodes_used: quota.icount,
        blocks_limit: quota.blk_hardlimit,
        inodes_limit: quota.ino_hardlimit,
    }
}

/// The first argument of `quotactl`: the command, then the project type below it.
fn qcmd(command: u32) -> libc::c_int {
    ((command << 8) | XQM_PRJQUOTA) as libc::c_int
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// Opens a directory without following a link in the last component.
fn open_dir(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

impl Sys for Kernel {
    fn mountinfo(&self) -> io::Result<String> {
        fs::read_to_string("/proc/self/mountinfo")
    }

    fn block_rdev(&self, path: &Path) -> io::Result<Option<u64>> {
        let meta = fs::metadata(path)?;
        Ok(meta.file_type().is_block_device().then_some(meta.rdev()))
    }

    fn dev_of(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.dev())
    }

    fn quota_flags(&self, device: &Path) -> io::Result<u16> {
        let special = cstr(device)?;
        syscall(QuotaStat::default(), |stat| {
            // FFI-BOUNDARY: `special` is a NUL-terminated string owned here, alive for the
            // call; `stat` is a live exclusive borrow of a `#[repr(C)]` struct with the
            // layout of `fs_quota_stat`, which the kernel fills. The kernel keeps neither.
            unsafe {
                libc::quotactl(
                    qcmd(Q_XGETQSTAT),
                    special.as_ptr(),
                    0,
                    std::ptr::from_mut(stat).cast(),
                )
            }
        })
        .map(flags_of)
    }

    fn open_dir(&self, path: &Path) -> io::Result<File> {
        open_dir(path)
    }

    fn is_empty(&self, dir: &File) -> io::Result<bool> {
        // The descriptor's own listing, through its `/proc` link: the path is not looked up
        // again, the link resolves to the directory the descriptor holds. An entry that
        // cannot be read still is an entry, so it counts as not empty.
        let entries = fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?;
        Ok(entries.take(1).count() == 0)
    }

    fn get_attr(&self, dir: &File) -> io::Result<Fsx> {
        syscall(Fsx::default(), |attr| {
            // FFI-BOUNDARY: the descriptor `dir` is borrowed and open for the call; `attr`
            // is a live exclusive borrow of a `#[repr(C)]` struct with the layout of
            // `struct fsxattr` (28 bytes), which the kernel fills. It keeps neither.
            unsafe {
                libc::ioctl(
                    dir.as_raw_fd(),
                    FS_IOC_FSGETXATTR as _,
                    std::ptr::from_mut(attr),
                )
            }
        })
    }

    fn set_attr(&self, dir: &File, attr: Fsx) -> io::Result<()> {
        let mut attr = attr;
        // FFI-BOUNDARY: as `get_attr`, but the kernel only reads `attr`.
        check(unsafe {
            libc::ioctl(
                dir.as_raw_fd(),
                FS_IOC_FSSETXATTR as _,
                std::ptr::from_mut(&mut attr),
            )
        })
    }

    fn get_quota(&self, device: &Path, project: u32) -> io::Result<Dq> {
        let special = cstr(device)?;
        syscall(DiskQuota::default(), |quota| {
            // FFI-BOUNDARY: `special` is a NUL-terminated string owned here, alive for the
            // call; `quota` is a live exclusive borrow of a `#[repr(C)]` struct with the
            // layout of `fs_disk_quota` (112 bytes), which the kernel fills.
            unsafe {
                libc::quotactl(
                    qcmd(Q_XGETQUOTA),
                    special.as_ptr(),
                    project as libc::c_int,
                    std::ptr::from_mut(quota).cast(),
                )
            }
        })
        .map(dq_of)
    }

    fn set_limits(&self, device: &Path, project: u32, blocks: u64, inodes: u64) -> io::Result<()> {
        let special = cstr(device)?;
        let mut wanted = DiskQuota {
            version: FS_DQUOT_VERSION,
            flags: FS_PROJ_QUOTA,
            fieldmask: FS_DQ_HARD_LIMITS,
            id: project,
            blk_hardlimit: blocks,
            ino_hardlimit: inodes,
            ..DiskQuota::default()
        };
        // FFI-BOUNDARY: as `get_quota`, but the kernel only reads `wanted`, and only the
        // fields `fieldmask` names.
        check(unsafe {
            libc::quotactl(
                qcmd(Q_XSETQLIM),
                special.as_ptr(),
                project as libc::c_int,
                std::ptr::from_mut(&mut wanted).cast(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("js-quota-sys-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The structs are the size the kernel copies, and the ioctl numbers and commands are
    /// the ones the headers define. It catches a field added or reordered in the mirror of
    /// a kernel struct, which would make the kernel read or write the wrong bytes.
    #[test]
    fn layouts_and_numbers_match_the_kernel_headers() {
        assert_eq!(size_of::<Fsx>(), 28);
        assert_eq!(size_of::<DiskQuota>(), 112);
        assert_eq!(size_of::<QuotaStat>(), 80);
        assert_eq!(qcmd(Q_XSETQLIM), 0x0058_0402);
        assert_eq!(qcmd(Q_XGETQUOTA), 0x0058_0302);
        assert_eq!(qcmd(Q_XGETQSTAT), 0x0058_0502);
        assert_eq!(FS_IOC_FSGETXATTR, (2 << 30) | (28 << 16) | (0x58 << 8) | 31);
        assert_eq!(FS_IOC_FSSETXATTR, (1 << 30) | (28 << 16) | (0x58 << 8) | 32);
    }

    /// A call that returns -1 is the error of `errno`; one that returns 0 hands back what it
    /// filled in.
    #[test]
    fn a_failed_call_is_an_error_and_a_good_one_returns_its_argument() {
        let ok = syscall(1_u32, |n| {
            *n += 1;
            0
        });
        assert_eq!(ok.unwrap(), 2);
        let failed = syscall((), |_| -1);
        assert!(failed.is_err());
        assert!(check(0).is_ok());
        assert!(check(-1).is_err());
    }

    /// What the kernel filled in is read from the right fields: the flags of the state, and
    /// the used and the hard-limit fields of the project (not the soft ones).
    #[test]
    fn replies_are_read_from_the_right_fields() {
        let stat = QuotaStat {
            flags: 0x30,
            version: 1,
            ..QuotaStat::default()
        };
        assert_eq!(flags_of(stat), 0x30);
        let quota = DiskQuota {
            bcount: 1,
            icount: 2,
            blk_hardlimit: 3,
            ino_hardlimit: 4,
            blk_softlimit: 5,
            ino_softlimit: 6,
            ..DiskQuota::default()
        };
        let want = Dq {
            blocks_used: 1,
            inodes_used: 2,
            blocks_limit: 3,
            inodes_limit: 4,
        };
        assert_eq!(dq_of(quota), want);
    }

    /// A path with a NUL cannot be passed to the kernel.
    #[test]
    fn a_path_with_a_nul_is_invalid() {
        let bad = cstr(Path::new("a\0b")).unwrap_err();
        assert_eq!(bad.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            cstr(Path::new("/dev/null")).unwrap().as_bytes(),
            b"/dev/null"
        );
    }

    /// The mount table of this process can be read, and a character device is not a block
    /// device, and a path that is not there is an error.
    #[test]
    fn reads_the_mount_table_and_tells_a_block_device() {
        assert!(Kernel.mountinfo().unwrap().contains(" - "));
        assert_eq!(Kernel.block_rdev(Path::new("/dev/null")).unwrap(), None);
        assert!(Kernel.block_rdev(Path::new("/dev/none-such")).is_err());
        let root = Kernel.dev_of(Path::new("/")).unwrap();
        assert_eq!(root, std::fs::metadata("/").unwrap().dev());
        assert!(Kernel.dev_of(Path::new("/dev/none-such")).is_err());
    }

    /// The quota calls on something that is not a block device fail, for a path the kernel
    /// is given and for one it cannot be given.
    #[test]
    fn quota_calls_on_a_node_that_is_no_device_fail() {
        let null = Path::new("/dev/null");
        assert!(Kernel.quota_flags(null).is_err());
        assert!(Kernel.get_quota(null, 5).is_err());
        assert!(Kernel.set_limits(null, 5, 1, 1).is_err());
        let nul = Path::new("a\0b");
        assert!(Kernel.quota_flags(nul).is_err());
        assert!(Kernel.get_quota(nul, 5).is_err());
        assert!(Kernel.set_limits(nul, 5, 1, 1).is_err());
    }

    /// A directory that is not there cannot be opened. A link to a directory is refused and
    /// not followed (`O_NOFOLLOW`: without it the open would succeed and the project id
    /// would land on whatever the link points at), and so is a plain file. For a directory
    /// that is there, the listing says whether it is empty, and a descriptor that is no
    /// directory cannot be listed. The attribute calls on an open directory get whatever
    /// its file system answers (an ordinary one has no project quota, a tmpfs may not know
    /// the call): the test makes them and takes the answer as it comes, because the real
    /// effect is tested on an XFS host.
    #[test]
    fn the_project_calls_work_on_one_open_directory() {
        let gone = Path::new("/nonexistent-js-quota-dir");
        assert_eq!(
            Kernel.open_dir(gone).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let dir = temp("attr");
        let link = dir.with_extension("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let refused = Kernel.open_dir(&link).unwrap_err();
        assert!(
            matches!(refused.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
            "{refused:?}"
        );
        let file = dir.join("plain");
        fs::write(&file, "x").unwrap();
        assert!(Kernel.open_dir(&file).is_err());
        let open = Kernel.open_dir(&dir).unwrap();
        assert!(!Kernel.is_empty(&open).unwrap());
        fs::remove_file(&file).unwrap();
        assert!(Kernel.is_empty(&open).unwrap());
        assert!(Kernel.is_empty(&File::open("/dev/null").unwrap()).is_err());
        let _ = Kernel.get_attr(&open);
        let _ = Kernel.set_attr(&open, Fsx::default());
        drop(open);
        fs::remove_file(&link).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
