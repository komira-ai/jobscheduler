//! A run's output committed to tierstore's put ingest after the run (`crate::mac_put`): the
//! flags, the put target a start's mounts name, and the commit in the removal.
//!
//! - **Flags.** `--native-put-root` (the put ingest's directory, the node's `put_root`) and
//!   `--native-put-user` (the user the node runs as, by name or number) are given together or
//!   not at all. The root must be a directory that a read-write `--mount-root` holds (the
//!   lifecycle admits a mount's source only beneath a mount root, and every mount root is a
//!   read deny of every run, so no run reads the root), and with them the largest file a run
//!   may write (`--native-max-file-mib`) must be at least [`MIN_FILE_MIB`], the build
//!   service's largest pack.
//! - **Target.** A read-write host-path mount whose source lies beneath the put root is the
//!   run's put target when it is exactly `cas/b/<build>/<nonce>` below it, whether or not it
//!   exists yet (`native/mounts.rs`); it is never linked into the run's folder nor granted in
//!   its profile, so the job never holds a writable put directory. A mount at `/work/<rest>`
//!   gives the run the one output `<rest>/pack` (for the build service `.kbf-out/pack`),
//!   which the job writes in its own folder and the removal copies out with the declared
//!   outputs.
//! - **Commit**, in the removal, after the outputs are copied and before the account is
//!   retired, once (a removal tried again does not commit again): when the copy has no
//!   failure and holds `<rest>/pack`, the copy is committed as it is ([`mac_put::commit`]),
//!   the key's folders given to the node's user and the job's directory to its group
//!   ([`mac_put::hand_over`]). A run with no put target, or whose job wrote no pack, commits
//!   nothing and fails nothing. A commit that fails fails the removal only as its outputs
//!   ([`RuntimeError::outputs`]): the run's exit is unchanged and the account is retired. What
//!   was committed is kept by lease until the lifecycle asks
//!   ([`crate::lifecycle::Runtime::outputs_report`]).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::Args;
use js_agent_wire::{OutputKind, OutputSpec};

use super::mounts::{MountPlan, MountRoot};
use super::outputs;
use super::run::Run;
use crate::lifecycle::{OutputsReport, RuntimeError};
use crate::mac_put::{self, Committed, Node, PutTarget};

/// The least `--native-max-file-mib` beside the put flags: the build service's largest pack.
pub const MIN_FILE_MIB: u64 = 128;

/// The name of the one output a put mount gives a run, below the mount's target.
const PACK: &str = "pack";

/// The put flags as the command line gave them.
#[derive(Debug, Clone, Default, Args)]
pub struct PutFlags {
    /// The put ingest's directory (tierstore's `local_cache.put_ingest` put root): a run's
    /// read-write mount of `<this>/cas/b/<build>/<nonce>` is where its output is committed
    /// after it, and is never given to the job. It must be a directory a read-write
    /// `--mount-root` holds. Needs `--native-put-user` and `--native-accounts`.
    #[arg(long, value_name = "PATH")]
    pub native_put_root: Option<PathBuf>,
    /// The user the tierstore node runs as, by name or by number (a number is its uid and
    /// its group): the folders of each key become its, and each job's put directory its
    /// group's. Needs `--native-put-root`.
    #[arg(long, value_name = "USER")]
    pub native_put_user: Option<String>,
}

/// The checked put flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutConfig {
    /// The put root, canonical.
    pub root: PathBuf,
    /// The user and group the node runs as.
    pub node: Node,
}

/// Checks `flags` against the agent's `roots` and its largest file (`max_file_bytes`); a
/// user named by name is looked up with `user_of` (its uid and group, or `None`).
///
/// # Errors
///
/// The text names the flag at fault: one flag without the other, a root that is not an
/// absolute directory or that no read-write mount root holds, a user that is not known, or
/// a largest file below [`MIN_FILE_MIB`].
pub fn put_of(
    flags: &PutFlags,
    roots: &[MountRoot],
    max_file_bytes: u64,
    user_of: &dyn Fn(&str) -> io::Result<Option<Node>>,
) -> Result<Option<PutConfig>, String> {
    let (root, user) = match (&flags.native_put_root, &flags.native_put_user) {
        (None, None) => return Ok(None),
        (Some(root), Some(user)) => (root, user),
        (Some(_), None) => return Err("--native-put-root: needs --native-put-user".to_owned()),
        (None, Some(_)) => return Err("--native-put-user: needs --native-put-root".to_owned()),
    };
    let found = fs::canonicalize(root).ok().filter(|_| root.is_absolute());
    let root = found
        .filter(|found| found.is_dir())
        .ok_or_else(|| format!("--native-put-root: {} is not a directory", root.display()))?;
    if !roots
        .iter()
        .any(|held| !held.read_only && root.starts_with(&held.path))
    {
        return Err(format!(
            "--native-put-root: {} lies beneath no read-write --mount-root",
            root.display()
        ));
    }
    let node = match user.parse::<u32>() {
        Ok(id) => Node { uid: id, gid: id },
        Err(_) => user_of(user)
            .map_err(|why| format!("--native-put-user: {user}: {why}"))?
            .ok_or_else(|| format!("--native-put-user: no user {user:?}"))?,
    };
    if max_file_bytes < MIN_FILE_MIB << 20 {
        return Err(format!(
            "--native-max-file-mib: at least {MIN_FILE_MIB} beside --native-put-root (the \
             build service's largest pack)"
        ));
    }
    Ok(Some(PutConfig { root, node }))
}

/// What a run's put is: its target, the output it gives the run, and what the removal did.
pub(super) struct PutRun {
    target: PutTarget,
    /// `<rest>/pack`, the output the run's job writes.
    output: String,
    /// What the commit did, once the removal has tried it: what it committed, `None` when it
    /// had nothing to commit, or why it failed.
    done: Option<Result<Option<Committed>, String>>,
}

/// The put target among a start's `plans` and the output it gives the run, if a plan is
/// one.
pub(super) fn found_in(plans: &[MountPlan]) -> Option<(PutTarget, String)> {
    plans.iter().find_map(|plan| {
        let target = plan.put.clone()?;
        let output = plan.rest.join(PACK).to_string_lossy().into_owned();
        Some((target, output))
    })
}

/// Begins the put of the run `lease`: its output joins the run's outputs.
pub(super) fn begin(
    puts: &mut BTreeMap<String, PutRun>,
    lease: &str,
    run: &mut Run,
    (target, output): (PutTarget, String),
) {
    run.outputs.push(OutputSpec {
        path: output.clone(),
        kind: OutputKind::File,
    });
    let done = None;
    puts.insert(
        lease.to_owned(),
        PutRun {
            target,
            output,
            done,
        },
    );
}

/// Commits the output of the run `lease` (see the module documentation), once.
pub(super) fn commit_run(
    puts: &mut BTreeMap<String, PutRun>,
    config: Option<&PutConfig>,
    lease: &str,
    run: &Run,
) {
    let (Some(put), Some(config)) = (puts.get_mut(lease), config) else {
        return;
    };
    if put.done.is_some() {
        return;
    }
    let collected = run.collected.clone().unwrap_or_default();
    let copied = collected.copied.iter().any(|(path, _)| *path == put.output);
    if !collected.failed.is_empty() || !copied {
        put.done = Some(Ok(None));
        return;
    }
    let file = outputs::dir_in(&run.scratch).join(&put.output);
    let owner = rustix::process::geteuid().as_raw();
    let ready = |_: &Path| mac_put::hand_over(&config.root, &put.target, owner, config.node);
    let done = mac_put::commit(&file, &put.target, &ready);
    match &done {
        Ok(made) => tracing::info!(
            "native {lease}: output committed as {}, {} bytes, sha256 {}",
            made.key,
            made.bytes,
            made.sha256
        ),
        Err(why) => tracing::warn!("native {lease}: output not committed: {why}"),
    }
    put.done = Some(done.map(Some).map_err(|why| why.to_string()));
}

/// Ends the put of the removed run `lease`: what it committed is kept in `committed` for the
/// lifecycle to ask.
///
/// # Errors
///
/// The commit failed: the removal fails as the run's outputs.
pub(super) fn end_run(
    puts: &mut BTreeMap<String, PutRun>,
    committed: &mut BTreeMap<String, OutputsReport>,
    lease: &str,
) -> Result<(), RuntimeError> {
    match puts.remove(lease).and_then(|put| put.done) {
        Some(Err(why)) => Err(RuntimeError::outputs(format!(
            "outputs not committed: {why}"
        ))),
        Some(Ok(Some(made))) => {
            let report = OutputsReport {
                key: made.key,
                sha256: made.sha256,
                bytes: made.bytes,
            };
            committed.insert(lease.to_owned(), report);
            Ok(())
        }
        Some(Ok(None)) | None => Ok(()),
    }
}
