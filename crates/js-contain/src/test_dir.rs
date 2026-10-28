//! [`TestDir`]: a scratch directory a unit test owns, removed when it is dropped.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory removed (with everything in it) on drop.
#[derive(Debug)]
pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// A new, empty directory `<root>/<name>-<pid>-<n>`, making `root` if needed.
    ///
    /// # Panics
    /// If the directory cannot be made (a test cannot go on without it).
    pub(crate) fn under(root: impl AsRef<Path>, name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root
            .as_ref()
            .join(format!("{name}-{}-{n}", std::process::id()));
        // Absent unless an earlier process of this pid left it.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("make test dir");
        Self { path }
    }
}

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
