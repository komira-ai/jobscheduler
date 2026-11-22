//! The job's trees: each prepared and mounted by the mount provider, read-only, and linked into
//! `work` before the start; each released after the sweep.

use std::fs;
use std::os::unix::fs::{lchown, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::mac_job::Args;
use crate::mac_job::layout::Layout;
use crate::mac_job::profile::Kind;
use crate::mac_job::report::TreeUse;
use crate::mac_job::spec::JobFile;
use crate::provider::CallBounds;
use crate::provider::blocking::{Provider, beneath};

/// How long a tree may take to be prepared and mounted: the agent's `--mount-provider-wait`
/// default.
const MOUNT_WAIT: Duration = Duration::from_secs(120);

/// How long a release may take, with its retries.
const RELEASE_WAIT: Duration = Duration::from_secs(30);

/// A tree mounted and linked, not yet released.
pub(super) struct Mounted {
    /// The source index it was mounted under.
    index: u32,
    /// The path it was mounted at, resolved.
    resolved: PathBuf,
    /// What the report says of it.
    entry: TreeUse,
}

/// The provider the flags name, if they name one.
fn provider(args: &Args) -> Result<Option<(Provider, PathBuf)>, String> {
    let (Some(socket), Some(root)) = (&args.mount_provider, &args.mount_root) else {
        return Ok(None);
    };
    let user = match args.mount_provider_user.parse::<u32>() {
        Ok(uid) => uid,
        Err(_) => {
            super::sys::user(&args.mount_provider_user)
                .map_err(|why| {
                    format!("--mount-provider-user {}: {why}", args.mount_provider_user)
                })?
                .uid
        }
    };
    let root = fs::canonicalize(root).map_err(|why| format!("{}: {why}", root.display()))?;
    let provider = Provider {
        agent: args.agent_id.clone(),
        socket: socket.clone(),
        user,
        bounds: CallBounds::default(),
    };
    Ok(Some((provider, root)))
}

/// Mounts and links each of the job's trees, as the user `uid`'s links. A failure releases what
/// was mounted.
pub(super) fn link(
    args: &Args,
    job: &JobFile,
    layout: &Layout,
    uid: u32,
    gid: u32,
) -> Result<Vec<Mounted>, String> {
    if job.trees.is_empty() {
        return Ok(Vec::new());
    }
    if args.profile != Kind::ConfinedWrites {
        return Err("a job with trees runs under --profile confined-writes only".to_owned());
    }
    let Some((provider, root)) = provider(args)? else {
        return Err("the job names trees and no --mount-provider is given".to_owned());
    };
    let mut mounted = Vec::new();
    for (index, tree) in (0u32..).zip(&job.trees) {
        match link_one(&provider, &root, job, layout, index, tree, (uid, gid)) {
            Ok((entry, resolved)) => mounted.push(Mounted {
                index,
                resolved,
                entry,
            }),
            Err(why) => {
                // The failing tree may be prepared or mounted already: it goes too.
                let _ = provider.release(&job.id, index, RELEASE_WAIT);
                let _ = release(args, job, mounted);
                return Err(format!("tree {} at {}: {why}", tree.reference, tree.at));
            }
        }
    }
    Ok(mounted)
}

/// Mounts one tree and links `work/<at>` to it, the link and the folders it needs `owner`'s.
fn link_one(
    provider: &Provider,
    root: &Path,
    job: &JobFile,
    layout: &Layout,
    index: u32,
    tree: &crate::mac_job::spec::Tree,
    (uid, gid): (u32, u32),
) -> Result<(TreeUse, PathBuf), String> {
    let started = Instant::now();
    let path = provider
        .mount(&job.id, index, &tree.reference, MOUNT_WAIT)
        .map_err(|why| why.to_string())?;
    let mount_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let resolved = fs::canonicalize(&path).map_err(|why| format!("{path}: {why}"))?;
    if !beneath(&resolved, root) {
        return Err(format!(
            "the provider mounted it at {path}, not beneath {}",
            root.display()
        ));
    }
    // Each part of the place is walked without following a link: the inputs copied into work may
    // hold links, and root must make nothing through one.
    let mut link = layout.work();
    let parts: Vec<&str> = tree.at.split('/').collect();
    let (last, parents) = parts.split_last().unwrap_or((&"", &[]));
    for part in parents {
        link.push(part);
        match fs::symlink_metadata(&link) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{} is not a folder", link.display())),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&link).map_err(|why| format!("{}: {why}", link.display()))?;
                lchown(&link, Some(uid), Some(gid))
                    .map_err(|why| format!("{}: {why}", link.display()))?;
            }
            Err(why) => return Err(format!("{}: {why}", link.display())),
        }
    }
    link.push(last);
    symlink(&resolved, &link).map_err(|why| format!("{}: {why}", link.display()))?;
    lchown(&link, Some(uid), Some(gid)).map_err(|why| format!("{}: {why}", link.display()))?;
    let entry = TreeUse {
        reference: tree.reference.clone(),
        at: tree.at.clone(),
        path,
        mount_ms,
        released: Ok(()),
    };
    Ok((entry, resolved))
}

/// The profile's rules for the mounted trees, and their parameters: none without trees.
pub(super) fn rules(
    args: &Args,
    mounted: &[Mounted],
) -> Result<(String, Vec<(String, String)>), String> {
    if mounted.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let Some((_, root)) = provider(args)? else {
        return Err("trees are mounted and no --mount-root is given".to_owned());
    };
    let trees: Vec<PathBuf> = mounted.iter().map(|m| m.resolved.clone()).collect();
    crate::mac_job::profile::mount_rules(&root, &trees)
        .map_err(|tree| format!("{} is not beneath {}", tree.display(), root.display()))
}

/// Releases every mounted tree: the report's entries, each with its release.
pub(super) fn release(args: &Args, job: &JobFile, mounted: Vec<Mounted>) -> Vec<TreeUse> {
    let provider = match provider(args) {
        Ok(Some((provider, _))) => Some(provider),
        _ => None,
    };
    mounted
        .into_iter()
        .map(
            |Mounted {
                 index, mut entry, ..
             }| {
                entry.released = match &provider {
                    Some(provider) => provider
                        .release(&job.id, index, RELEASE_WAIT)
                        .map_err(|why| why.to_string()),
                    None => Err("no provider to release it with".to_owned()),
                };
                entry
            },
        )
        .collect()
}
