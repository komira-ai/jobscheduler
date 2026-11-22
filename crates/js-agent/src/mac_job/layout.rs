//! The job folder and the environment the job runs in.
//!
//! ```text
//! <root>/<id>/          the job folder, the only place the profile lets the job write
//!     work/             the working directory; the inputs are copied here
//!     home/             HOME and CFFIXED_USER_HOME
//!     tmp/              TMPDIR
//!     cache/            XDG_CACHE_HOME; cache/clang is CLANG_MODULE_CACHE_PATH
//!                       (and tmp/xcrun_db is xcrun's cache, `xcrun_db`)
//! ```
//!
//! Every place a build tool writes to by default (the home, the temporary folder, the caches,
//! clang's module cache and xcrun's cache, which otherwise go to the user's folder under
//! `/var/folders`) is inside the job folder, so removing the folder removes it. Measured on
//! macOS 27: without `xcrun_db`, every `cc`, `swiftc` and `otool` of a slot account printed
//! "couldn't create cache file .../T/xcrun_db-..." (the profile refuses that folder); with it,
//! the cache is made at the path it names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::spec::JobFile;

/// The `PATH` a job starts with.
pub const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The shell a job is told it has.
pub const SHELL: &str = "/bin/sh";

/// The subfolders of a job folder, in the order they are made.
pub const SUBFOLDERS: [&str; 4] = ["work", "home", "tmp", "cache"];

/// Where a job's folders are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The job folder.
    pub folder: PathBuf,
}

impl Layout {
    /// The layout of job `id` under `root`.
    #[must_use]
    pub fn new(root: &Path, id: &str) -> Self {
        Self {
            folder: root.join(id),
        }
    }

    /// The working directory.
    #[must_use]
    pub fn work(&self) -> PathBuf {
        self.folder.join("work")
    }

    /// `HOME`.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.folder.join("home")
    }

    /// `TMPDIR`.
    #[must_use]
    pub fn tmp(&self) -> PathBuf {
        self.folder.join("tmp")
    }

    /// `XDG_CACHE_HOME`.
    #[must_use]
    pub fn cache(&self) -> PathBuf {
        self.folder.join("cache")
    }

    /// The environment of `job`, run as `user`: the job's own variables, then the agent's,
    /// which a job cannot set ([`super::spec::RESERVED_ENV`]).
    #[must_use]
    pub fn env(&self, job: &JobFile, user: &str) -> BTreeMap<String, String> {
        let mut env = job.env.clone();
        let text = |p: PathBuf| p.to_string_lossy().into_owned();
        let home = text(self.home());
        // A trailing `/` as macOS's own TMPDIR has: tools join names to it without one.
        let tmp = format!("{}/", text(self.tmp()));
        let cache = text(self.cache());
        let clang = format!("{cache}/clang");
        let xcrun = format!("{}/xcrun_db", text(self.tmp()));
        for (name, value) in [
            ("HOME", home.clone()),
            ("CFFIXED_USER_HOME", home),
            ("TMPDIR", tmp),
            ("XDG_CACHE_HOME", cache),
            ("CLANG_MODULE_CACHE_PATH", clang),
            ("xcrun_db", xcrun),
            ("USER", user.to_owned()),
            ("LOGNAME", user.to_owned()),
            ("SHELL", SHELL.to_owned()),
            ("PATH", PATH.to_owned()),
        ] {
            env.insert(name.to_owned(), value);
        }
        env
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
