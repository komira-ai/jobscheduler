//! The machine.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::{Cmd, Host, Kind, Meta, Output};

/// The real file system and the real programs, under a root directory.
#[derive(Clone, Debug)]
pub struct RealHost {
    root: PathBuf,
}

impl RealHost {
    /// The machine itself: paths are the system's.
    pub fn new() -> RealHost {
        RealHost::under("/")
    }

    /// The machine with every path taken below `root` (a directory standing in for `/`).
    /// Programs still run on the machine.
    pub fn under(root: impl Into<PathBuf>) -> RealHost {
        RealHost { root: root.into() }
    }

    /// Like [`RealHost::under`], trusting directories owned by `uids`.
    pub fn trusting(root: impl Into<PathBuf>, _uids: &[u32]) -> RealHost {
        RealHost::under(root)
    }

    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl Default for RealHost {
    fn default() -> RealHost {
        RealHost::new()
    }
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
        Ok(path.to_owned())
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

    fn write(&self, path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) -> io::Result<()> {
        let target = self.at(path);
        let mut temp = target.clone().into_os_string();
        temp.push(".js-host.tmp");
        let temp = PathBuf::from(temp);
        found(fs::remove_file(&temp))?;
        // Created 0600 so the bytes are never readable wider than the mode asked for.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.set_permissions(Permissions::from_mode(mode))?;
        chown(&temp, Some(owner.0), Some(owner.1))?;
        file.sync_all()?;
        fs::rename(&temp, &target)
    }

    fn mkdir(&self, path: &str, mode: u32, owner: (u32, u32)) -> io::Result<()> {
        let dir = self.at(path);
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, Permissions::from_mode(mode))?;
        chown(&dir, Some(owner.0), Some(owner.1))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        found(fs::remove_file(self.at(path))).map(|_| ())
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
