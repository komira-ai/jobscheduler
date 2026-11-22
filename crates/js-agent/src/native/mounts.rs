//! The mounts of a job-keyed start on a Mac (`docs/design/job-rootfs.md`, sections 5.1, 5.2
//! and 5.4): a `READ_ONLY` or `READ_WRITE` mount of a host path at `/work/<rest>` is a
//! symbolic link `exec/<rest>` to the canonical host path, made by the agent before the
//! run's first process, and a grant in the run's profile ([`MountGrant`]).
//!
//! The lifecycle asks the runtime three things, in this order:
//!
//! 1. [`check`], at admission and with no I/O: what a Mac cannot give is refused with its
//!    own code, before anything is made. Layers (`rootfs-unsupported-here`: a native job has
//!    no root for them to replace), a copy-on-write mount (`rootfs-unsupported-here`: its
//!    mechanism is not chosen, section 5.3), a host port whose job port differs from its host
//!    port (`host-port-remap-unsupported-here`: a native job shares the host's loopback), a
//!    mount target that is `/work` itself, outside `/work`, or below another mount's target
//!    (`mount-target-unsupported-here`: a link cannot replace the working directory, and a
//!    link made below another would be made inside the host's directory), a provider's tree
//!    mounted other than read-only (`input-not-allowed`: every job that names the tree is given
//!    the one directory the provider wrote, [`crate::native_fs`]), and more mounts than the
//!    profile lists (`too-many-inputs-here`). A host port whose two numbers are equal is
//!    not refused here: it falls through to the lifecycle's host-port admission, which refuses
//!    it `input-not-allowed` because a native runtime offers no host port.
//! 2. [`resolve`], when the start waits for its file system: each host path (with its
//!    `sub_path`) is resolved with `canonicalize`, which is a metadata lookup and is answered
//!    at once (`Ready`), and its canonical path must be a directory (`input-unavailable`
//!    when it is missing or is not one) beneath a root of the agent that the mount's mode
//!    allows (`input-not-allowed`), on a local volume (`input-not-allowed`, `statfs`'s
//!    `MNT_LOCAL`). A read-write mount's directory that is missing is taken when the deepest
//!    part of it that is there lies beneath a writable root: the rest is made at the start
//!    ([`crate::native_fs::Missing`]). The lifecycle's own check of the same rule reads the
//!    text of the path only, so a link inside a root that leads out is caught here. The
//!    plans are kept by run id until [`NativeRuntime::create_and_start`] takes them, or
//!    [`discard`] drops them.
//! 3. [`link_all`], in the start, after `exec/` is made and before the working directory is
//!    opened: a read-write mount's missing directories are made (the last the run user's),
//!    the parent directories of each link are made as the run's user's, the link is made and
//!    given to the user. A name that is there already fails the start.
//!
//! A provider's tree (`--mount-provider`) reaches this runtime as a host path: the lifecycle
//! asks the provider for it and puts the directory the provider answered in the mount's
//! source before it asks [`resolve`] again, so the directory is held to the same rules.
//!
//! [`NativeRuntime::create_and_start`]: super::NativeRuntime

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};

use js_agent_wire::{HostPort, JobMount, MountMode, MountSource, refusal};
use js_mac_pool::seatbelt::{AbsPath, MAX_LISTED, MountGrant};

use super::files::{HandOver, MakeDir, make_parents};
use crate::lifecycle::{FsPrepared, JobFs, MountRoots, NativeRefusal};
use crate::native_fs::{self, Missing, Volume};

/// `statfs`'s flag of a local volume (`MNT_LOCAL` of `<sys/mount.h>`).
const MNT_LOCAL: u32 = 0x0000_1000;

/// The volume `path` is on, as `statfs` reads it: its type and whether it is local.
///
/// # Errors
///
/// `statfs` failed.
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    let found = rustix::fs::statfs(path)?;
    let name: Vec<u8> = found
        .f_fstypename
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| byte.to_ne_bytes()[0])
        .collect();
    Ok(Volume {
        fs_type: String::from_utf8_lossy(&name).into_owned(),
        local: found.f_flags & MNT_LOCAL != 0,
    })
}

/// The directory a mount's target lies below: the run's working directory, `exec/`.
const WORK: &str = "/work/";

/// A root of the agent that jobs may mount from (`--mount-root`, `--mount-root-ro`), by
/// canonical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRoot {
    /// The canonical path of the root.
    pub path: PathBuf,
    /// Jobs may mount from it read-only only (`--mount-root-ro`).
    pub read_only: bool,
}

/// The roots of `roots` as the native runtime holds them, by path: one entry per canonical
/// path, writable when any root of that path is.
#[must_use]
pub fn roots_of(roots: &MountRoots) -> Vec<MountRoot> {
    let writable: BTreeSet<&Path> = roots.writable_paths().collect();
    let all: BTreeSet<&Path> = roots.paths().collect();
    all.into_iter()
        .map(|path| MountRoot {
            path: path.to_path_buf(),
            read_only: !writable.contains(path),
        })
        .collect()
}

/// One mount of a run, resolved: what [`link_all`] makes and what the profile grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MountPlan {
    /// The link's path below `exec/`: the target without its `/work/`.
    pub(super) rest: PathBuf,
    /// The canonical host directory the link points at.
    pub(super) host: AbsPath,
    /// The job may write below it (a read-write mount).
    pub(super) write: bool,
    /// Nothing below it may be run or mapped executable.
    pub(super) no_exec: bool,
    /// The directories of a read-write mount that are not there yet, and the root they are
    /// made below: [`link_all`] makes them.
    pub(super) missing: Option<(Missing, PathBuf)>,
}

/// The plans of the runs whose file system was resolved and whose start has not taken them,
/// by run id.
pub(super) type Plans = BTreeMap<String, Vec<MountPlan>>;

fn refuse(code: &'static str, why: String) -> NativeRefusal {
    NativeRefusal { code, why }
}

/// What a Mac cannot give a job start (see the module documentation), in the order: layers,
/// a remapped host port, then each mount (copy-on-write, then target, then a provider's tree
/// other than read-only), nested targets, and the count.
pub(super) fn check(fs: &JobFs, host_ports: &[HostPort]) -> Result<(), NativeRefusal> {
    if !fs.layers.is_empty() {
        return Err(refuse(
            refusal::ROOTFS_UNSUPPORTED_HERE,
            "a native job has no root for layers to replace".to_owned(),
        ));
    }
    if let Some(port) = host_ports
        .iter()
        .find(|port| port.container_port.is_some_and(|job| job != port.host_port))
    {
        return Err(refuse(
            refusal::HOST_PORT_REMAP_UNSUPPORTED_HERE,
            format!(
                "host port {} is asked at job port {}: a native job shares the host's loopback \
                 and reaches a host port only at its own number",
                port.host_port,
                port.job_port()
            ),
        ));
    }
    for mount in &fs.mounts {
        if mount.mode == MountMode::CopyOnWrite {
            return Err(refuse(
                refusal::ROOTFS_UNSUPPORTED_HERE,
                format!(
                    "{}: a copy-on-write mount needs a mechanism this agent does not have",
                    mount.target
                ),
            ));
        }
        plan_target(&mount.target)?;
        native_fs::check_provider_mode(mount)?;
    }
    nested(&fs.mounts)?;
    if fs.mounts.len() > MAX_LISTED {
        return Err(refuse(
            refusal::TOO_MANY_INPUTS_HERE,
            format!(
                "{} mounts: a native job's profile lists at most {MAX_LISTED}",
                fs.mounts.len()
            ),
        ));
    }
    Ok(())
}

/// The link of a mount at `target`, relative to `exec/`: `target` must lie strictly below
/// `/work`, in normal form.
fn plan_target(target: &str) -> Result<PathBuf, NativeRefusal> {
    let rest = target
        .strip_prefix(WORK)
        .filter(|rest| !rest.split('/').any(|part| matches!(part, "" | "." | "..")));
    rest.map(PathBuf::from).ok_or_else(|| {
        refuse(
            refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
            format!(
                "the mount target {target} is not below /work: a native job's mounts are links in \
                 its working directory"
            ),
        )
    })
}

/// Refuses a target below another mount's target, compared in lower case (the volume ignores
/// case): the link of the inner one would be made inside the host directory the outer one
/// points at.
fn nested(mounts: &[JobMount]) -> Result<(), NativeRefusal> {
    let targets: Vec<String> = mounts
        .iter()
        .map(|mount| mount.target.to_lowercase())
        .collect();
    for (at, inner) in targets.iter().enumerate() {
        for (other, outer) in targets.iter().enumerate() {
            if at != other && inner.starts_with(&format!("{outer}/")) {
                return Err(refuse(
                    refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
                    format!(
                        "{} lies below the mount at {}: a native job's mounts are links and do not nest",
                        mounts[at].target, mounts[other].target
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Resolves the mounts of `fs` for the run `run_id` and keeps the plans: `Ready`, or the
/// refusal of the first mount that cannot be had.
pub(super) fn resolve(
    roots: &[MountRoot],
    plans: &mut Plans,
    run_id: &str,
    fs: &JobFs,
) -> FsPrepared {
    match fs
        .mounts
        .iter()
        .map(|mount| plan_of(roots, mount))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(list) => {
            plans.insert(run_id.to_owned(), list);
            FsPrepared::Ready
        }
        Err(refused) => FsPrepared::Refused(refused),
    }
}

/// The plan of `mount`: its target, and its source resolved to a canonical directory that
/// lies beneath a root of the agent that its mode allows.
fn plan_of(roots: &[MountRoot], mount: &JobMount) -> Result<MountPlan, NativeRefusal> {
    let rest = plan_target(&mount.target)?;
    let MountSource::HostPath { path } = &mount.source else {
        return Err(refuse(
            refusal::INPUT_NOT_ALLOWED,
            format!(
                "{}: a provider's tree is looked up once the provider has answered its directory",
                mount.target
            ),
        ));
    };
    let wanted = match mount.sub_path.as_deref() {
        Some(sub) if !sub.is_empty() => Path::new(path).join(sub),
        _ => PathBuf::from(path),
    };
    let write = mount.mode == MountMode::ReadWrite;
    let unavailable = |why: io::Error| {
        refuse(
            refusal::INPUT_UNAVAILABLE,
            format!("{}: {why}", wanted.display()),
        )
    };
    let (host, missing) = match fs::canonicalize(&wanted) {
        Ok(host) => (host, None),
        Err(why) if write && why.kind() == io::ErrorKind::NotFound => {
            let missing = Missing::find(&wanted).map_err(unavailable)?;
            (missing.path(), Some(missing))
        }
        Err(why) => return Err(unavailable(why)),
    };
    if missing.is_none() && !host.is_dir() {
        return Err(refuse(
            refusal::INPUT_UNAVAILABLE,
            format!("{}: not a directory", host.display()),
        ));
    }
    let beneath = roots
        .iter()
        .filter(|root| host.starts_with(&root.path))
        .max_by_key(|root| root.path.components().count());
    let root = match beneath {
        None => {
            return Err(refuse(
                refusal::INPUT_NOT_ALLOWED,
                format!(
                    "{} lies beneath no --mount-root or --mount-root-ro once its links are followed",
                    host.display()
                ),
            ));
        }
        Some(root) if write && root.read_only => {
            return Err(refuse(
                refusal::INPUT_NOT_ALLOWED,
                format!(
                    "{} lies beneath the root {}, which jobs may mount read-only only",
                    host.display(),
                    root.path.display()
                ),
            ));
        }
        Some(root) => root,
    };
    // What is there of the source: the directory, or the deepest part of a missing one, which
    // must lie beneath the root (the root itself is never made).
    let there = missing.as_ref().map_or(host.as_path(), Missing::base);
    if !there.starts_with(&root.path) {
        return Err(refuse(
            refusal::INPUT_UNAVAILABLE,
            format!(
                "{}: the root {} is not there",
                wanted.display(),
                root.path.display()
            ),
        ));
    }
    native_fs::local_source(there, &volume_of)?;
    let missing = missing.map(|missing| (missing, root.path.clone()));
    let host = AbsPath::new(&host)
        .map_err(|why| refuse(refusal::INVALID_MOUNT, format!("{}: {why}", host.display())))?;
    Ok(MountPlan {
        rest,
        host,
        write,
        no_exec: mount.no_exec,
        missing,
    })
}

/// Makes the link of each plan below `exec`, given to `uid`: a read-write mount's missing
/// directories first (below its root, given away by `chown`), then the missing directories
/// above the link, made by `make` (as the user's), then the link, then `chown` of the link
/// itself.
///
/// # Errors
///
/// A name that is there already, or a directory or link that cannot be made or given away;
/// the text names the mount's target.
pub(super) fn link_all(
    plans: &[MountPlan],
    exec: &Path,
    uid: u32,
    (chown, make): (&HandOver, MakeDir<'_>),
) -> io::Result<()> {
    let gid = fs::symlink_metadata(exec)?.gid();
    for plan in plans {
        let named = |why: io::Error| {
            io::Error::new(why.kind(), format!("{WORK}{}: {why}", plan.rest.display()))
        };
        if let Some((missing, root)) = &plan.missing {
            missing.make(root, uid, &**chown).map_err(named)?;
        }
        let above = plan.rest.parent().unwrap_or(Path::new(""));
        make_parents(exec, above, make).map_err(named)?;
        let link = exec.join(&plan.rest);
        symlink(Path::new(plan.host.as_str()), &link).map_err(named)?;
        chown(&link, uid, gid).map_err(named)?;
    }
    Ok(())
}

/// The grants of the profile for `plans`.
pub(super) fn grants(plans: &[MountPlan]) -> Vec<MountGrant> {
    plans
        .iter()
        .map(|plan| MountGrant {
            path: plan.host.clone(),
            write: plan.write,
            no_exec: plan.no_exec,
        })
        .collect()
}

/// The links of the read-only mounts below `exec`: the run may not replace them (the profile
/// denies writes there, as for an input).
pub(super) fn read_only_links<'a>(
    plans: &'a [MountPlan],
    exec: &'a Path,
) -> impl Iterator<Item = (&'static str, PathBuf)> + 'a {
    plans
        .iter()
        .filter(|plan| !plan.write)
        .map(|plan| ("a mount", exec.join(&plan.rest)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use js_agent_wire::Propagation;
    use js_testkit::dir::TestDir;

    use super::*;

    fn mount(target: &str) -> JobMount {
        JobMount {
            source: MountSource::HostPath {
                path: "/srv/data".to_owned(),
            },
            target: target.to_owned(),
            mode: MountMode::ReadOnly,
            sub_path: None,
            no_exec: false,
            propagation: Propagation::Private,
        }
    }

    fn plan(rest: &str, host: &str) -> MountPlan {
        MountPlan {
            rest: PathBuf::from(rest),
            host: AbsPath::parse(host).unwrap(),
            write: false,
            no_exec: true,
            missing: None,
        }
    }

    /// Asserts that a target is a link name only when it lies strictly below `/work` in
    /// normal form: the text after `/work/` is kept, and `/work`, `/work/`, a lookalike
    /// prefix, a target outside and one with an empty, `.` or `..` part are refused with
    /// `mount-target-unsupported-here`.
    ///
    /// Catches a target that makes a link outside `exec/` or in place of it.
    #[test]
    fn a_target_is_a_link_name_only_strictly_below_work() {
        assert_eq!(plan_target("/work/a").unwrap(), PathBuf::from("a"));
        assert_eq!(plan_target("/work/a/b").unwrap(), PathBuf::from("a/b"));
        for bad in [
            "/work",
            "/work/",
            "/workshop/a",
            "/elsewhere/a",
            "/work//a",
            "/work/./a",
            "/work/a/../b",
            "/work/a/",
        ] {
            let refused = plan_target(bad).unwrap_err();
            assert_eq!(
                refused.code,
                refusal::MOUNT_TARGET_UNSUPPORTED_HERE,
                "{bad}"
            );
            assert_eq!(
                refused.why,
                format!(
                    "the mount target {bad} is not below /work: a native job's mounts are links \
                     in its working directory"
                )
            );
        }
    }

    /// Asserts that a target below another mount's target is refused, also when the two
    /// differ in case, and that siblings and a lookalike prefix are not.
    ///
    /// Catches a link made inside the host directory another mount's link points at.
    #[test]
    fn a_target_below_another_targets_is_refused_in_any_case() {
        let ok = [mount("/work/a"), mount("/work/ab"), mount("/work/b/c")];
        assert!(nested(&ok).is_ok());
        let inside = [mount("/work/a"), mount("/work/A/b")];
        let refused = nested(&inside).unwrap_err();
        assert_eq!(refused.code, refusal::MOUNT_TARGET_UNSUPPORTED_HERE);
        assert_eq!(
            refused.why,
            "/work/A/b lies below the mount at /work/a: a native job's mounts are links and do \
             not nest"
        );
    }

    /// Asserts that a root of a path that is read-write and read-only under two names is
    /// writable, and that the others keep their mode, in path order.
    ///
    /// Catches a root listed twice, or a writable root reported read-only.
    #[test]
    fn roots_are_listed_once_by_path_and_writable_when_any_name_of_it_is() {
        let dir = TestDir::under(std::env::temp_dir(), "native-mount-roots");
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        for path in [&a, &b, &c] {
            fs::create_dir(path).unwrap();
        }
        let named = |name: &str, path: &Path| (name.to_owned(), path.to_path_buf());
        let roots = MountRoots::with_read_only(
            &[named("rw", &a), named("same", &b)],
            &[named("ro", &c), named("same-ro", &b)],
        )
        .unwrap();
        let listed = roots_of(&roots);
        let canonical = |path: &Path| fs::canonicalize(path).unwrap();
        assert_eq!(
            listed,
            [
                MountRoot {
                    path: canonical(&a),
                    read_only: false
                },
                MountRoot {
                    path: canonical(&b),
                    read_only: false
                },
                MountRoot {
                    path: canonical(&c),
                    read_only: true
                },
            ]
        );
    }

    /// Asserts that each failing step of making a link says the mount's target and why: a
    /// directory that cannot be made, a name that is there, and a link that cannot be given
    /// away; and that a plan whose steps succeed makes the link to the host path.
    ///
    /// Catches an error that does not name the mount, and a failed step that is passed over.
    #[test]
    fn a_link_that_cannot_be_made_names_its_mount() {
        let dir = TestDir::under(std::env::temp_dir(), "native-mount-links");
        let exec = dir.path().join("exec");
        fs::create_dir(&exec).unwrap();
        let give: HandOver = Arc::new(|_, _, _| Ok(()));
        let plans = [plan("x/y", "/Volumes/data/tree")];

        let cannot = |_: &Path| Err(io::Error::other("no room"));
        let why = link_all(&plans, &exec, 0, (&give, &cannot)).unwrap_err();
        assert_eq!(why.to_string(), "/work/x/y: no room");

        fs::create_dir(exec.join("x")).unwrap();
        let make = |path: &Path| fs::create_dir(path);
        fs::write(exec.join("x").join("y"), "").unwrap();
        let why = link_all(&plans, &exec, 0, (&give, &make)).unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::AlreadyExists);
        assert!(why.to_string().starts_with("/work/x/y: "), "{why}");
        fs::remove_file(exec.join("x").join("y")).unwrap();

        let refuse_give: HandOver = Arc::new(|_, _, _| Err(io::Error::other("not root")));
        let why = link_all(&plans, &exec, 0, (&refuse_give, &make)).unwrap_err();
        assert_eq!(why.to_string(), "/work/x/y: not root");
        fs::remove_file(exec.join("x").join("y")).unwrap();

        link_all(&plans, &exec, 0, (&give, &make)).unwrap();
        assert_eq!(
            fs::read_link(exec.join("x").join("y")).unwrap(),
            PathBuf::from("/Volumes/data/tree")
        );
    }

    /// Asserts that a link is never made through a parent that is a link or a file: a parent
    /// that is a link (as an input's link, or another mount's link, would be) fails the start
    /// naming the mount and the part, and nothing is made where the link leads; a parent
    /// that is a file fails the same way.
    ///
    /// Catches the agent, as root, making a link (and giving it away) inside a host
    /// directory a link among the parents leads to.
    #[test]
    fn a_link_is_never_made_through_a_parent_that_is_not_a_directory() {
        let dir = TestDir::under(std::env::temp_dir(), "native-mount-parents");
        let exec = dir.path().join("exec");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&exec).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, exec.join("in")).unwrap();
        let give: HandOver = Arc::new(|_, _, _| Ok(()));
        let make = |path: &Path| fs::create_dir(path);

        let through_link = [plan("in/m", "/Volumes/data/tree")];
        let why = link_all(&through_link, &exec, 0, (&give, &make)).unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            why.to_string(),
            format!(
                "/work/in/m: {} is there and is not a directory",
                exec.join("in").display()
            )
        );
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);

        fs::write(exec.join("file"), "").unwrap();
        let through_file = [plan("file/m", "/Volumes/data/tree")];
        let why = link_all(&through_file, &exec, 0, (&give, &make)).unwrap_err();
        assert_eq!(
            why.to_string(),
            format!(
                "/work/file/m: {} is there and is not a directory",
                exec.join("file").display()
            )
        );
    }

    /// Asserts that the grants carry each plan's path and modes, and that only the
    /// read-only plans have a link to protect.
    ///
    /// Catches a grant that widens a read-only mount, or a read-write link that the profile
    /// would deny writes through.
    #[test]
    fn grants_and_protected_links_follow_the_plans() {
        let mut writable = plan("out", "/Volumes/data/out");
        writable.write = true;
        writable.no_exec = false;
        let plans = [plan("tree", "/Volumes/data/tree"), writable];
        let granted = grants(&plans);
        assert_eq!(
            (granted[0].write, granted[0].no_exec),
            (false, true),
            "{granted:?}"
        );
        assert_eq!((granted[1].write, granted[1].no_exec), (true, false));
        assert_eq!(granted[1].path.as_str(), "/Volumes/data/out");
        let links: Vec<_> = read_only_links(&plans, Path::new("/h/exec")).collect();
        assert_eq!(links, [("a mount", PathBuf::from("/h/exec/tree"))]);
    }
}
