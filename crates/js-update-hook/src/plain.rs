//! The spelling rule for the paths a hook trusts.

use std::path::{Component, Path, PathBuf};

/// Whether `path` is spelt the way it would be after canonicalising it lexically: absolute,
/// no `.` or `..`, no doubled or trailing slash.
#[must_use]
pub fn is_plain(path: &Path) -> bool {
    let rebuilt: PathBuf = path.components().collect();
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && rebuilt.as_os_str() == path.as_os_str()
}
