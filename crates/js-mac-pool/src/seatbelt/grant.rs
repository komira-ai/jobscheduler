//! What the allow-list read model grants beyond its fixed base set, and the proof
//! objects each grant is made from.

use std::path::Path;

use js_bundle::{DiffId, ReadGrantKind, tree_path};

use super::path::{AbsPath, PathError};
use crate::xcode::HostXcode;

/// A tree of the bundle store that a run has pinned. Its directory is a function of the
/// store root and the diff id (`<root>/v1/objects/trees/<hex>`), so a grant names one
/// content-addressed tree and never a directory that holds others.
///
/// The store's own handle type lives in a runtime crate this one may not depend on; the
/// agent makes this from the handle it holds, and the path is the same function of the
/// same two inputs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PinnedTree {
    diff_id: DiffId,
    path: AbsPath,
}

impl PinnedTree {
    /// The tree of `diff_id` in the store at `store_root`.
    ///
    /// # Errors
    /// A [`PathError`] when the tree's directory is not an acceptable path.
    pub fn new(store_root: &AbsPath, diff_id: &DiffId) -> Result<Self, PathError> {
        let path = tree_path(Path::new(store_root.as_str()), diff_id);
        Ok(Self {
            diff_id: diff_id.clone(),
            path: AbsPath::new(&path)?,
        })
    }

    /// The tree's diff id.
    #[must_use]
    pub fn diff_id(&self) -> &DiffId {
        &self.diff_id
    }

    /// The tree's directory.
    #[must_use]
    pub fn path(&self) -> &AbsPath {
        &self.path
    }
}

/// One read grant of the allow-list model. Each is made from a proof object: a pinned
/// tree, a ready Xcode of the agent's catalog, or a kind of the closed vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadGrant {
    /// The files of a pinned tree.
    Tree(PinnedTree),
    /// The selected Xcode's app and its fixed support files.
    Xcode(HostXcode),
    /// A fixed set of system files a bundle declares it needs.
    Kind(ReadGrantKind),
}

/// How a rule names a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The path itself.
    Literal,
    /// The path and everything below it.
    Subpath,
}

/// The fixed files of a [`ReadGrantKind`]. `SysctlInfo` has none: it is a sysctl class,
/// and sysctl reads are not file reads.
#[must_use]
pub fn kind_rules(kind: ReadGrantKind) -> &'static [(Match, &'static str)] {
    match kind {
        ReadGrantKind::SystemCerts => &[
            (Match::Subpath, "/private/etc/ssl"),
            (Match::Subpath, "/System/Library/Security"),
        ],
        ReadGrantKind::Timezone => &[
            (Match::Literal, "/private/etc/localtime"),
            (Match::Subpath, "/usr/share/zoneinfo"),
        ],
        ReadGrantKind::ResolverConfig => &[
            (Match::Literal, "/private/etc/resolv.conf"),
            (Match::Literal, "/private/var/run/resolv.conf"),
        ],
        ReadGrantKind::SysctlInfo => &[],
        ReadGrantKind::Locale => &[(Match::Subpath, "/usr/share/locale")],
    }
}

/// The files outside the Xcode app that its tools read: the licence record.
pub(super) const XCODE_SUPPORT: [(Match, &str); 1] = [(
    Match::Literal,
    "/Library/Preferences/com.apple.dt.Xcode.plist",
)];

/// Directories whose whole content is never granted: each holds other users' or other
/// tenants' data, or the agent's own. A grant names the literal file it needs instead.
const DIRECTORY_WIDE: [&str; 17] = [
    "",
    "/System",
    "/System/Volumes/Data",
    "/Library",
    "/Library/Apple",
    "/Library/Preferences",
    "/Users",
    "/Users/Shared",
    "/Volumes",
    "/opt",
    "/opt/homebrew",
    "/private",
    "/private/etc",
    "/private/tmp",
    "/private/var",
    "/private/var/db",
    "/private/var/tmp",
];

/// Whether a subpath rule on `path` would be a directory-wide grant (the root, and the
/// directories of the table above). The base sets and the kinds hold none; `""` and `/`
/// are the root.
#[must_use]
pub fn is_directory_wide(path: &str) -> bool {
    DIRECTORY_WIDE.contains(&path.trim_end_matches('/'))
}
