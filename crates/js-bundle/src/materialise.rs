//! Resolving a validated config for one run (design section 5.1): where each tree is,
//! what the environment and arguments are, which reads the profile grants.
//!
//! The result is a function of the config, the store root and the inputs in
//! [`Inputs`] and of nothing else: no file is read, so every host with the same store
//! root resolves the same strings, and a kbf action's arguments are the same on every
//! host.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::{BundleConfig, EnvOp, ReadGrantKind};
use crate::env::check_env_name;
use crate::error::{ExpandError, MaterialiseError};
use crate::names::{DiffId, TreeName};
use crate::template::{Template, check_components};

/// The directories searched after a bundle's own `PATH` entries when the config's
/// `system_path` is true.
pub const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The directory of a tree in the store: `<store_root>/v1/objects/trees/<hex>`. A
/// function of the store root and the diff id, so one bundle has one absolute path on
/// every host that uses the same root.
pub fn tree_path(store_root: &Path, diff_id: &DiffId) -> PathBuf {
    store_root
        .join("v1")
        .join("objects")
        .join("trees")
        .join(diff_id.hex())
}

/// What a run supplies.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The store root: an absolute, normalised path.
    pub store_root: &'a Path,
    /// The writable clone of each of the config's `writable_trees`, and of no other
    /// tree. A placeholder for such a tree resolves to the clone.
    pub writable: &'a BTreeMap<TreeName, PathBuf>,
    /// The selected Xcode's developer directory, if the job selects one. It becomes
    /// `DEVELOPER_DIR` and is what `${developer_dir}` expands to.
    pub developer_dir: Option<&'a Path>,
    /// The environment the agent set before the bundle's rules (`HOME`, `TMPDIR`, ...).
    pub base_env: &'a [(String, String)],
    /// The job's environment: applied last, may not name a locked or reserved variable.
    pub job_env: &'a BTreeMap<String, String>,
    /// The job's arguments, with placeholders.
    pub job_args: &'a [String],
}

/// A bundle resolved for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialised {
    /// Each tree's directory: the store directory, or the writable clone.
    pub trees: BTreeMap<TreeName, PathBuf>,
    /// The environment: the base, then the bundle's rules in order, then
    /// `DEVELOPER_DIR`, then the job's. A variable keeps the position of its first
    /// appearance.
    pub env: Vec<(String, String)>,
    /// The job's arguments with placeholders resolved.
    pub args: Vec<String>,
    /// The reads for the profile builder, in the config's order.
    pub reads: Vec<ReadGrantKind>,
    /// Each entrypoint's resolved path.
    pub entrypoints: BTreeMap<String, PathBuf>,
}

/// An absolute, normalised path as text, or `None`: not UTF-8, not absolute, an empty,
/// `.` or `..` component, or a control character.
fn clean(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let rest = text.strip_prefix('/')?;
    if text.chars().any(char::is_control) {
        return None;
    }
    check_components(rest).ok()?;
    Some(text.to_string())
}

fn clean_or(path: &Path) -> Result<String, MaterialiseError> {
    clean(path).ok_or_else(|| MaterialiseError::BadPath(path.to_path_buf()))
}

fn put(env: &mut Vec<(String, String)>, name: &str, value: String) {
    match env.iter_mut().find(|entry| entry.0 == name) {
        Some(entry) => entry.1 = value,
        None => env.push((name.to_string(), value)),
    }
}

fn apply(env: &mut Vec<(String, String)>, op: EnvOp, name: &str, joined: String) {
    let old = env
        .iter()
        .find(|entry| entry.0 == name)
        .map(|entry| entry.1.as_str())
        .filter(|value| !value.is_empty());
    let new = match (op, old) {
        (EnvOp::Default, Some(_)) => return,
        (EnvOp::Prepend, Some(old)) => format!("{joined}:{old}"),
        (EnvOp::Append, Some(old)) => format!("{old}:{joined}"),
        _ => joined,
    };
    put(env, name, new);
}

fn expand_each<'a>(
    items: impl IntoIterator<Item = (String, &'a str)>,
    trees: &BTreeMap<TreeName, String>,
    developer_dir: Option<&str>,
) -> Result<Vec<String>, MaterialiseError> {
    items
        .into_iter()
        .map(|(what, text)| {
            Template::parse(text)
                .map_err(ExpandError::from)
                .and_then(|template| template.expand(trees, developer_dir))
                .map_err(|source| MaterialiseError::Expand { what, source })
        })
        .collect()
}

/// Resolves `config` for a run.
///
/// The config is validated first. Then, in order: the trees (the store directory of
/// each, or the writable clone); the environment (the base, the rules in order, then
/// `PATH` extended by [`SYSTEM_PATH`] when `system_path` is set, then `DEVELOPER_DIR`
/// when a developer directory is given, then the job's variables); the job's
/// arguments; the entrypoints. A job variable that is reserved
/// ([`check_env_name`]) or locked by a bundle rule is refused.
pub fn materialise(
    config: &BundleConfig,
    inputs: &Inputs<'_>,
) -> Result<Materialised, MaterialiseError> {
    config.validate()?;
    let root = clean_or(inputs.store_root)?;
    let developer_dir = inputs.developer_dir.map(clean_or).transpose()?;
    let developer_dir = developer_dir.as_deref();

    let wanted: BTreeSet<&TreeName> = config.writable_trees.iter().collect();
    if let Some(name) = inputs.writable.keys().find(|name| !wanted.contains(name)) {
        return Err(MaterialiseError::UnexpectedWritable(name.clone()));
    }
    let mut dirs = BTreeMap::new();
    let mut paths = BTreeMap::new();
    for layer in &config.layers {
        let path = match inputs.writable.get(&layer.tree) {
            Some(clone) => clone.clone(),
            None if wanted.contains(&layer.tree) => {
                return Err(MaterialiseError::MissingWritable(layer.tree.clone()));
            }
            None => tree_path(Path::new(&root), &layer.diff_id),
        };
        dirs.insert(layer.tree.clone(), clean_or(&path)?);
        paths.insert(layer.tree.clone(), path);
    }

    let mut env: Vec<(String, String)> = inputs.base_env.to_vec();
    let mut locked = BTreeSet::new();
    for rule in &config.env {
        let texts = rule
            .values
            .iter()
            .map(|value| (format!("env {}", rule.name), value.as_str()));
        let values = expand_each(texts, &dirs, developer_dir)?;
        apply(&mut env, rule.op, &rule.name, values.join(":"));
        if rule.locked {
            locked.insert(rule.name.as_str());
        }
    }
    if config.system_path {
        apply(&mut env, EnvOp::Append, "PATH", SYSTEM_PATH.to_string());
    }
    if let Some(dir) = developer_dir {
        put(&mut env, "DEVELOPER_DIR", dir.to_string());
    }
    for (name, value) in inputs.job_env {
        check_env_name(name).map_err(|why| MaterialiseError::JobEnvName {
            name: name.clone(),
            why,
        })?;
        if locked.contains(name.as_str()) {
            return Err(MaterialiseError::Locked(name.clone()));
        }
        let text = [(format!("env {name}"), value.as_str())];
        let value = expand_each(text, &dirs, developer_dir)?.concat();
        put(&mut env, name, value);
    }

    let texts = inputs
        .job_args
        .iter()
        .enumerate()
        .map(|(index, arg)| (format!("arg {index}"), arg.as_str()));
    let args = expand_each(texts, &dirs, developer_dir)?;
    let texts = config
        .entrypoints
        .iter()
        .map(|(name, entry)| (format!("entrypoint {name}"), entry.path.as_str()));
    let resolved = expand_each(texts, &dirs, developer_dir)?;
    let entrypoints = config
        .entrypoints
        .keys()
        .cloned()
        .zip(resolved.into_iter().map(PathBuf::from))
        .collect();
    Ok(Materialised {
        trees: paths,
        env,
        args,
        reads: config.seatbelt.reads.clone(),
        entrypoints,
    })
}
