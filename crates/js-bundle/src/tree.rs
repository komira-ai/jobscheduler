//! The path rules of a layer and the in-memory model of its tree.
//!
//! Both the packer and the extractor run every entry through the same functions, so a
//! layer the packer writes is a layer the extractor admits and the other way round. The
//! model holds the path of every entry and what it is (directory, file, symlink with its
//! target); it knows nothing of any file system. Names are ASCII by rule, so every path
//! in it is a `str`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The longest whole relative path, in bytes (64 components of 255 bytes would overrun
/// `PATH_MAX` and break every path-based consumer).
pub const MAX_PATH_BYTES: usize = 512;
/// The longest component of a path, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// The most components in a path.
pub const MAX_DEPTH: usize = 64;
/// The longest symlink target, in bytes.
pub const MAX_TARGET_BYTES: usize = 1024;
/// The most symlinks followed in resolving one target.
const MAX_HOPS: u32 = 8;
/// The walker's fixed budget of resolution steps (one per path component followed), before
/// the per-link share. The packer's own output stays far below the whole budget: a link
/// takes a handful of steps, the budget gives 64 per link.
const WALK_BASE_STEPS: u64 = 65_536;
/// The steps the walker budget adds for each symlink.
const WALK_STEPS_PER_LINK: u64 = 64;
/// The default cap on the bytes the model of a tree may hold (see [`Tree::new`]): 256 MiB,
/// a thousand times the model of the largest layer the first bundles hold.
pub const DEFAULT_MODEL_BYTES: u64 = 1 << 28;

/// Names that APFS tools treat specially, refused at the tree root in any letter case (the
/// volume may be case-insensitive, where `.TRASHES` is `.Trashes`): the volume that a tree is
/// unpacked onto creates them itself.
const ROOT_RESERVED: [&str; 6] = [
    ".fseventsd",
    ".Spotlight-V100",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".metadata_never_index",
];

/// Why a path or a symlink target is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameDefect {
    /// There is nothing in it.
    #[error("is empty")]
    Empty,
    /// It starts at the file-system root.
    #[error("is absolute")]
    Absolute,
    /// A byte is outside `0x21..=0x7e` (this includes NUL, spaces, control bytes, `0x7f`
    /// and every byte of a non-ASCII or non-UTF-8 name).
    #[error("has a byte outside 0x21..=0x7e")]
    ForbiddenByte,
    /// Two separators in a row, or one at the end.
    #[error("has an empty component")]
    EmptyComponent,
    /// A path component is `.` or `..`.
    #[error("has a `.` or `..` component")]
    DotComponent,
    /// A component is over 255 bytes.
    #[error("has a component over 255 bytes")]
    ComponentTooLong,
    /// A component starts with `._` (an AppleDouble file).
    #[error("has a component that starts with `._`")]
    AppleDouble,
    /// The path is over 512 bytes, or the target over 1024.
    #[error("is over its length cap")]
    TooLong,
    /// The path has more than 64 components.
    #[error("is deeper than 64 components")]
    TooDeep,
    /// The first component is a name the volume reserves.
    #[error("names an entry the volume reserves at the tree root")]
    RootReserved,
}

/// Why an entry cannot join the tree built so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EntryDefect {
    /// An earlier entry has this path.
    #[error("appears twice")]
    Duplicate,
    /// An earlier entry has a path that differs only in ASCII case.
    #[error("differs only in case from an earlier entry")]
    CaseCollision,
    /// The parent directory is not an earlier entry of the stream.
    #[error("comes before its parent directory")]
    ParentMissing,
    /// The parent is an earlier entry, but not a directory.
    #[error("has a parent that is not a directory")]
    ParentNotDirectory,
    /// The paths and targets of the entries so far are more than the model may hold.
    #[error("is past the cap on the bytes of paths the tree model holds")]
    ModelTooLarge,
}

/// Why a symlink is refused by the walker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkDefect {
    /// Resolving the target, as the kernel does, steps above the tree root.
    #[error("resolves above the tree root")]
    Escapes,
    /// Resolving the target goes through something that is not a directory.
    #[error("resolves through a file")]
    NotDirectory,
    /// Resolving the target follows more than 8 symlinks.
    #[error("follows more than 8 symlinks")]
    TooManyHops,
    /// The target reaches a missing entry and then `.` or `..`.
    #[error("dangles and then climbs")]
    Dangling,
    /// The walker's budget of steps for the whole tree ran out here.
    #[error("is where the walker ran out of steps")]
    OverBudget,
}

/// Why a path, a symlink target or an entry of a layer is refused. Every path is shown
/// escaped and cut to 128 bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeRefusal {
    /// The path of an entry breaks a naming rule.
    #[error("path {path:?} {defect}")]
    Path {
        /// The path.
        path: String,
        /// The rule.
        defect: NameDefect,
    },
    /// The target of a symlink breaks a naming rule.
    #[error("target of the symlink {path:?} {defect}")]
    Target {
        /// The symlink's path.
        path: String,
        /// The rule.
        defect: NameDefect,
    },
    /// The entry cannot join the tree.
    #[error("entry {path:?} {defect}")]
    Entry {
        /// The path.
        path: String,
        /// The rule.
        defect: EntryDefect,
    },
    /// The symlink fails the walker.
    #[error("symlink {path:?} {defect}")]
    Link {
        /// The symlink's path.
        path: String,
        /// The rule.
        defect: LinkDefect,
    },
}

/// The most bytes of a path an error carries.
const MAX_SHOWN: usize = 128;

/// `bytes` as text for an error: lossy, and cut.
pub(crate) fn show(bytes: &[u8]) -> String {
    let cut = bytes.len().min(MAX_SHOWN);
    String::from_utf8_lossy(&bytes[..cut]).into_owned()
}

/// Whether every byte of `text` is in `0x21..=0x7e`.
fn printable(text: &str) -> bool {
    text.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Checks an entry path and returns it as text.
///
/// A path is relative, at most 512 bytes and 64 components, with no empty component, no
/// `.` or `..`, no component over 255 bytes or starting with `._`, every byte in
/// `0x21..=0x7e` (so no space, control byte, `0x7f` or non-ASCII byte, which includes
/// every non-UTF-8 name), and none of the names APFS tools treat specially as its first
/// component.
///
/// # Errors
/// The first rule the path breaks.
pub fn check_path(path: &[u8]) -> Result<&str, NameDefect> {
    let text = std::str::from_utf8(path).map_err(|_| NameDefect::ForbiddenByte)?;
    if text.is_empty() {
        return Err(NameDefect::Empty);
    }
    if text.starts_with('/') {
        return Err(NameDefect::Absolute);
    }
    if text.len() > MAX_PATH_BYTES {
        return Err(NameDefect::TooLong);
    }
    if !printable(text) {
        return Err(NameDefect::ForbiddenByte);
    }
    if text.split('/').count() > MAX_DEPTH {
        return Err(NameDefect::TooDeep);
    }
    for part in text.split('/') {
        if part.is_empty() {
            return Err(NameDefect::EmptyComponent);
        }
        if matches!(part, "." | "..") {
            return Err(NameDefect::DotComponent);
        }
        if part.len() > MAX_COMPONENT_BYTES {
            return Err(NameDefect::ComponentTooLong);
        }
        if part.starts_with("._") {
            return Err(NameDefect::AppleDouble);
        }
    }
    let first = text.split('/').next().unwrap_or_default();
    if ROOT_RESERVED
        .iter()
        .any(|name| name.eq_ignore_ascii_case(first))
    {
        return Err(NameDefect::RootReserved);
    }
    Ok(text)
}

/// Checks the target of a symlink and returns it as text.
///
/// A target is relative, at most 1024 bytes, with every byte in `0x21..=0x7e` (so no NUL)
/// and no empty component or component over 255 bytes. `.` and `..` components are
/// allowed here; where they lead is the walker's question.
///
/// # Errors
/// The first rule the target breaks.
pub fn check_target(target: &[u8]) -> Result<&str, NameDefect> {
    let text = std::str::from_utf8(target).map_err(|_| NameDefect::ForbiddenByte)?;
    if text.is_empty() {
        return Err(NameDefect::Empty);
    }
    if text.len() > MAX_TARGET_BYTES {
        return Err(NameDefect::TooLong);
    }
    if text.starts_with('/') {
        return Err(NameDefect::Absolute);
    }
    if !printable(text) {
        return Err(NameDefect::ForbiddenByte);
    }
    for part in text.split('/') {
        if part.is_empty() {
            return Err(NameDefect::EmptyComponent);
        }
        if part.len() > MAX_COMPONENT_BYTES {
            return Err(NameDefect::ComponentTooLong);
        }
    }
    Ok(text)
}

/// What an entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symlink, with its checked target.
    Link(String),
}

/// The entries admitted so far, by path.
#[derive(Debug)]
pub(crate) struct Tree {
    nodes: BTreeMap<String, Node>,
    folded: BTreeSet<String>,
    /// Bytes counted so far: each path twice (it is kept folded too) and each target once.
    used: u64,
    cap: u64,
}

impl Tree {
    /// An empty tree whose model may hold `cap` bytes of paths (twice) and targets.
    pub(crate) fn new(cap: u64) -> Tree {
        Tree {
            nodes: BTreeMap::new(),
            folded: BTreeSet::new(),
            used: 0,
            cap,
        }
    }

    /// Adds the entry at `path` (already checked by [`check_path`]).
    ///
    /// # Errors
    /// The path is taken, differs from a taken one only in case, or has no earlier
    /// directory entry as its parent.
    pub(crate) fn add(&mut self, path: &str, node: Node) -> Result<(), TreeRefusal> {
        let refuse = |defect| TreeRefusal::Entry {
            path: path.to_owned(),
            defect,
        };
        if self.nodes.contains_key(path) {
            return Err(refuse(EntryDefect::Duplicate));
        }
        if !self.folded.insert(path.to_ascii_lowercase()) {
            return Err(refuse(EntryDefect::CaseCollision));
        }
        let target = match &node {
            Node::Link(target) => target.len(),
            Node::Dir | Node::File => 0,
        };
        self.used = self
            .used
            .saturating_add(2 * path.len() as u64 + target as u64);
        if self.used > self.cap {
            return Err(refuse(EntryDefect::ModelTooLarge));
        }
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| self.nodes.get(parent));
        match parent {
            None | Some(Some(Node::Dir)) => {}
            Some(None) => return Err(refuse(EntryDefect::ParentMissing)),
            Some(Some(_)) => return Err(refuse(EntryDefect::ParentNotDirectory)),
        }
        self.nodes.insert(path.to_owned(), node);
        Ok(())
    }

    /// Runs the walker over every symlink: each target is resolved the way the kernel
    /// does, against this model.
    ///
    /// # Errors
    /// The first symlink (in path order) that fails.
    pub(crate) fn check_links(&self) -> Result<(), TreeRefusal> {
        let links = self
            .nodes
            .values()
            .filter(|node| matches!(node, Node::Link(_)))
            .count() as u64;
        let mut budget = WALK_BASE_STEPS.saturating_add(links * WALK_STEPS_PER_LINK);
        for (path, node) in &self.nodes {
            if let Node::Link(target) = node {
                self.resolve(path, target, &mut budget)
                    .map_err(|defect| TreeRefusal::Link {
                        path: path.clone(),
                        defect,
                    })?;
            }
        }
        Ok(())
    }

    /// Resolves `target`, the target of the symlink at `at`, component by component.
    /// A symlink met on the way is followed (its target replaces it in the queue), `..`
    /// pops the directory resolved so far, and popping past the root is the escape.
    fn resolve<'a>(
        &'a self,
        at: &'a str,
        target: &'a str,
        budget: &mut u64,
    ) -> Result<(), LinkDefect> {
        let mut here: Vec<&str> = at.split('/').collect();
        here.pop().ok_or(LinkDefect::Escapes)?;
        let mut todo: VecDeque<&str> = target.split('/').collect();
        let mut hops = 0;
        while let Some(part) = todo.pop_front() {
            *budget = budget.checked_sub(1).ok_or(LinkDefect::OverBudget)?;
            match part {
                "." => {}
                ".." => {
                    here.pop().ok_or(LinkDefect::Escapes)?;
                }
                name => {
                    let path = here
                        .iter()
                        .copied()
                        .chain([name])
                        .collect::<Vec<_>>()
                        .join("/");
                    match self.nodes.get(&path) {
                        Some(Node::Dir) => here.push(name),
                        Some(Node::File) => {
                            return if todo.is_empty() {
                                Ok(())
                            } else {
                                Err(LinkDefect::NotDirectory)
                            };
                        }
                        Some(Node::Link(next)) => {
                            let _ = (&mut hops, next);
                            here.push(name);
                        }
                        None => {
                            let climbs = todo.iter().any(|rest| matches!(*rest, "." | ".."));
                            return if climbs {
                                Err(LinkDefect::Dangling)
                            } else {
                                Ok(())
                            };
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
