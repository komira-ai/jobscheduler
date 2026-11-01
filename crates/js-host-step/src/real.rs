//! The machine.

use std::io;
use std::path::{Path, PathBuf};

use crate::{Cmd, Host, Meta, Output};

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

    /// The machine with every path taken below `root`.
    pub fn under(root: impl Into<PathBuf>) -> RealHost {
        RealHost { root: root.into() }
    }
}

impl Default for RealHost {
    fn default() -> RealHost {
        RealHost::new()
    }
}

fn unfinished<T>(root: &Path) -> io::Result<T> {
    Err(io::Error::other(format!(
        "not written yet ({})",
        root.display()
    )))
}

impl Host for RealHost {
    fn euid(&self) -> u32 {
        u32::MAX
    }

    fn egid(&self) -> u32 {
        u32::MAX
    }

    fn stat(&self, _path: &str) -> io::Result<Option<Meta>> {
        unfinished(&self.root)
    }

    fn read(&self, _path: &str) -> io::Result<Option<Vec<u8>>> {
        unfinished(&self.root)
    }

    fn write(&self, _path: &str, _mode: u32, _owner: (u32, u32), _bytes: &[u8]) -> io::Result<()> {
        unfinished(&self.root)
    }

    fn mkdir(&self, _path: &str, _mode: u32, _owner: (u32, u32)) -> io::Result<()> {
        unfinished(&self.root)
    }

    fn remove(&self, _path: &str) -> io::Result<()> {
        unfinished(&self.root)
    }

    fn run(&self, _cmd: &Cmd) -> io::Result<Output> {
        unfinished(&self.root)
    }
}
