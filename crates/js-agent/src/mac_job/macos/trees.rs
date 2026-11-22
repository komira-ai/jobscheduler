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
            Ok(entry) => mounted.push(Mounted { index, entry }),
            Err(why) => {
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
) -> Result<TreeUse, String> {
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
    let link = layout.work().join(&tree.at);
    let work = layout.work();
    let mut parent = link
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| work.clone());
    let mut made = Vec::new();
    while parent != work && !parent.exists() {
        made.push(parent.clone());
        let Some(up) = parent.parent() else { break };
        parent = up.to_path_buf();
    }
    for dir in made.iter().rev() {
        fs::create_dir(dir).map_err(|why| format!("{}: {why}", dir.display()))?;
        lchown(dir, Some(uid), Some(gid)).map_err(|why| format!("{}: {why}", dir.display()))?;
    }
    symlink(&resolved, &link).map_err(|why| format!("{}: {why}", link.display()))?;
    lchown(&link, Some(uid), Some(gid)).map_err(|why| format!("{}: {why}", link.display()))?;
    Ok(TreeUse {
        reference: tree.reference.clone(),
        at: tree.at.clone(),
        path,
        mount_ms,
        released: Ok(()),
    })
}

/// Releases every mounted tree: the report's entries, each with its release.
pub(super) fn release(args: &Args, job: &JobFile, mounted: Vec<Mounted>) -> Vec<TreeUse> {
    let provider = match provider(args) {
        Ok(Some((provider, _))) => Some(provider),
        _ => None,
    };
    mounted
        .into_iter()
        .map(|Mounted { index, mut entry }| {
            entry.released = match &provider {
                Some(provider) => provider
                    .release(&job.id, index, RELEASE_WAIT)
                    .map_err(|why| why.to_string()),
                None => Err("no provider to release it with".to_owned()),
            };
            entry
        })
        .collect()
}
