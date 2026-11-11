//! The assembly of a job's file system once its host paths are open and its images are in
//! the store (`docs/design/job-rootfs.md`, 4.2 step 3, 4.3 and 4.4), made by a thread of the
//! `js-paths` pool, since its mounts look up the providers' file systems:
//!
//! 1. each opened host path (the layers' first, then the mounts') cloned private, `nosuid`
//!    and `nodev` (read-only for a layer and a read-only mount, `noexec` when the mount asks)
//!    at `<mounts>/<lease>/src/<i>`, and the job user's search of each mount's clone checked,
//!    since `crun` binds it as that user;
//! 2. with layers, the root: an overlay at `<mounts>/<lease>/rootfs` of the layers (top
//!    first) over the image's tree, with its writable layer `rootfs-upper` (owned by the
//!    container's root, mode `0755`) in the lease directory;
//! 3. each copy-on-write mount an overlay of its clone at `<mounts>/<lease>/cow/<i>`, its
//!    writable layer `cow/<i>/upper` (owned by the job's own identity) in the lease
//!    directory;
//! 4. the targets: made in the root's writable layer (through the root, so the layers' links
//!    are read as the job reads them), or in the writable layer of the copy-on-write mount
//!    they lie below; one below a read-only mount must be in that mount (`invalid-mount`).
//!    One below a read-write mount, and with no layers one below no mount, is `crun`'s to
//!    make, as it makes a GPU mount's target today.
//!
//! Every mount point is listed ([`super::list`]) before its mount is made, so whatever
//! fails, what was made is unmounted by the run's cleanup.

use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use js_agent_wire::{JobMount, MountMode, refusal};
use js_contain::container::HostMount;
use js_contain::job_fs::{Attributes, MountCalls, Overlay, clone_at, mount_overlay};

use super::kernel::{PathCalls, make_layer_dir, reach};
use super::list;
use crate::lifecycle::NativeRefusal;

/// The writable layer of a layered root, in the lease directory.
pub(crate) const ROOT_UPPER: &str = "rootfs-upper";
/// The work directory of a layered root's overlay, in the lease directory.
pub(crate) const ROOT_WORK: &str = "rootfs-work";
/// The directory of the copy-on-write mounts' writable layers, in the lease directory.
pub(crate) const COW: &str = "cow";

/// One layer of a root, as the assembly finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lower {
    /// A host path: the next of the opened sources, in order.
    Source,
    /// An image's tree in the store.
    Image(PathBuf),
}

/// Who owns what the assembly makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owners {
    /// The container's root (the job user's first subordinate ids): a layered root's
    /// writable layer and the targets made in it.
    pub(crate) root: (u32, u32),
    /// The job's own identity in the container: a copy-on-write mount's writable layer
    /// and the targets made in it.
    pub(crate) job: (u32, u32),
    /// The job user, as which `crun` binds a mount's clone.
    pub(crate) user: (u32, u32),
}

/// What the assembly of one run is given.
#[derive(Debug)]
pub(crate) struct Plan {
    /// The run's directory of mounts, `<mounts>/<lease>`, made with its list.
    pub(crate) dir: PathBuf,
    /// The lease directory, where the writable layers go.
    pub(crate) lease: PathBuf,
    /// The image's tree, the root's bottom; none for a root of layers alone.
    pub(crate) image: Option<PathBuf>,
    /// The root's layers, base first.
    pub(crate) layers: Vec<Lower>,
    /// The mounts.
    pub(crate) mounts: Vec<JobMount>,
    /// The opened host paths: one for each [`Lower::Source`] of `layers`, in order, then one
    /// for each mount.
    pub(crate) sources: Vec<OwnedFd>,
    /// Who owns what is made.
    pub(crate) owners: Owners,
}

/// What an assembly made, for the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assembled {
    /// The layered root, mounted; none without layers (the image's tree is the root).
    pub(crate) root: Option<PathBuf>,
    /// The mounts `crun` binds, parents before children.
    pub(crate) mounts: Vec<HostMount>,
    /// How many host paths were cloned, at `src/0` and on.
    pub(crate) sources: usize,
}

/// Why an assembly did not end in a file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unassembled {
    /// What the start names cannot be given on this host's data.
    Refused(NativeRefusal),
    /// A call failed: the host's.
    Failed(String),
}

/// The refusal `code` for `why`.
fn refused(code: &'static str, why: String) -> Unassembled {
    Unassembled::Refused(NativeRefusal { code, why })
}

/// A failure of the step `what`.
fn failed(what: &'static str) -> impl Fn(io::Error) -> Unassembled {
    move |why| Unassembled::Failed(format!("{what}: {why}"))
}

/// A mount's failure.
fn mount_failed(why: js_contain::job_fs::MountError) -> Unassembled {
    Unassembled::Failed(why.to_string())
}

/// Assembles the file system of `plan` with `calls` and `paths`.
///
/// # Errors
/// See [`Unassembled`]; what was mounted is in the run's list.
pub(crate) fn assemble(
    calls: &dyn MountCalls,
    paths: &dyn PathCalls,
    plan: &Plan,
) -> Result<Assembled, Unassembled> {
    let src = |i: usize| plan.dir.join("src").join(i.to_string());
    let host_layers = plan
        .layers
        .iter()
        .filter(|lower| **lower == Lower::Source)
        .count();
    let layer = Attributes {
        read_only: true,
        no_exec: false,
    };
    let attributes =
        std::iter::repeat_n(layer, host_layers).chain(plan.mounts.iter().map(|mount| Attributes {
            read_only: mount.mode == MountMode::ReadOnly,
            no_exec: mount.no_exec,
        }));
    for (i, (source, attributes)) in plan.sources.iter().zip(attributes).enumerate() {
        let point = src(i);
        list::note(&plan.dir, &point).map_err(failed("list a clone"))?;
        clone_at(calls, source.as_fd(), &point, attributes).map_err(mount_failed)?;
    }
    for (j, mount) in plan.mounts.iter().enumerate() {
        paths
            .search_as(&src(host_layers + j), plan.owners.user)
            .map_err(|why| {
                let why = format!(
                    "the job user cannot reach the source of {}: {why}",
                    mount.target
                );
                refused(refusal::INPUT_UNAVAILABLE, why)
            })?;
    }
    let root = match plan.layers.is_empty() {
        true => None,
        false => Some(mount_root((calls, paths), plan, &src)?),
    };
    for (j, mount) in plan.mounts.iter().enumerate() {
        if mount.mode == MountMode::CopyOnWrite {
            mount_copy((calls, paths), plan, (host_layers + j, mount.no_exec), &src)?;
        }
    }
    make_targets(paths, plan, (host_layers, root.as_deref()))?;
    let mut order: Vec<(usize, &JobMount)> = plan.mounts.iter().enumerate().collect();
    order.sort_by_key(|(_, mount)| depth(&mount.target));
    let mounts = order
        .into_iter()
        .map(|(j, mount)| HostMount {
            target: mount.target.clone(),
            source: match mount.mode {
                MountMode::CopyOnWrite => plan.dir.join(COW).join((host_layers + j).to_string()),
                MountMode::ReadOnly | MountMode::ReadWrite => src(host_layers + j),
            },
            read_only: mount.mode == MountMode::ReadOnly,
            recursive: true,
            no_exec: mount.no_exec,
        })
        .collect();
    Ok(Assembled {
        root,
        mounts,
        sources: plan.sources.len(),
    })
}

/// How deep `target` lies: a parent is less deep than its children.
fn depth(target: &str) -> usize {
    Path::new(target).components().count()
}

/// Mounts the layered root of `plan` at `<mounts>/<lease>/rootfs`: its layers, top first,
/// over its image, under a writable layer owned by the container's root.
fn mount_root(
    (calls, paths): (&dyn MountCalls, &dyn PathCalls),
    plan: &Plan,
    src: &dyn Fn(usize) -> PathBuf,
) -> Result<PathBuf, Unassembled> {
    let mut next = 0;
    let mut lowers: Vec<PathBuf> = plan
        .layers
        .iter()
        .map(|lower| match lower {
            Lower::Source => {
                next += 1;
                src(next - 1)
            }
            Lower::Image(tree) => tree.clone(),
        })
        .collect();
    lowers.reverse();
    lowers.extend(plan.image.clone());
    let (upper, work) = (plan.lease.join(ROOT_UPPER), plan.lease.join(ROOT_WORK));
    make_layer_dir((&plan.lease, ROOT_UPPER), 0o755, plan.owners.root, paths)
        .map_err(failed("make the root's layer"))?;
    make_layer_dir((&plan.lease, ROOT_WORK), 0o700, plan.owners.user, paths)
        .map_err(failed("make the root's work"))?;
    let point = plan.dir.join("rootfs");
    list::note(&plan.dir, &point).map_err(failed("list the root"))?;
    let overlay = Overlay {
        lowers,
        upper,
        work,
        no_exec: false,
    };
    mount_overlay(calls, &overlay, &point).map_err(mount_failed)?;
    Ok(point)
}

/// Mounts the copy-on-write mount whose clone is source `i` at `<mounts>/<lease>/cow/<i>`,
/// under a writable layer owned by the job's identity, `noexec` when asked.
fn mount_copy(
    (calls, paths): (&dyn MountCalls, &dyn PathCalls),
    plan: &Plan,
    (i, no_exec): (usize, bool),
    src: &dyn Fn(usize) -> PathBuf,
) -> Result<(), Unassembled> {
    let layers = plan.lease.join(COW).join(i.to_string());
    let (upper, work) = (layers.join("upper"), layers.join("work"));
    make_layer_dir((&layers, "upper"), 0o755, plan.owners.job, paths)
        .map_err(failed("make a mount's layer"))?;
    make_layer_dir((&layers, "work"), 0o700, plan.owners.user, paths)
        .map_err(failed("make a mount's work"))?;
    let point = plan.dir.join(COW).join(i.to_string());
    list::note(&plan.dir, &point).map_err(failed("list a mount"))?;
    let overlay = Overlay {
        lowers: vec![src(i)],
        upper,
        work,
        no_exec,
    };
    mount_overlay(calls, &overlay, &point).map_err(mount_failed)
}

/// Makes, or finds, the target of each mount, parents first (see the [module](self)).
fn make_targets(
    paths: &dyn PathCalls,
    plan: &Plan,
    (host_layers, root): (usize, Option<&Path>),
) -> Result<(), Unassembled> {
    for (j, mount) in plan.mounts.iter().enumerate() {
        let target = Path::new(&mount.target);
        let parent = plan
            .mounts
            .iter()
            .enumerate()
            .filter(|(k, other)| *k != j && target.starts_with(&other.target))
            .max_by_key(|(_, other)| depth(&other.target));
        let (tree, under, owner) = match (parent, root) {
            (Some((k, other)), _) => {
                let i = (host_layers + k).to_string();
                let (tree, owner) = match other.mode {
                    MountMode::ReadOnly => (plan.dir.join("src").join(i), None),
                    MountMode::CopyOnWrite => (plan.dir.join(COW).join(i), Some(plan.owners.job)),
                    MountMode::ReadWrite => continue,
                };
                (tree, other.target.as_str(), owner)
            }
            (None, Some(root)) => (root.to_owned(), "/", Some(plan.owners.root)),
            (None, None) => continue,
        };
        let rel = target.strip_prefix(under).unwrap_or(target);
        reach(&tree, rel, owner, paths).map_err(|why| {
            let why = format!(
                "the target {} cannot be had below {under}: {why}",
                mount.target
            );
            refused(refusal::INVALID_MOUNT, why)
        })?;
    }
    Ok(())
}
