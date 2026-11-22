//! The job's put directories: made by the agent beneath the put ingest's root, as the node needs
//! them, linked into `work`, and listed after the job (see the module documentation of
//! `crate::mac_job`).
//!
//! The node (`local_cache.put_ingest`) reads, stores and removes a committed object, and removes
//! the job's directory once it is empty, as the node's user: so the key's folders
//! (`<root>/cas/b/<run>`) are that user's, mode 0755, and the job's directory (`<nonce>`) is the
//! job's user's with the node's group, mode 0770 (measured: with other modes the node could not
//! remove the object, which then was never stored).

use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, chown};
use std::path::{Component, Path, PathBuf};

use crate::mac_job::Args;
use crate::mac_job::layout::Layout;
use crate::mac_job::profile::Kind;
use crate::mac_job::report::PutUse;
use crate::mac_job::spec::JobFile;

/// The uid and gid the put ingest's node runs as: `--put-user`, a name or `uid:gid`.
fn node_user(args: &Args) -> Result<(u32, u32), String> {
    let given = &args.put_user;
    match given.split_once(':') {
        Some((uid, gid)) => match (uid.parse::<u32>(), gid.parse::<u32>()) {
            (Ok(uid), Ok(gid)) => Ok((uid, gid)),
            _ => Err(format!("--put-user {given}: a name or uid:gid")),
        },
        None => {
            let user =
                super::sys::user(given).map_err(|why| format!("--put-user {given}: {why}"))?;
            Ok((user.uid, user.gid))
        }
    }
}

/// Makes and links each of the job's put directories; their paths, in the job's order.
pub(super) fn make(
    args: &Args,
    job: &JobFile,
    layout: &Layout,
    uid: u32,
) -> Result<Vec<PathBuf>, String> {
    if job.put_dirs.is_empty() {
        return Ok(Vec::new());
    }
    if args.profile != Kind::ConfinedWrites {
        return Err("a job with put dirs runs under --profile confined-writes only".to_owned());
    }
    let Some(root) = &args.put_ingest else {
        return Err("the job names put dirs and no --put-ingest is given".to_owned());
    };
    let root = fs::canonicalize(root).map_err(|why| format!("{}: {why}", root.display()))?;
    let (node_uid, node_gid) = node_user(args)?;
    let mut made = Vec::new();
    for dir in &job.put_dirs {
        let path = PathBuf::from(&dir.path);
        let below = path
            .strip_prefix(&root)
            .ok()
            .filter(|rest| {
                !rest.as_os_str().is_empty()
                    && rest.components().all(|c| matches!(c, Component::Normal(_)))
            })
            .ok_or_else(|| {
                format!(
                    "put dir {} is not beneath {}",
                    path.display(),
                    root.display()
                )
            })?;
        // The key's folders, from the root down: the node's.
        let mut at = root.clone();
        let parts: Vec<_> = below.components().collect();
        let (_, parents) = parts.split_last().unwrap_or((&Component::CurDir, &[]));
        for part in parents {
            at.push(part);
            if fs::symlink_metadata(&at).is_err() {
                fs::DirBuilder::new()
                    .mode(0o755)
                    .create(&at)
                    .map_err(|why| format!("{}: {why}", at.display()))?;
                chown(&at, Some(node_uid), Some(node_gid))
                    .map_err(|why| format!("{}: {why}", at.display()))?;
            }
        }
        // The job's directory: made for this run, the job's with the node's group.
        fs::DirBuilder::new()
            .mode(0o770)
            .create(&path)
            .map_err(|why| format!("{}: {why}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o770))
            .map_err(|why| format!("{}: {why}", path.display()))?;
        chown(&path, Some(uid), Some(node_gid))
            .map_err(|why| format!("{}: {why}", path.display()))?;
        super::files::link_at(&layout.work(), &dir.at, &path, (uid, node_gid))?;
        made.push(path);
    }
    Ok(made)
}

/// What is in each put directory now.
pub(super) fn report(job: &JobFile, made: &[PathBuf]) -> Vec<PutUse> {
    job.put_dirs
        .iter()
        .zip(made)
        .map(|(dir, path)| {
            let (left, error) = match list(path) {
                Ok(left) => (left, None),
                Err(why) if why.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
                Err(why) => (Vec::new(), Some(why.to_string())),
            };
            PutUse {
                path: path.display().to_string(),
                at: dir.at.clone(),
                left,
                error,
            }
        })
        .collect()
}

/// The names in `dir`, sorted.
fn list(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}
