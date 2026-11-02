//! What a run wrote outside its project: a walk of the run's directories that adds up the
//! bytes of every entry whose XFS project is not the run's.
//!
//! The walk reads the tree through [`Tree`], so its rules are tested here over a scripted
//! tree with no file system, and in `quota.rs` over [`Disk`] on the XFS volume:
//!
//! - a regular file or a directory is opened without following a link and read for its
//!   project; a file system that keeps no project ids, or any failed read, ends the walk with
//!   an error naming the path and what failed, so nothing unreadable drops out of the total;
//! - any other entry (a link, a pipe, an overlay whiteout) is not opened, so a link is never
//!   followed, and it is counted in its directory's project: XFS gives a new entry the
//!   project of a directory marked to pass its project on, and refuses to link or move one in
//!   from another project;
//! - a tree on another device than the volume (a mount) holds nothing of the volume, and is
//!   neither read for its project nor entered.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use js_agent::quota::project_of;

/// What the walk is told of one entry, by `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The device the entry is on.
    pub dev: u64,
    /// What it is.
    pub kind: Kind,
    /// The bytes it holds on its device (`st_blocks` times 512).
    pub bytes: u64,
}

/// The kinds of entry the walk tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file: opened and read for its project.
    File,
    /// A directory: opened and read for its project, and entered.
    Dir,
    /// Anything else (a link, a pipe, a device, a socket): never opened.
    Other,
}

/// A tree the walk reads.
pub trait Tree {
    /// The entry at `path`, a link not followed.
    fn entry(&self, path: &Path) -> io::Result<Entry>;
    /// The project of the regular file or directory at `path`, opened without following a
    /// link; `None` where its file system keeps no project ids.
    fn project(&self, path: &Path) -> io::Result<Option<u32>>;
    /// The paths of the entries of the directory `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
}

/// Why the walk stopped: what it was doing, where, and the system's reason.
#[derive(Debug, PartialEq, Eq)]
pub struct WalkError {
    /// The entry the walk could not read.
    pub path: PathBuf,
    /// What it was doing.
    pub what: &'static str,
    /// Why it failed.
    pub why: String,
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.what, self.path.display(), self.why)
    }
}

/// Wraps a failed read of `path` with what was being done.
fn at(path: &Path, what: &'static str) -> impl FnOnce(io::Error) -> WalkError {
    move |why| WalkError {
        path: path.to_owned(),
        what,
        why: why.to_string(),
    }
}

/// The bytes on the device `dev` of `dirs`, and of everything below them, that are not in the
/// project `project`. A root that is not opened counts as in project 0.
///
/// # Errors
///
/// An entry could not be read, listed or read for its project, or its file system keeps no
/// project ids.
pub fn escaped(tree: &dyn Tree, dev: u64, dirs: &[&Path], project: u32) -> Result<u64, WalkError> {
    dirs.iter().try_fold(0, |sum, dir| {
        Ok(sum + escaped_below(tree, dir, dev, 0, project)?)
    })
}

/// [`escaped`] of `path`, which lies in a directory of the project `parent`.
fn escaped_below(
    tree: &dyn Tree,
    path: &Path,
    dev: u64,
    parent: u32,
    project: u32,
) -> Result<u64, WalkError> {
    let entry = tree.entry(path).map_err(at(path, "lstat"))?;
    if entry.dev != dev {
        return Ok(0);
    }
    let own = match entry.kind {
        Kind::Other => parent,
        Kind::File | Kind::Dir => tree
            .project(path)
            .map_err(at(path, "read the project of"))?
            .ok_or_else(|| WalkError {
                path: path.to_owned(),
                what: "read the project of",
                why: "its file system keeps no project ids".to_owned(),
            })?,
    };
    let here = match own == project {
        true => 0,
        false => entry.bytes,
    };
    if entry.kind != Kind::Dir {
        return Ok(here);
    }
    let entries = tree.list(path).map_err(at(path, "list"))?;
    entries.iter().try_fold(here, |sum, below| {
        Ok(sum + escaped_below(tree, below, dev, own, project)?)
    })
}

/// The tree on disk.
pub struct Disk;

impl Tree for Disk {
    fn entry(&self, path: &Path) -> io::Result<Entry> {
        let meta = fs::symlink_metadata(path)?;
        let kind = meta.file_type();
        let kind = match (kind.is_file(), kind.is_dir()) {
            (true, _) => Kind::File,
            (_, true) => Kind::Dir,
            _ => Kind::Other,
        };
        Ok(Entry {
            dev: meta.dev(),
            kind,
            bytes: meta.blocks() * 512,
        })
    }

    fn project(&self, path: &Path) -> io::Result<Option<u32>> {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let open = rustix::fs::open(path, flags, Mode::empty())?;
        project_of(&fs::File::from(open))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{Entry, Kind, Tree, WalkError, escaped};

    /// The volume's device in the scripted trees.
    const DEV: u64 = 7;
    /// The run's project.
    const RUN: u32 = 0x6A73_0016;
    /// Another run's project.
    const FOREIGN: u32 = 0x6A73_0017;

    /// One scripted entry: what each read of it answers.
    struct Node {
        entry: io::Result<Entry>,
        project: io::Result<Option<u32>>,
        list: io::Result<Vec<PathBuf>>,
    }

    /// A tree whose entries are scripted, and which records every read it answers.
    #[derive(Default)]
    struct Scripted {
        nodes: BTreeMap<PathBuf, Node>,
        reads: RefCell<Vec<String>>,
    }

    fn failed() -> io::Error {
        io::Error::from_raw_os_error(libc::EACCES)
    }

    impl Scripted {
        /// Adds `path` (below its parent's listing, if the parent is scripted).
        fn add(&mut self, path: &str, node: Node) -> &mut Self {
            let path = PathBuf::from(path);
            if let Some(Node { list: Ok(list), .. }) =
                path.parent().and_then(|p| self.nodes.get_mut(p))
            {
                list.push(path.clone());
            }
            self.nodes.insert(path, node);
            self
        }

        fn dir(&mut self, path: &str, dev: u64, project: u32, bytes: u64) -> &mut Self {
            self.add(path, node(dev, Kind::Dir, Ok(Some(project)), bytes))
        }

        fn file(&mut self, path: &str, project: u32, bytes: u64) -> &mut Self {
            self.add(path, node(DEV, Kind::File, Ok(Some(project)), bytes))
        }

        /// A link, a pipe or a whiteout: its project, if anyone read it, would be
        /// `FOREIGN`, which the walk must not ask for.
        fn other(&mut self, path: &str, bytes: u64) -> &mut Self {
            self.add(path, node(DEV, Kind::Other, Ok(Some(FOREIGN)), bytes))
        }

        fn read(&self, what: &str, path: &Path) -> io::Result<&Node> {
            self.reads
                .borrow_mut()
                .push(format!("{what} {}", path.display()));
            self.nodes
                .get(path)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
        }

        fn reads(&self) -> Vec<String> {
            self.reads.borrow().clone()
        }
    }

    fn node(dev: u64, kind: Kind, project: io::Result<Option<u32>>, bytes: u64) -> Node {
        Node {
            entry: Ok(Entry { dev, kind, bytes }),
            project,
            list: Ok(Vec::new()),
        }
    }

    fn copy<T: Clone>(answer: &io::Result<T>) -> io::Result<T> {
        match answer {
            Ok(value) => Ok(value.clone()),
            Err(why) => Err(io::Error::from_raw_os_error(
                why.raw_os_error().expect("a scripted errno"),
            )),
        }
    }

    impl Tree for Scripted {
        fn entry(&self, path: &Path) -> io::Result<Entry> {
            copy(&self.read("lstat", path)?.entry)
        }
        fn project(&self, path: &Path) -> io::Result<Option<u32>> {
            copy(&self.read("project", path)?.project)
        }
        fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
            copy(&self.read("list", dir)?.list)
        }
    }

    fn walk(tree: &Scripted, dirs: &[&str]) -> Result<u64, WalkError> {
        let dirs: Vec<&Path> = dirs.iter().map(Path::new).collect();
        escaped(tree, DEV, &dirs, RUN)
    }

    /// Catches a walk that counts a correctly bound run: two roots and everything below them
    /// in the run's project hold no escaped byte.
    #[test]
    fn a_run_wholly_in_its_project_has_nothing_escaped() {
        let mut tree = Scripted::default();
        tree.dir("/run", DEV, RUN, 4096)
            .file("/run/fill", RUN, 8 << 20)
            .dir("/lease", DEV, RUN, 4096)
            .dir("/lease/upper", DEV, RUN, 4096)
            .file("/lease/upper/fill", RUN, 2 << 20);
        assert_eq!(walk(&tree, &["/run", "/lease"]), Ok(0));
    }

    /// Catches a walk that reads only its roots' projects and assumes everything below shares
    /// them: below a bound root, a directory of another (nonzero) project and its file, and a
    /// file of project 0, are counted to the byte, and their siblings in the run's project are
    /// not.
    #[test]
    fn a_descendant_of_another_project_below_a_bound_root_is_counted() {
        let mut tree = Scripted::default();
        tree.dir("/run", DEV, RUN, 4096)
            .file("/run/mine", RUN, 1 << 20)
            .dir("/run/theirs", DEV, FOREIGN, 4096)
            .file("/run/theirs/fill", FOREIGN, 3 << 20)
            .file("/run/theirs/back", RUN, 1 << 20)
            .file("/run/none", 0, 2 << 20);
        assert_eq!(walk(&tree, &["/run"]), Ok(4096 + (3 << 20) + (2 << 20)));
    }

    /// Catches a walk that skips a root that is not bound: everything in a root of project 0
    /// is counted, the root's own blocks too.
    #[test]
    fn everything_in_an_unbound_root_is_counted() {
        let mut tree = Scripted::default();
        tree.dir("/run", DEV, 0, 4096)
            .file("/run/fill", 0, 2 << 20)
            .other("/run/link", 4096);
        assert_eq!(walk(&tree, &["/run"]), Ok(4096 + (2 << 20) + 4096));
    }

    /// Catches a walk that opens a link, a pipe or a whiteout (it would follow the link or
    /// block on the pipe): such an entry is never read for its project and is counted in its
    /// directory's project, so with its blocks it is not counted in a bound directory and is
    /// counted in a foreign one.
    #[test]
    fn an_entry_that_is_not_opened_takes_its_directorys_project() {
        let mut tree = Scripted::default();
        tree.dir("/run", DEV, RUN, 0)
            .other("/run/link", 4096)
            .other("/run/pipe", 0)
            .dir("/run/theirs", DEV, FOREIGN, 0)
            .other("/run/theirs/link", 4096);
        assert_eq!(walk(&tree, &["/run"]), Ok(4096));
        let opened: Vec<String> = tree
            .reads()
            .into_iter()
            .filter(|read| read.starts_with("project"))
            .collect();
        assert_eq!(opened, ["project /run", "project /run/theirs"]);
    }

    /// Catches a walk that crosses onto another device: a tree mounted below a run holds
    /// nothing of the volume, is not counted, and is neither read for its project nor listed.
    #[test]
    fn a_tree_on_another_device_is_not_counted_or_entered() {
        let mut tree = Scripted::default();
        tree.dir("/run", DEV, RUN, 0)
            .dir("/run/mnt", DEV + 1, 0, 4096)
            .file("/run/mnt/fill", 0, 1 << 20);
        assert_eq!(walk(&tree, &["/run"]), Ok(0));
        assert_eq!(
            tree.reads(),
            ["lstat /run", "project /run", "list /run", "lstat /run/mnt"]
        );
    }

    /// Catches a walk that drops what it cannot read from the total: a failed `lstat`, a failed
    /// read of a project, a file system with no project ids, and a failed listing each end
    /// the walk with the path and what failed.
    #[test]
    fn an_entry_that_cannot_be_read_ends_the_walk_with_its_path() {
        let eacces = failed().to_string();
        let cases: [(Node, &str, String); 4] = [
            (
                Node {
                    entry: Err(failed()),
                    ..node(DEV, Kind::File, Ok(Some(RUN)), 0)
                },
                "lstat",
                eacces.clone(),
            ),
            (
                node(DEV, Kind::File, Err(failed()), 0),
                "read the project of",
                eacces.clone(),
            ),
            (
                node(DEV, Kind::File, Ok(None), 0),
                "read the project of",
                "its file system keeps no project ids".to_owned(),
            ),
            (
                Node {
                    list: Err(failed()),
                    ..node(DEV, Kind::Dir, Ok(Some(RUN)), 0)
                },
                "list",
                eacces,
            ),
        ];
        for (bad, what, why) in cases {
            let mut tree = Scripted::default();
            tree.dir("/run", DEV, RUN, 0).add("/run/bad", bad);
            let error = walk(&tree, &["/run"]).expect_err(what);
            assert_eq!(
                error.to_string(),
                format!("{what} /run/bad: {why}"),
                "{what}"
            );
        }
        let tree = Scripted::default();
        assert_eq!(
            walk(&tree, &["/gone"]).map_err(|e| e.to_string()),
            Err(format!(
                "lstat /gone: {}",
                io::Error::from_raw_os_error(libc::ENOENT)
            ))
        );
    }
}
