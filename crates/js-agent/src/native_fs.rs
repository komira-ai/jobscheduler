//! The rules of a native (macOS) job's mounts that need no Mac to decide, built and tested on
//! every platform: the system calls they rest on (`statfs`, the directory service) are passed
//! in by the native runtime (`native/mounts.rs`) and the macOS platform (`platform/macos.rs`).
//! The design is that of komira-ai/jobscheduler#958, its step 1.
//!
//! - **A provider's tree is read-only** ([`check_provider_mode`]). The provider writes it
//!   whole on the Mac's disk and every job that names the same tree is given the same
//!   directory, so a job may not write it.
//! - **Every mount comes from a local volume** ([`local_roots`] at the agent's start,
//!   [`local_source`] for each mount once its links are followed). macOS refuses Apple's
//!   programs every read of a network volume, so a tree there would fail inside each job
//!   instead of once, by name (the design's section 2.1).
//! - **A read-write mount's missing directories are made** ([`Missing`]). A job's put
//!   directory, below a `--mount-root`, need not exist before its start: the agent makes the
//!   directories that are not there, those above the mount's own with the root's owner, group
//!   and mode, and the mount's own the run user's, of the root's group, mode `0770`, so the
//!   root's owner can take and remove what the job leaves there. The agent never removes
//!   them.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use js_agent_wire::{JobMount, MountMode, MountSource, refusal};

use crate::lifecycle::{MountRoots, NativeRefusal};

/// The mode of the directory the agent makes for a read-write mount: its user's and its
/// root's group's, to read, write and search.
pub const MOUNT_DIR_MODE: u32 = 0o770;

/// What the native runtime needs to know of the volume a path is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The file system's type, as `statfs` names it (`apfs`, `nfs`, `smbfs`, ...).
    pub fs_type: String,
    /// The volume is local (`statfs`'s `MNT_LOCAL`).
    pub local: bool,
}

/// Refuses a mount of a provider's tree whose mode is not read-only, `input-not-allowed`. A
/// mount of a host path is not this rule's.
///
/// # Errors
///
/// The refusal names the mount's target.
pub fn check_provider_mode(mount: &JobMount) -> Result<(), NativeRefusal> {
    match (&mount.source, mount.mode) {
        (MountSource::Provider { .. }, MountMode::ReadWrite | MountMode::CopyOnWrite) => {
            Err(NativeRefusal {
                code: refusal::INPUT_NOT_ALLOWED,
                why: format!(
                    "{}: a provider's tree is shared by every native job that names it, so a \
                     native job mounts it read-only only",
                    mount.target
                ),
            })
        }
        _ => Ok(()),
    }
}

/// Checks that every root of `roots` is on a local volume, as `volume_of` reads it.
///
/// # Errors
///
/// The text names the flag (`--mount-root` or `--mount-root-ro`), the root's name and path,
/// and the volume's type, or what could not be read.
pub fn local_roots(
    roots: &MountRoots,
    volume_of: &dyn Fn(&Path) -> io::Result<Volume>,
) -> Result<(), String> {
    let writable: Vec<&Path> = roots.writable_paths().collect();
    for (name, path) in roots.named() {
        let flag = match writable.contains(&path) {
            true => "--mount-root",
            false => "--mount-root-ro",
        };
        let volume =
            volume_of(path).map_err(|why| format!("{flag} {name}: {}: {why}", path.display()))?;
        if !volume.local {
            return Err(format!(
                "{flag} {name}: {} is on a volume of type {} that is not local: a native job \
                 reads local volumes only",
                path.display(),
                volume.fs_type
            ));
        }
    }
    Ok(())
}

/// Refuses a mount whose canonical directory `host` is not on a local volume
/// (`input-not-allowed`), or whose volume cannot be read (`input-unavailable`).
///
/// # Errors
///
/// The refusal names the path, and the volume's type or the error.
pub fn local_source(
    host: &Path,
    volume_of: &dyn Fn(&Path) -> io::Result<Volume>,
) -> Result<(), NativeRefusal> {
    match volume_of(host) {
        Ok(volume) if volume.local => Ok(()),
        Ok(volume) => Err(NativeRefusal {
            code: refusal::INPUT_NOT_ALLOWED,
            why: format!(
                "{} is on a volume of type {} that is not local: a native job reads local \
                 volumes only",
                host.display(),
                volume.fs_type
            ),
        }),
        Err(why) => Err(NativeRefusal {
            code: refusal::INPUT_UNAVAILABLE,
            why: format!("{}: {why}", host.display()),
        }),
    }
}

/// The directories of a read-write mount's path that are not there yet: the deepest of its
/// directories that is, canonical, and the names below it to make, the last the mount's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    base: PathBuf,
    names: Vec<OsString>,
}

impl Missing {
    /// The missing part of `wanted`, an absolute path in normal form that is not there.
    ///
    /// # Errors
    ///
    /// `wanted` is not absolute or not in normal form, it is there (a link that leads
    /// nowhere), a part of it is a file or a link that leads nowhere, or a part of it cannot be
    /// looked at.
    pub fn find(wanted: &Path) -> io::Result<Self> {
        let normal = wanted.is_absolute()
            && wanted
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        if !normal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not an absolute path in normal form",
            ));
        }
        let mut base = wanted.to_path_buf();
        let mut names = Vec::new();
        loop {
            match fs::symlink_metadata(&base) {
                Ok(_) => break,
                Err(why) if why.kind() == io::ErrorKind::NotFound => {
                    // `/` is always there, so a path that is not has a last part.
                    names.extend(base.file_name().map(ToOwned::to_owned));
                    base.pop();
                }
                Err(why) => return Err(why),
            }
        }
        if names.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "a link that leads nowhere",
            ));
        }
        names.reverse();
        // Something below it was looked for and not found, so it is a directory.
        let base = fs::canonicalize(&base)?;
        Ok(Self { base, names })
    }

    /// The deepest of the directories that is there, canonical.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The path the mount's directory will have: [`Missing::base`] and the names below it.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.names
            .iter()
            .fold(self.base.clone(), |path, name| path.join(name))
    }

    /// Makes each missing directory below `root` (the mount's root, which the base lies
    /// beneath): those above the last given to the root's owner and group with the root's
    /// mode, the last to `uid` and the root's group with [`MOUNT_DIR_MODE`]; `chown` gives a
    /// directory away (`lchown`, which needs root). The base is looked up again first and must
    /// still be the canonical directory [`Missing::find`] found, beneath `root`, so a link put
    /// in its place since is not followed; each mode is set on the directory opened without
    /// following a link. A directory above the last that another start made meanwhile is
    /// taken as it is, and so is the last when it is `uid`'s; a name that is there as anything
    /// but a directory fails.
    ///
    /// # Errors
    ///
    /// The root cannot be looked at, the base changed, the last directory is there and is not
    /// `uid`'s, or a directory cannot be made or given away; the text names the path.
    pub fn make(
        &self,
        root: &Path,
        uid: u32,
        chown: &dyn Fn(&Path, u32, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        let named = |path: &Path, why: io::Error| {
            io::Error::new(why.kind(), format!("{}: {why}", path.display()))
        };
        let of_root = fs::metadata(root).map_err(|why| named(root, why))?;
        let base = fs::canonicalize(&self.base).map_err(|why| named(&self.base, why))?;
        if base != self.base || !base.starts_with(root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is no longer the directory below {} that was looked up",
                    self.base.display(),
                    root.display()
                ),
            ));
        }
        let group = of_root.gid();
        let mut path = self.base.clone();
        for (at, name) in self.names.iter().enumerate() {
            path.push(name);
            let (owner, mode) = match at + 1 == self.names.len() {
                true => (uid, MOUNT_DIR_MODE),
                false => (of_root.uid(), of_root.mode() & 0o7777),
            };
            match fs::create_dir(&path) {
                Ok(()) => {
                    give(&path, (owner, group, mode), chown).map_err(|why| named(&path, why))?
                }
                Err(why) if why.kind() == io::ErrorKind::AlreadyExists => {
                    let there = fs::symlink_metadata(&path).map_err(|why| named(&path, why))?;
                    if !there.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("{} is there and is not a directory", path.display()),
                        ));
                    }
                    if at + 1 == self.names.len() && there.uid() != uid {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "{} is there and is uid {}'s, not the run's ({uid})",
                                path.display(),
                                there.uid()
                            ),
                        ));
                    }
                }
                Err(why) => return Err(named(&path, why)),
            }
        }
        Ok(())
    }
}

/// Gives the directory `path` that was just made to `owner` and `group` through `chown`, then
/// sets its `mode` on the directory opened without following a link.
fn give(
    path: &Path,
    (owner, group, mode): (u32, u32, u32),
    chown: &dyn Fn(&Path, u32, u32) -> io::Result<()>,
) -> io::Result<()> {
    chown(path, owner, group)?;
    let dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)?;
    dir.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    use js_agent_wire::Propagation;
    use js_testkit::dir::TestDir;

    use super::*;

    fn mount(source: MountSource, mode: MountMode) -> JobMount {
        JobMount {
            source,
            target: "/work/.kbf-tree".to_owned(),
            mode,
            sub_path: None,
            no_exec: true,
            propagation: Propagation::Private,
        }
    }

    fn provider() -> MountSource {
        MountSource::Provider {
            provider: "tierstore".to_owned(),
            reference: "cas/l/00".to_owned(),
        }
    }

    fn volume(fs_type: &str, local: bool) -> Volume {
        Volume {
            fs_type: fs_type.to_owned(),
            local,
        }
    }

    /// Asserts that a provider's mount is admitted read-only and refused read-write and
    /// copy-on-write, `input-not-allowed` with the exact detail, and that a host path's mount
    /// is not this rule's in any mode. Catches a job that writes a tree every job of the Mac
    /// that names it shares.
    #[test]
    fn a_providers_tree_is_admitted_read_only_only() {
        assert_eq!(
            check_provider_mode(&mount(provider(), MountMode::ReadOnly)),
            Ok(())
        );
        for mode in [MountMode::ReadWrite, MountMode::CopyOnWrite] {
            assert_eq!(
                check_provider_mode(&mount(provider(), mode)),
                Err(NativeRefusal {
                    code: refusal::INPUT_NOT_ALLOWED,
                    why: "/work/.kbf-tree: a provider's tree is shared by every native job \
                          that names it, so a native job mounts it read-only only"
                        .to_owned(),
                }),
                "{mode:?}"
            );
        }
        let host = MountSource::HostPath {
            path: "/srv/out".to_owned(),
        };
        for mode in [
            MountMode::ReadOnly,
            MountMode::ReadWrite,
            MountMode::CopyOnWrite,
        ] {
            assert_eq!(check_provider_mode(&mount(host.clone(), mode)), Ok(()));
        }
    }

    /// Asserts that a mount's directory on a local volume is taken, one on a volume that is
    /// not local is refused `input-not-allowed` naming the path and the volume's type, and one
    /// whose volume cannot be read is refused `input-unavailable` with the error, each by
    /// exact equality. Catches a tree on a network volume that every program of the job would
    /// fail to read, and an error taken for a local volume.
    #[test]
    fn a_mounts_directory_must_be_on_a_local_volume() {
        let host = Path::new("/usr/local/tierstore/job-trees/ab");
        let local = |_: &Path| Ok(volume("apfs", true));
        assert_eq!(local_source(host, &local), Ok(()));
        let network = |_: &Path| Ok(volume("nfs", false));
        assert_eq!(
            local_source(host, &network),
            Err(NativeRefusal {
                code: refusal::INPUT_NOT_ALLOWED,
                why: "/usr/local/tierstore/job-trees/ab is on a volume of type nfs that is not \
                      local: a native job reads local volumes only"
                    .to_owned(),
            })
        );
        let unread = |_: &Path| Err(io::Error::other("no statfs"));
        assert_eq!(
            local_source(host, &unread),
            Err(NativeRefusal {
                code: refusal::INPUT_UNAVAILABLE,
                why: "/usr/local/tierstore/job-trees/ab: no statfs".to_owned(),
            })
        );
    }

    /// Asserts that the agent's start takes roots on local volumes, and refuses a root of
    /// either flag on a volume that is not local, and one whose volume cannot be read, naming
    /// the flag, the root and the volume by exact equality; and that each root is asked about
    /// by its canonical path. Catches a configuration no job could use that is only found as
    /// a failure inside each job.
    #[test]
    fn the_agents_mount_roots_must_be_on_local_volumes() {
        let dir = TestDir::under(std::env::temp_dir(), "native-fs-roots");
        let (trees, put) = (dir.path().join("trees"), dir.path().join("put"));
        for path in [&trees, &put] {
            fs::create_dir(path).unwrap();
        }
        let roots = MountRoots::with_read_only(
            &[("put".to_owned(), put.clone())],
            &[("trees".to_owned(), trees.clone())],
        )
        .unwrap();
        let (trees, put) = (
            fs::canonicalize(trees).unwrap(),
            fs::canonicalize(put).unwrap(),
        );
        let asked = RefCell::new(Vec::new());
        let local = |path: &Path| {
            asked.borrow_mut().push(path.to_path_buf());
            Ok(volume("apfs", true))
        };
        assert_eq!(local_roots(&roots, &local), Ok(()));
        assert_eq!(*asked.borrow(), [put.clone(), trees.clone()]);
        assert_eq!(local_roots(&MountRoots::default(), &local), Ok(()));

        let not_local = |wanted: &Path| {
            let found = |path: &Path| Ok(volume("smbfs", path != wanted));
            local_roots(&roots, &found)
        };
        assert_eq!(
            not_local(&put),
            Err(format!(
                "--mount-root put: {} is on a volume of type smbfs that is not local: a native \
                 job reads local volumes only",
                put.display()
            ))
        );
        assert_eq!(
            not_local(&trees),
            Err(format!(
                "--mount-root-ro trees: {} is on a volume of type smbfs that is not local: a \
                 native job reads local volumes only",
                trees.display()
            ))
        );
        let unread = |_: &Path| Err(io::Error::other("no statfs"));
        assert_eq!(
            local_roots(&roots, &unread),
            Err(format!("--mount-root put: {}: no statfs", put.display()))
        );
    }

    /// Asserts that the missing part of a path is found below its deepest directory that is
    /// there, canonical (through a link), with the names to make in order; and that a path
    /// that is not absolute or in normal form, a link that leads nowhere (at the end or on
    /// the way), and a deepest part that is a file are refused. Catches a directory made
    /// through a link or beside a file.
    #[test]
    fn the_missing_part_of_a_put_directory_is_found_below_what_is_there() {
        let dir = TestDir::under(std::env::temp_dir(), "native-fs-missing");
        let root = dir.path().join("put");
        fs::create_dir(&root).unwrap();
        symlink(&root, dir.path().join("via")).unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        let found = Missing::find(&dir.path().join("via/cas/b/build/nonce")).unwrap();
        assert_eq!(found.base(), canonical);
        assert_eq!(found.path(), canonical.join("cas/b/build/nonce"));
        assert_eq!(
            found.names,
            ["cas", "b", "build", "nonce"].map(OsString::from)
        );

        for bad in ["relative/x", "/a/../b"] {
            let why = Missing::find(Path::new(bad)).unwrap_err();
            assert_eq!(why.kind(), io::ErrorKind::InvalidInput, "{bad}");
            assert_eq!(why.to_string(), "not an absolute path in normal form");
        }
        symlink(dir.path().join("nowhere"), root.join("dangling")).unwrap();
        let why = Missing::find(&root.join("dangling")).unwrap_err();
        assert_eq!(why.to_string(), "a link that leads nowhere");
        let through = Missing::find(&root.join("dangling/nonce")).unwrap_err();
        assert_eq!(through.kind(), io::ErrorKind::NotFound);
        fs::write(root.join("file"), "").unwrap();
        for below_a_file in ["file/nonce", "file/deeper/nonce"] {
            let why = Missing::find(&root.join(below_a_file)).unwrap_err();
            assert_eq!(why.kind(), io::ErrorKind::NotADirectory, "{below_a_file}");
        }
    }

    /// Asserts that the missing directories are made: each above the last given to the root's
    /// owner and group with the root's mode, the last to the run user and the root's group,
    /// mode 0770; that a last directory another start made meanwhile is kept as it is and not
    /// given away again when it is the run's, and refused when it is another uid's; that a
    /// base replaced by a link since the lookup, or not beneath the root, makes nothing; and
    /// that a name there as a file or a link fails, and a directory that cannot be given away
    /// fails, naming the path. Catches a put directory its job cannot
    /// write, one the root's owner cannot empty, and a directory made through a link.
    #[test]
    fn the_missing_directories_are_made_with_the_roots_owner_and_the_users_last() {
        let dir = TestDir::under(std::env::temp_dir(), "native-fs-make");
        let root = dir.path().join("put");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o751)).unwrap();
        let of_root = fs::metadata(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let given = RefCell::new(Vec::new());
        let chown = |path: &Path, uid: u32, gid: u32| {
            given.borrow_mut().push((path.to_path_buf(), uid, gid));
            Ok(())
        };
        let wanted = root.join("cas/b/build/nonce");
        Missing::find(&wanted)
            .unwrap()
            .make(&root, 501, &chown)
            .unwrap();
        let group = of_root.gid();
        assert_eq!(
            *given.borrow(),
            [
                (root.join("cas"), of_root.uid(), group),
                (root.join("cas/b"), of_root.uid(), group),
                (root.join("cas/b/build"), of_root.uid(), group),
                (wanted.clone(), 501, group),
            ]
        );
        let mode = |path: &Path| fs::metadata(path).unwrap().mode() & 0o7777;
        for above in ["cas", "cas/b", "cas/b/build"] {
            assert_eq!(mode(&root.join(above)), 0o751, "{above}");
        }
        assert_eq!(mode(&wanted), MOUNT_DIR_MODE);

        // A start of the same build that another start overtook: the directory is the run's,
        // so it is taken and not given away again; one of another uid is refused.
        given.borrow_mut().clear();
        let other = root.join("cas/b/build/other");
        let missing = Missing::find(&other).unwrap();
        fs::create_dir(&other).unwrap();
        let mine = fs::metadata(&other).unwrap().uid();
        missing.make(&root, mine, &chown).unwrap();
        assert!(given.borrow().is_empty(), "{:?}", given.borrow());
        let why = missing.make(&root, mine + 1, &chown).unwrap_err();
        assert_eq!(
            why.to_string(),
            format!(
                "{} is there and is uid {mine}'s, not the run's ({})",
                other.display(),
                mine + 1
            )
        );

        // The base was replaced by a link since the lookup, or lies beneath another root:
        // nothing is made.
        let swapped = Missing::find(&root.join("cas/b/swap/nonce")).unwrap();
        let moved = Missing::find(&root.join("cas/b/gone/nonce")).unwrap();
        let base = root.join("cas/b");
        fs::rename(&base, root.join("cas/b-old")).unwrap();
        symlink(root.join("cas/b-old"), &base).unwrap();
        let changed = |missing: &Missing, under: &Path| {
            let why = missing.make(under, 501, &chown).unwrap_err();
            assert_eq!(
                why.to_string(),
                format!(
                    "{} is no longer the directory below {} that was looked up",
                    base.display(),
                    under.display()
                )
            );
        };
        changed(&swapped, &root);
        fs::remove_file(&base).unwrap();
        fs::rename(root.join("cas/b-old"), &base).unwrap();
        changed(&moved, &root.join("cas/b/build"));
        assert!(!base.join("swap").exists());
        assert!(!base.join("gone").exists());

        // A directory above the last that another start made since the lookup is taken.
        let raced = Missing::find(&root.join("cas/b/raced/nonce")).unwrap();
        fs::create_dir(root.join("cas/b/raced")).unwrap();
        raced.make(&root, 501, &chown).unwrap();
        assert_eq!(mode(&root.join("cas/b/raced/nonce")), MOUNT_DIR_MODE);

        fs::write(root.join("cas/b/file"), "").unwrap();
        let as_file = Missing::find(&root.join("cas/b/file2/nonce")).unwrap();
        fs::rename(root.join("cas/b/file"), root.join("cas/b/file2")).unwrap();
        let why = as_file.make(&root, 501, &chown).unwrap_err();
        assert_eq!(
            why.to_string(),
            format!(
                "{} is there and is not a directory",
                root.join("cas/b/file2").display()
            )
        );
        let as_link = Missing::find(&root.join("cas/b/link/nonce")).unwrap();
        symlink(dir.path(), root.join("cas/b/link")).unwrap();
        let why = as_link.make(&root, 501, &chown).unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let refused = |_: &Path, _: u32, _: u32| Err(io::Error::other("not root"));
        let why = Missing::find(&root.join("cas/b/build/third"))
            .unwrap()
            .make(&root, 501, &refused)
            .unwrap_err();
        assert_eq!(
            why.to_string(),
            format!("{}: not root", root.join("cas/b/build/third").display())
        );
    }

    /// Asserts that a root that cannot be looked at, a name the volume cannot hold, and a base
    /// removed since the lookup fail the making, naming the path. Catches an error passed over as a directory made.
    #[test]
    fn a_directory_that_cannot_be_made_names_its_path() {
        let dir = TestDir::under(std::env::temp_dir(), "native-fs-unmade");
        let root = dir.path().join("put");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let chown = |_: &Path, _: u32, _: u32| Ok(());
        let missing = Missing::find(&root.join("nonce")).unwrap();
        missing.make(&root, 501, &chown).unwrap();
        fs::remove_dir(root.join("nonce")).unwrap();
        let gone = dir.path().join("gone");
        let why = missing.make(&gone, 501, &chown).unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::NotFound);
        assert!(
            why.to_string()
                .starts_with(&format!("{}: ", gone.display())),
            "{why}"
        );
        // A name the volume cannot hold: the directory cannot be made.
        let long = Missing {
            base: root.clone(),
            names: vec![OsString::from("n".repeat(300))],
        };
        let why = long.make(&root, 501, &chown).unwrap_err();
        assert!(
            why.to_string()
                .starts_with(&format!("{}: ", long.path().display())),
            "{why}"
        );
        // The base was removed after the lookup: nothing is made, and the error names it.
        let later = Missing::find(&root.join("a/b")).unwrap();
        fs::remove_dir(&root).unwrap();
        fs::create_dir(dir.path().join("elsewhere-root")).unwrap();
        let why = later
            .make(&dir.path().join("elsewhere-root"), 501, &chown)
            .unwrap_err();
        assert_eq!(
            why.to_string(),
            format!("{}: No such file or directory (os error 2)", root.display())
        );
    }
}
