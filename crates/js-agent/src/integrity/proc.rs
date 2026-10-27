//! The Linux host as the checks read it: `/proc`, `/etc/passwd`, `/etc/group` and the
//! listings of the watched directories, through two seams so that a test feeds recorded
//! text instead of the machine it runs on.
//!
//! [`FileReader`] reads a file; [`DirLister`] lists a directory (the `/proc` directory is
//! how the process table is found). Nothing is skipped: an input that cannot be read or
//! understood is an error, and the check that needed it is a finding. The one thing that is
//! not an error is a process that ends while the table is read: its files are gone and so
//! is its entry in `/proc`, which the table confirms by listing `/proc` again before it
//! lets the process go.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::view::{HostView, Mount, Process, Watched};
use crate::facts::{FileReader, SystemFiles};

/// The most entries of a watched directory that are read. A directory with more cannot be
/// digested and is a finding.
pub const WATCH_ENTRY_CAP: usize = 4096;

/// The most processes read. A host with more cannot be checked and is a finding.
pub const PROCESS_CAP: usize = 1 << 16;

/// One entry of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name.
    pub name: String,
    /// The size in bytes; `u64::MAX` for an entry that could not be examined.
    pub size: u64,
    /// The modification time: seconds and nanoseconds since the epoch.
    pub mtime: (i64, i64),
    /// The status change time: seconds and nanoseconds since the epoch. A job cannot set
    /// it, so it moves when the contents are rewritten and the modification time put back.
    pub ctime: (i64, i64),
    /// The inode number: a file replaced by another has a different one.
    pub ino: u64,
    /// The owning user.
    pub uid: u32,
    /// The owning group.
    pub gid: u32,
    /// The type and permission bits.
    pub mode: u32,
}

/// What [`DirLister::list`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// At most the cap's entries, in no particular order.
    pub entries: Vec<Entry>,
    /// There are more entries than the cap.
    pub more: bool,
}

/// Lists a directory.
pub trait DirLister {
    /// The first `cap` entries of `dir`, and whether there are more. A link is an entry of
    /// its own and is not followed.
    ///
    /// # Errors
    ///
    /// The directory cannot be read; the text names it.
    fn list(&self, dir: &Path, cap: usize) -> Result<Listing, String>;
}

/// The machine's own directories.
#[derive(Debug, Clone, Copy)]
pub struct SystemDirs;

impl DirLister for SystemDirs {
    /// An entry that cannot be examined (it ended between the listing and the look, or the
    /// agent may not look) is listed with `u64::MAX` as its size and the largest values for
    /// the rest, so its name is still part of what is compared.
    fn list(&self, dir: &Path, cap: usize) -> Result<Listing, String> {
        let named = |err: io::Error| format!("{}: {err}", dir.display());
        fs::read_dir(dir)
            .map_err(named)
            .and_then(|read| {
                read.take(cap.saturating_add(1))
                    .map(|item| item.map(|item| entry_of(&item)).map_err(named))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map(|mut entries| {
                let more = entries.len() > cap;
                entries.truncate(cap);
                Listing { entries, more }
            })
    }
}

/// The entry for `item`, looked at without following a link.
fn entry_of(item: &fs::DirEntry) -> Entry {
    let meta = item.metadata().ok();
    Entry {
        name: item.file_name().to_string_lossy().into_owned(),
        size: meta.as_ref().map_or(u64::MAX, fs::Metadata::len),
        mtime: meta
            .as_ref()
            .map_or((i64::MAX, 0), |meta| (meta.mtime(), meta.mtime_nsec())),
        ctime: meta
            .as_ref()
            .map_or((i64::MAX, 0), |meta| (meta.ctime(), meta.ctime_nsec())),
        ino: meta.as_ref().map_or(u64::MAX, MetadataExt::ino),
        uid: meta.as_ref().map_or(u32::MAX, MetadataExt::uid),
        gid: meta.as_ref().map_or(u32::MAX, MetadataExt::gid),
        mode: meta.as_ref().map_or(u32::MAX, MetadataExt::mode),
    }
}

/// The Linux host: [`HostView`] over `/proc`, `/etc/passwd`, `/etc/group` and directory
/// listings.
pub struct ProcHost {
    files: Box<dyn FileReader>,
    dirs: Box<dyn DirLister>,
    process_cap: usize,
    watch_cap: usize,
}

impl ProcHost {
    /// The host read through `files` and `dirs`, with the caps [`PROCESS_CAP`] and
    /// [`WATCH_ENTRY_CAP`].
    pub fn new(files: Box<dyn FileReader>, dirs: Box<dyn DirLister>) -> Self {
        Self {
            files,
            dirs,
            process_cap: PROCESS_CAP,
            watch_cap: WATCH_ENTRY_CAP,
        }
    }

    /// The machine itself.
    pub fn system() -> Self {
        Self::new(Box::new(SystemFiles), Box::new(SystemDirs))
    }

    /// The same host with other caps, for a test that must not build a huge directory.
    #[must_use]
    pub fn with_caps(self, process_cap: usize, watch_cap: usize) -> Self {
        Self {
            process_cap,
            watch_cap,
            ..self
        }
    }

    /// The process `pid`, or `None` when it has ended since `/proc` was listed.
    fn process(&self, pid: u32) -> Result<Option<Process>, String> {
        let read = |name: &str| self.files.read(&format!("/proc/{pid}/{name}"));
        read("stat")
            .and_then(|stat| {
                read("status")
                    .and_then(|status| read("cgroup").map(|cgroup| (stat, status, cgroup)))
            })
            .and_then(|(stat, status, cgroup)| {
                parse_process(pid, &stat, &status, &cgroup)
                    .ok_or_else(|| format!("/proc/{pid}: cannot read the process entry"))
            })
            .map(Some)
            .or_else(|why| {
                self.listed(pid).and_then(|there| match there {
                    true => Err(why),
                    false => Ok(None),
                })
            })
    }

    /// Whether `/proc` lists `pid` now.
    fn listed(&self, pid: u32) -> Result<bool, String> {
        let name = pid.to_string();
        self.dirs
            .list(Path::new("/proc"), self.process_cap)
            .map(|listing| listing.entries.iter().any(|entry| entry.name == name))
    }
}

impl HostView for ProcHost {
    fn processes(&self) -> Result<Vec<Process>, String> {
        self.dirs
            .list(Path::new("/proc"), self.process_cap)
            .and_then(|listing| match listing.more {
                true => Err(format!("/proc: more than {} processes", self.process_cap)),
                false => Ok(listing.entries),
            })
            .and_then(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.name.parse::<u32>().ok())
                    .map(|pid| self.process(pid))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map(|found| found.into_iter().flatten().collect())
    }

    fn mounts(&self) -> Result<Vec<Mount>, String> {
        self.files.read("/proc/self/mountinfo").and_then(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    parse_mount(line).ok_or_else(|| {
                        format!("/proc/self/mountinfo: cannot read the line {line:?}")
                    })
                })
                .collect()
        })
    }

    fn users(&self) -> Result<BTreeSet<String>, String> {
        self.files
            .read("/etc/passwd")
            .and_then(|text| parse_database("/etc/passwd", &text))
    }

    fn groups(&self) -> Result<BTreeSet<String>, String> {
        self.files
            .read("/etc/group")
            .and_then(|text| parse_database("/etc/group", &text))
    }

    fn modules(&self) -> Result<BTreeSet<String>, String> {
        self.files.read("/proc/modules").map(|text| {
            text.lines()
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_owned)
                .collect()
        })
    }

    fn watched(&self, dir: &Path) -> Result<Watched, String> {
        self.dirs
            .list(dir, self.watch_cap)
            .map(|listing| match listing.more {
                true => Watched::TooMany,
                false => Watched::Digest(digest(listing.entries)),
            })
    }
}

/// The process `pid` from the text of its `stat`, `status` and `cgroup` files. The name in
/// `stat` is in parentheses and may hold spaces and parentheses, so the fields are counted
/// from the last parenthesis: the parent is the second of them and the start time the
/// twentieth.
fn parse_process(pid: u32, stat: &str, status: &str, cgroup: &str) -> Option<Process> {
    let fields: Vec<&str> = stat
        .rfind(')')
        .and_then(|at| stat.get(at + 1..))
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let ppid = fields.get(1).and_then(|field| field.parse().ok());
    let start = fields.get(19).and_then(|field| field.parse().ok());
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().next())
        .and_then(|id| id.parse().ok());
    ppid.zip(start)
        .zip(uid)
        .map(|((ppid, start), uid)| Process {
            pid,
            ppid,
            uid,
            start,
            cgroup: cgroup.split_whitespace().collect::<Vec<_>>().join(" "),
        })
}

/// A line of `/proc/self/mountinfo`: `id parent major:minor root point options [tags] -
/// type source super-options`. The numbers are the kernel's for this mount alone and are
/// left out.
fn parse_mount(line: &str) -> Option<Mount> {
    line.split_once(" - ").and_then(|(before, after)| {
        let before: Vec<&str> = before.split(' ').collect();
        let after: Vec<&str> = after.split(' ').collect();
        before
            .get(3)
            .zip(before.get(4))
            .zip(after.first().zip(after.get(1)))
            .map(|((root, point), (fstype, source))| Mount {
                root: (*root).to_owned(),
                point: (*point).to_owned(),
                fstype: (*fstype).to_owned(),
                source: (*source).to_owned(),
            })
    })
}

/// The entries of `/etc/passwd` or `/etc/group`: every line that is not blank or a comment
/// must hold a colon, or the file is not one of these.
fn parse_database(path: &str, text: &str) -> Result<BTreeSet<String>, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.contains(':') {
            true => Ok(line.to_owned()),
            false => Err(format!("{path}: a line is not an entry")),
        })
        .collect()
}

/// The digest of a directory's entries, whatever order they were listed in. A name cannot
/// hold a NUL, so the NULs keep one entry's fields from running into the next.
fn digest(mut entries: Vec<Entry>) -> String {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut hash = Sha256::new();
    for entry in &entries {
        let ((seconds, nanos), (c_seconds, c_nanos)) = (entry.mtime, entry.ctime);
        hash.update(format!(
            "{}\0{}\0{seconds}\0{nanos}\0{c_seconds}\0{c_nanos}\0{}\0{}\0{}\0{}\0",
            entry.name, entry.size, entry.ino, entry.uid, entry.gid, entry.mode
        ));
    }
    hex::encode(hash.finalize())
}
