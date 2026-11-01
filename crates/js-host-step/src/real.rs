//! The machine.
//!
//! Every change to the file system walks from the root directory one component at a time with
//! `openat(O_NOFOLLOW | O_DIRECTORY)`, so a symbolic link in any directory on the way (a
//! directory the service user owns can be made one at any time) is refused instead of followed.
//! Directories are made with `mkdirat`, and mode and owner are set on the open descriptor
//! (`fchmod`, `fchown`), never on a path. A file is written to a temporary name in its
//! directory through the parent's descriptor and renamed over the target with `renameat`.
//! Every directory on the way must be owned by root, by the effective user or by the owner
//! the step asks for.

use std::fs::{self, File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, fchown};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::fs::{AtFlags, Mode as FileMode, OFlags, mkdirat, openat, renameat, unlinkat};
use rustix::io::Errno;

use crate::{Cmd, Host, Kind, Meta, Output};

/// The real file system and the real programs, under a root directory.
#[derive(Clone, Debug)]
pub struct RealHost {
    root: PathBuf,
    trusted: Vec<u32>,
}

impl RealHost {
    /// The machine itself: paths are the system's.
    pub fn new() -> RealHost {
        RealHost::under("/")
    }

    /// The machine with every path taken below `root` (a directory standing in for `/`).
    /// Programs still run on the machine. Directories owned by root or by the effective user
    /// are trusted on the way to a path.
    pub fn under(root: impl Into<PathBuf>) -> RealHost {
        let euid = rustix::process::geteuid().as_raw();
        RealHost::trusting(root, &[0, euid])
    }

    /// Like [`RealHost::under`], trusting directories owned by `uids` (and by the owner a
    /// step asks for) on the way to a path.
    pub fn trusting(root: impl Into<PathBuf>, uids: &[u32]) -> RealHost {
        RealHost {
            root: root.into(),
            trusted: uids.to_vec(),
        }
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    /// The directory that holds `path` as a descriptor, and the name in it. Each directory on
    /// the way is opened without following links, made (root's, 0755) when `create` and
    /// missing, and must be owned by a trusted user or by `owner`.
    fn parent(&self, path: &str, create: bool, owner: u32) -> io::Result<(File, String)> {
        let mut names: Vec<&str> = path.split('/').filter(|n| !n.is_empty()).collect();
        let name = names.pop().ok_or_else(|| refused(path, "names no file"))?;
        let mut dir = File::from(openat(
            rustix::fs::CWD,
            &self.root,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            FileMode::empty(),
        )?);
        for component in names {
            let flags = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let opened = match openat(&dir, component, flags, FileMode::empty()) {
                Err(Errno::NOENT) if create => {
                    mkdirat(&dir, component, FileMode::from_raw_mode(0o755))?;
                    openat(&dir, component, flags, FileMode::empty())
                }
                other => other,
            };
            let next = File::from(opened.map_err(|e| match e {
                Errno::LOOP | Errno::NOTDIR => {
                    refused(path, "has a symbolic link or a file as a directory")
                }
                other => io::Error::from(other),
            })?);
            let uid = next.metadata()?.uid();
            if !self.trusted.contains(&uid) && uid != owner {
                return Err(refused(path, "has a directory owned by another user"));
            }
            dir = next;
        }
        Ok((dir, name.to_owned()))
    }

    /// Writes `bytes` to `temp` in `dir`, sets mode and owner on the descriptor and renames
    /// it to `name`.
    fn put(
        dir: &File,
        temp: &str,
        name: &str,
        mode: u32,
        owner: (u32, u32),
        bytes: &[u8],
    ) -> io::Result<()> {
        found(unlinkat(dir, temp, AtFlags::empty()).map_err(io::Error::from))?;
        // Created 0600 so the bytes are never readable wider than the mode asked for.
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut file = File::from(openat(dir, temp, flags, FileMode::from_raw_mode(0o600))?);
        file.write_all(bytes)?;
        file.set_permissions(Permissions::from_mode(mode))?;
        fchown(&file, Some(owner.0), Some(owner.1))?;
        file.sync_all()?;
        Ok(renameat(dir, temp, dir, name)?)
    }
}

impl Default for RealHost {
    fn default() -> RealHost {
        RealHost::new()
    }
}

fn refused(path: &str, why: &str) -> io::Error {
    io::Error::other(format!("refused: {path} {why}"))
}

/// `result`, or `None` when the error is "not found".
fn found<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl Host for RealHost {
    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn egid(&self) -> u32 {
        rustix::process::getegid().as_raw()
    }

    fn canonical(&self, path: &str) -> io::Result<String> {
        let root = fs::canonicalize(&self.root)?;
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        let mut near = self.at(path);
        // The longest part that exists is resolved; the part that does not stays as given.
        let real = loop {
            match fs::canonicalize(&near) {
                Ok(real) => break real,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    rest.extend(near.file_name().map(ToOwned::to_owned));
                    near.pop();
                }
                Err(e) => return Err(e),
            }
        };
        let mut full = real;
        full.extend(rest.iter().rev());
        let below = full.strip_prefix(&root).unwrap_or(Path::new(""));
        Ok(format!("/{}", below.display()))
    }

    fn stat(&self, path: &str) -> io::Result<Option<Meta>> {
        let meta = found(fs::symlink_metadata(self.at(path)))?;
        Ok(meta.map(|m| Meta {
            kind: match (m.is_file(), m.is_dir()) {
                (true, _) => Kind::File,
                (_, true) => Kind::Dir,
                _ => Kind::Other,
            },
            mode: m.mode() & 0o7777,
            uid: m.uid(),
            gid: m.gid(),
            size: m.len(),
        }))
    }

    fn read(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        found(fs::read(self.at(path)))
    }

    fn read_limited(&self, path: &str, _limit: u64) -> io::Result<Option<Vec<u8>>> {
        self.read(path)
    }

    fn write(&self, path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) -> io::Result<()> {
        let (dir, name) = self.parent(path, false, owner.0)?;
        let temp = format!("{name}.js-host.tmp");
        let done = RealHost::put(&dir, &temp, &name, mode, owner, bytes);
        if done.is_err() {
            // A failed write leaves no temporary file behind.
            let _ = unlinkat(&dir, &temp, AtFlags::empty());
        }
        done
    }

    fn mkdir(&self, path: &str, mode: u32, owner: (u32, u32)) -> io::Result<()> {
        let (dir, name) = self.parent(path, true, owner.0)?;
        match mkdirat(&dir, &name, FileMode::from_raw_mode(0o700)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        let flags = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = openat(&dir, &name, flags, FileMode::empty())
            .map_err(|_| refused(path, "is a symbolic link or not a directory"))?;
        let made = File::from(opened);
        made.set_permissions(Permissions::from_mode(mode))?;
        fchown(&made, Some(owner.0), Some(owner.1))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        let euid = self.euid();
        let Some((dir, name)) = found(self.parent(path, false, euid))? else {
            return Ok(());
        };
        found(unlinkat(&dir, &name, AtFlags::empty()).map_err(io::Error::from)).map(|_| ())
    }

    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        let out = Command::new(&cmd.program)
            .args(&cmd.args)
            .env_clear()
            .stdin(Stdio::null())
            .output()?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}
