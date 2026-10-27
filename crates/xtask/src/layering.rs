//! The layering check: which workspace crate may depend on which, and which crates must
//! stay pure (`layers.toml` at the repository root, `docs/layering.md`).
//!
//! `layers.toml` lists the layers from the bottom up, and each layer's crates. A crate
//! may depend (through `[dependencies]`, `[build-dependencies]` and their `[target.*]`
//! forms) only on crates of its own layer or a lower one; `[dev-dependencies]` are not
//! edges, because a test may reach up to the crate it exercises. [`check`] reads the
//! manifests directly, so it needs no `cargo` and no network, and returns every
//! violation:
//!
//! - a workspace crate that no layer lists, or that two layers list;
//! - a dependency from a lower layer up to a higher one;
//! - a crate of a layer marked `pure` whose `src/lib.rs` does not deny
//!   `clippy::disallowed_methods` and `clippy::disallowed_types` (the lists in
//!   `clippy.toml`: no clock, no hashed collection, no socket) on one `#![deny(...)]` line;
//! - a package named in `single_source` (`js-types`) that has a second copy: it must
//!   appear in `Cargo.lock` exactly once, from the workspace, and every dependency on it
//!   must be a path or workspace dependency. Two copies would make one crate's `LeaseId`
//!   a different type from another's.
//!
//! A layer may also list `planned` crates: names reserved for crates that have not
//! arrived. A planned name that is not a workspace crate is not an error; once it is one
//! it counts as listed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Why the check could not run (a [`Violation`] is a finding, not a failure).
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A file could not be read.
    #[error("cannot read {path}: {why}")]
    Read {
        /// The file or directory.
        path: PathBuf,
        /// The operating system's reason.
        why: String,
    },
    /// A file is not what it must be.
    #[error("{path}: {why}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
}

/// One thing wrong with the workspace's layering.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Violation {
    /// A workspace crate that no layer lists.
    Unlisted(String),
    /// A crate that more than one layer lists.
    Listed(String),
    /// A dependency from a lower layer up to a higher one.
    Upward {
        /// The depending crate and its layer.
        from: (String, String),
        /// The crate it depends on and its layer.
        to: (String, String),
    },
    /// A crate of a pure layer that does not deny the disallowed items.
    NotDenying(String),
    /// A single-source package that is not in `Cargo.lock` exactly once, from the
    /// workspace.
    Copies {
        /// The package.
        package: String,
        /// How many `Cargo.lock` entries have its name.
        found: usize,
    },
    /// A dependency on a single-source package that is not a path or workspace one.
    OutsideSource {
        /// The manifest.
        manifest: String,
        /// The package.
        package: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Unlisted(c) => write!(f, "{c} is in no layer of layers.toml"),
            Violation::Listed(c) => write!(f, "{c} is in more than one layer of layers.toml"),
            Violation::Upward { from, to } => write!(
                f,
                "{} (layer {}) depends on {} (layer {}), a higher layer",
                from.0, from.1, to.0, to.1
            ),
            Violation::NotDenying(c) => write!(
                f,
                "{c} is in a pure layer but its src/lib.rs does not deny clippy::disallowed_methods and clippy::disallowed_types"
            ),
            Violation::Copies { package, found } => write!(
                f,
                "{package} must be in Cargo.lock once, from the workspace; found {found} entries"
            ),
            Violation::OutsideSource { manifest, package } => write!(
                f,
                "{manifest} depends on {package} by something other than a path or the workspace"
            ),
        }
    }
}

/// `layers.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    /// Packages that must exist once.
    #[serde(default)]
    single_source: Vec<String>,
    /// Bottom first.
    layer: Vec<Layer>,
}

/// One layer.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    name: String,
    #[serde(default)]
    pure: bool,
    #[serde(default)]
    crates: Vec<String>,
    #[serde(default)]
    planned: Vec<String>,
}

/// A workspace crate as its manifest declares it.
struct Member {
    dir: PathBuf,
    /// Every dependency (any kind), by package name, with how it is declared.
    deps: Vec<(String, toml::Value, bool)>,
}

/// The error for a failed operation on `path`.
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Read {
        path: path.to_owned(),
        why: e.to_string(),
    }
}

/// Reads `path` as text.
fn read(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(io(path))
}

/// Reads `path` as TOML.
fn read_toml(path: &Path) -> Result<toml::Table, Error> {
    read(path)?
        .parse()
        .map_err(|e: toml::de::Error| Error::Invalid {
            path: path.to_owned(),
            why: e.to_string(),
        })
}

/// A shorthand for the error of a manifest that lacks something.
fn invalid(path: &Path, why: &str) -> Error {
    Error::Invalid {
        path: path.to_owned(),
        why: why.to_owned(),
    }
}

/// The directories the root manifest's `workspace.members` name (`dir` or `dir/*`).
fn member_dirs(root: &Path, manifest: &toml::Table) -> Result<Vec<PathBuf>, Error> {
    let path = root.join("Cargo.toml");
    let patterns = manifest
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| invalid(&path, "no workspace.members"))?;
    let mut dirs = Vec::new();
    for pattern in patterns {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| invalid(&path, "a workspace member is not a string"))?;
        match pattern.strip_suffix("/*") {
            Some(parent) => {
                let parent = root.join(parent);
                let entries = std::fs::read_dir(&parent).map_err(io(&parent))?;
                for entry in entries {
                    let entry = entry.map_err(io(&parent))?;
                    if entry.path().join("Cargo.toml").is_file() {
                        dirs.push(entry.path());
                    }
                }
            }
            None => dirs.push(root.join(pattern)),
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// The dependency tables of `table` (a manifest or a `[target.*]` table), each with
/// whether it is `[dev-dependencies]`.
fn kinds(table: &toml::Table) -> impl Iterator<Item = (&toml::Table, bool)> {
    [
        ("dependencies", false),
        ("build-dependencies", false),
        ("dev-dependencies", true),
    ]
    .into_iter()
    .filter_map(|(key, dev)| Some((table.get(key)?.as_table()?, dev)))
}

/// The dependency tables of `manifest`, then those of each `[target.*]` table.
fn dependency_tables(manifest: &toml::Table) -> Vec<(&toml::Table, bool)> {
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    let targets = targets.into_iter().flat_map(toml::Table::values);
    kinds(manifest)
        .chain(targets.filter_map(toml::Value::as_table).flat_map(kinds))
        .collect()
}

/// Reads one workspace member: its package name and dependencies.
fn member(dir: &Path) -> Result<(String, Member), Error> {
    let path = dir.join("Cargo.toml");
    let manifest = read_toml(&path)?;
    let name = manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| invalid(&path, "no package.name"))?
        .to_owned();
    let mut deps = Vec::new();
    for (table, dev) in dependency_tables(&manifest) {
        for (key, value) in table {
            // `package = "real-name"` renames a dependency.
            let real = value
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(key);
            deps.push((real.to_owned(), value.clone(), dev));
        }
    }
    let member = Member {
        dir: dir.to_owned(),
        deps,
    };
    Ok((name, member))
}

/// Whether `src` has a `#![deny(...)]` (or `forbid`) line naming both disallowed lints.
fn denies_disallowed(src: &str) -> bool {
    ["clippy::disallowed_methods", "clippy::disallowed_types"]
        .iter()
        .all(|lint| {
            src.lines().any(|line| {
                let line = line.trim();
                (line.starts_with("#![deny(") || line.starts_with("#![forbid("))
                    && line.contains(lint)
            })
        })
}

/// Whether a dependency declaration names a path or the workspace (not a registry or git).
fn from_workspace(dep: &toml::Value) -> bool {
    dep.get("path").is_some() || dep.get("workspace").and_then(toml::Value::as_bool) == Some(true)
}

/// How many `[[package]]` entries of the lock file `lock` have a name in `names`, and how
/// many of those come from somewhere other than the workspace (have a `source`).
fn lock_entries(lock: &toml::Table, name: &str) -> (usize, usize) {
    let entries = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.get("name").and_then(toml::Value::as_str) == Some(name));
    entries.fold((0, 0), |(all, foreign), p| {
        (all + 1, foreign + usize::from(p.get("source").is_some()))
    })
}

/// Checks the workspace at `root` against `root/layers.toml`: every violation, sorted.
pub fn check(root: &Path) -> Result<Vec<Violation>, Error> {
    let layers_path = root.join("layers.toml");
    let file: File = toml::from_str(&read(&layers_path)?).map_err(|e| Error::Invalid {
        path: layers_path.clone(),
        why: e.to_string(),
    })?;
    let root_manifest = read_toml(&root.join("Cargo.toml"))?;
    let mut members = BTreeMap::new();
    for dir in member_dirs(root, &root_manifest)? {
        let (name, m) = member(&dir)?;
        members.insert(name, m);
    }

    let mut violations = BTreeSet::new();
    // Which layer (by index) lists each name.
    let mut layer_of: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, layer) in file.layer.iter().enumerate() {
        for name in layer.crates.iter().chain(&layer.planned) {
            // The first (lowest) listing stands, so one mistake reports once.
            match layer_of.get(name.as_str()) {
                Some(&first) if first != index => {
                    violations.insert(Violation::Listed(name.clone()));
                }
                Some(_) => {}
                None => {
                    layer_of.insert(name, index);
                }
            }
        }
    }
    let layer_name = |name: &str| file.layer[layer_of[name]].name.clone();

    for (name, m) in &members {
        let Some(&mine) = layer_of.get(name.as_str()) else {
            violations.insert(Violation::Unlisted(name.clone()));
            continue;
        };
        for (dep, _, dev) in &m.deps {
            let Some(&theirs) = layer_of.get(dep.as_str()) else {
                continue;
            };
            if !dev && theirs > mine {
                violations.insert(Violation::Upward {
                    from: (name.clone(), layer_name(name)),
                    to: (dep.clone(), layer_name(dep)),
                });
            }
        }
        if file.layer[mine].pure && !denies_disallowed(&read(&m.dir.join("src/lib.rs"))?) {
            violations.insert(Violation::NotDenying(name.clone()));
        }
    }

    let lock = read_toml(&root.join("Cargo.lock"))?;
    for package in &file.single_source {
        let (found, foreign) = lock_entries(&lock, package);
        if found != 1 || foreign != 0 {
            violations.insert(Violation::Copies {
                package: package.clone(),
                found,
            });
        }
        let in_root = root_manifest
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.get(package.as_str()))
            .into_iter()
            .map(|dep| ("Cargo.toml".to_owned(), dep));
        let in_members = members.values().flat_map(|m| {
            let manifest = m
                .dir
                .strip_prefix(root)
                .unwrap_or(&m.dir)
                .join("Cargo.toml");
            m.deps
                .iter()
                .filter(|(dep, _, _)| dep == package)
                .map(move |(_, value, _)| (manifest.display().to_string(), value))
        });
        for (manifest, dep) in in_root.chain(in_members) {
            if !from_workspace(dep) {
                violations.insert(Violation::OutsideSource {
                    manifest,
                    package: package.clone(),
                });
            }
        }
    }
    Ok(violations.into_iter().collect())
}
