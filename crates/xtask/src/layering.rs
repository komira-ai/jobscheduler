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
//! - a crate of a layer marked `pure` whose library root or any binary root (`src/main.rs`,
//!   `src/bin/*`, a `[[bin]]` path) does not deny `clippy::disallowed_methods` and
//!   `clippy::disallowed_types` (the lists in `clippy.toml`: no clock read, no sleep, no
//!   `HashMap`/`HashSet`, no TCP or UDP socket) in one `#![deny(...)]` or `#![forbid(...)]`
//!   attribute line (a lint name in a trailing comment does not count), or that anywhere in
//!   its `src/` tree turns one of those lints, `clippy::all` or `clippy::style` down again
//!   with an attribute that is not `deny` or `forbid` ([`Violation::Relaxed`]);
//! - a non-dev dependency of a workspace crate that is neither a crate `layers.toml` lists
//!   nor a registry dependency declared in the root `[workspace.dependencies]`
//!   ([`Violation::Unknown`]: a `kbf-*` crate, an unlisted path or git dependency), or an
//!   external one that a member declares with something other than `workspace = true`
//!   ([`Violation::NotWorkspace`]);
//! - with [`check_closure`], a pure crate that reaches, through its resolved normal
//!   dependencies, an async runtime, network stack or random source ([`FORBIDDEN`]);
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
    /// A crate root (library or binary) of a pure layer's crate that does not deny the
    /// disallowed items: the crate, then the root file from the repository root.
    NotDenying(String, String),
    /// A single-source package that is not in `Cargo.lock` exactly once, from the
    /// workspace.
    Copies {
        /// The package.
        package: String,
        /// How many `Cargo.lock` entries have its name.
        found: usize,
    },
    /// A non-dev dependency that is neither a crate `layers.toml` lists nor a registry
    /// dependency of the root `[workspace.dependencies]`.
    Unknown {
        /// The manifest.
        manifest: String,
        /// The package.
        package: String,
    },
    /// An external dependency a member declares without `workspace = true`.
    NotWorkspace {
        /// The manifest.
        manifest: String,
        /// The package.
        package: String,
    },
    /// A pure crate's source turns a purity lint down (`allow`, `expect`, `warn`).
    Relaxed {
        /// The file, from the repository root.
        file: String,
        /// The attribute as written.
        attribute: String,
    },
    /// A pure crate that reaches a forbidden external crate through its dependencies.
    ForbiddenReach(String),
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
            Violation::NotDenying(c, file) => write!(
                f,
                "{c} is in a pure layer but {file} does not deny clippy::disallowed_methods and clippy::disallowed_types"
            ),
            Violation::Unknown { manifest, package } => write!(
                f,
                "{manifest} depends on {package}, which is neither a crate listed in layers.toml nor a registry dependency of the root [workspace.dependencies]"
            ),
            Violation::NotWorkspace { manifest, package } => write!(
                f,
                "{manifest} depends on {package} without `workspace = true`; external dependencies are declared once in the root [workspace.dependencies]"
            ),
            Violation::Relaxed { file, attribute } => write!(
                f,
                "{file} relaxes a purity lint in a pure crate: {attribute}"
            ),
            Violation::ForbiddenReach(chain) => {
                write!(f, "a pure crate reaches a forbidden crate: {chain}")
            }
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
    /// The crate roots: the library and every binary target's file.
    roots: Vec<PathBuf>,
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
        roots: roots(dir, &manifest),
        deps,
    };
    Ok((name, member))
}

/// Whether `src` has a `#![deny(...)]` (or `forbid`) attribute line whose list names both
/// disallowed lints. A comment after the attribute is not part of it.
fn denies_disallowed(src: &str) -> bool {
    let lists: Vec<Vec<&str>> = src
        .lines()
        .filter_map(|line| {
            let line = line.split("//").next()?.split("/*").next()?.trim();
            let inner = line
                .strip_prefix("#![deny(")
                .or_else(|| line.strip_prefix("#![forbid("))?
                .strip_suffix(")]")?;
            Some(inner.split(',').map(str::trim).collect())
        })
        .collect();
    ["clippy::disallowed_methods", "clippy::disallowed_types"]
        .iter()
        .all(|lint| lists.iter().any(|list| list.contains(lint)))
}

/// The lints a pure crate must not turn down: the two disallowed lists, and the groups
/// that contain them.
const PURITY_LINTS: [&str; 3] = ["clippy::disallowed_", "clippy::all", "clippy::style"];

/// The attributes in `src` that name a purity lint and are not `deny` or `forbid`
/// (`allow`, `expect`, `warn`, `cfg_attr`), as written up to the lint name.
fn relaxing_attributes(src: &str) -> Vec<String> {
    let mut found = Vec::new();
    for lint in PURITY_LINTS {
        for (at, _) in src.match_indices(lint) {
            let before = &src[..at];
            let start = [before.rfind("#!["), before.rfind("#[")]
                .into_iter()
                .flatten()
                .max();
            let Some(start) = start else { continue };
            let attribute = &before[start..];
            // A `]` between the attribute's opening and the lint name closes it: the
            // name is outside any attribute.
            if attribute.contains(']') {
                continue;
            }
            let head = attribute
                .strip_prefix("#![")
                .or_else(|| attribute.strip_prefix("#["))
                .unwrap_or(attribute)
                .trim();
            if !(head.starts_with("deny(") || head.starts_with("forbid(")) {
                let full = &src[at..];
                let end = full
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                    .unwrap_or(full.len());
                let written = attribute.split_whitespace().collect::<Vec<_>>().join(" ");
                found.push(format!("{written}{}", &full[..end]));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Every `.rs` file under `dir`, recursively, sorted. A directory that cannot be read has
/// none (the library root, which the check reads first, is the file that must exist).
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The crate roots of the package in `dir`: the library (always, `src/lib.rs` unless the
/// manifest moves it) and every binary target (`src/main.rs`, `src/bin/*.rs`,
/// `src/bin/*/main.rs`, a `[[bin]]` path) that exists.
fn roots(dir: &Path, manifest: &toml::Table) -> Vec<PathBuf> {
    let path_of = |table: &toml::Value| {
        table
            .get("path")
            .and_then(toml::Value::as_str)
            .map(|p| dir.join(p))
    };
    let mut roots = BTreeSet::new();
    roots.insert(
        manifest
            .get("lib")
            .and_then(path_of)
            .unwrap_or_else(|| dir.join("src/lib.rs")),
    );
    let main = dir.join("src/main.rs");
    if main.is_file() {
        roots.insert(main);
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("src/bin")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                roots.insert(path);
            } else if path.join("main.rs").is_file() {
                roots.insert(path.join("main.rs"));
            }
        }
    }
    let bins = manifest.get("bin").and_then(toml::Value::as_array);
    roots.extend(bins.into_iter().flatten().filter_map(path_of));
    roots.into_iter().collect()
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

/// Reads `root/layers.toml`.
fn layers(root: &Path) -> Result<File, Error> {
    let path = root.join("layers.toml");
    toml::from_str(&read(&path)?).map_err(|e| Error::Invalid {
        path,
        why: e.to_string(),
    })
}

/// Checks the workspace at `root` against `root/layers.toml`: every violation, sorted.
pub fn check(root: &Path) -> Result<Vec<Violation>, Error> {
    let file = layers(root)?;
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
        if file.layer[mine].pure {
            for root_file in &m.roots {
                if !denies_disallowed(&read(root_file)?) {
                    let shown = root_file.strip_prefix(root).unwrap_or(root_file);
                    violations.insert(Violation::NotDenying(
                        name.clone(),
                        shown.display().to_string(),
                    ));
                }
            }
            for source in rust_files(&m.dir.join("src")) {
                let shown = source.strip_prefix(root).unwrap_or(&source);
                for attribute in relaxing_attributes(&read(&source)?) {
                    violations.insert(Violation::Relaxed {
                        file: shown.display().to_string(),
                        attribute,
                    });
                }
            }
        }
    }

    // Every non-dev dependency is a listed crate or a registry dependency of the root
    // table, which the members take with `workspace = true`.
    let external: BTreeSet<String> = root_manifest
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter(|(_, dep)| dep.get("path").is_none() && dep.get("git").is_none())
        .map(|(key, dep)| {
            let real = dep.get("package").and_then(toml::Value::as_str);
            real.unwrap_or(key).to_owned()
        })
        .filter(|name| !name.starts_with("kbf-") && !name.starts_with("kbf_"))
        .collect();
    for m in members.values() {
        let manifest = m
            .dir
            .strip_prefix(root)
            .unwrap_or(&m.dir)
            .join("Cargo.toml")
            .display()
            .to_string();
        for (dep, value, dev) in &m.deps {
            // An unlisted workspace crate is reported as `Unlisted`, once.
            if *dev || layer_of.contains_key(dep.as_str()) || members.contains_key(dep) {
                continue;
            }
            let violation = if !external.contains(dep) {
                Violation::Unknown {
                    manifest: manifest.clone(),
                    package: dep.clone(),
                }
            } else if value.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                Violation::NotWorkspace {
                    manifest: manifest.clone(),
                    package: dep.clone(),
                }
            } else {
                continue;
            };
            violations.insert(violation);
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

/// Crates a pure crate must not reach, directly or through any other crate: an async
/// runtime, a network stack or a random source. (kbf's layering test keeps the same list.)
pub const FORBIDDEN: &[&str] = &[
    "tokio",
    "tonic",
    "hyper",
    "reqwest",
    "rand",
    "rand_core",
    "rand_chacha",
    "getrandom",
    "mio",
    "socket2",
];

/// `cargo metadata` for the workspace at `root`, as JSON (resolved against `Cargo.lock`).
/// Output that is not JSON comes back as `null`, which [`check_closure`] refuses.
pub fn cargo_metadata(root: &Path) -> Result<serde_json::Value, Error> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(root)
        .output()
        .map_err(io(root))?;
    if !out.status.success() {
        return Err(invalid(root, &String::from_utf8_lossy(&out.stderr)));
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or_default())
}

/// The part of `cargo metadata` the walk reads.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_members: BTreeSet<String>,
    resolve: Resolve,
}

/// One package of the metadata.
#[derive(Deserialize)]
struct MetaPackage {
    id: String,
    name: String,
}

/// The resolved graph.
#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

/// One package's resolved dependencies.
#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<MetaDep>,
}

/// One resolved dependency edge.
#[derive(Deserialize)]
struct MetaDep {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

/// How an edge is used: `kind` is absent (null) for a normal dependency, else `dev` or
/// `build`.
#[derive(Deserialize)]
struct DepKind {
    kind: Option<String>,
}

/// Walks the normal (not dev, not build) dependencies of each crate of a pure layer in
/// `metadata` (the output of [`cargo_metadata`]) and reports every [`FORBIDDEN`] crate
/// reached, with the chain that reaches it. A pure crate that is not in `metadata` is an
/// error, so a rename cannot make the walk pass on nothing.
pub fn check_closure(root: &Path, metadata: &serde_json::Value) -> Result<Vec<Violation>, Error> {
    let file = layers(root)?;
    let bad = |why: &str| invalid(&root.join("Cargo.lock"), &format!("cargo metadata: {why}"));
    let meta: Metadata =
        serde_json::from_value(metadata.clone()).map_err(|e| bad(&e.to_string()))?;
    let names: BTreeMap<&str, &str> = meta
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.name.as_str()))
        .collect();
    let normal: BTreeMap<&str, Vec<&str>> = meta
        .resolve
        .nodes
        .iter()
        .map(|node| {
            let linked = node
                .deps
                .iter()
                .filter(|d| d.dep_kinds.iter().any(|k| k.kind.is_none()));
            (node.id.as_str(), linked.map(|d| d.pkg.as_str()).collect())
        })
        .collect();
    let mut found = BTreeSet::new();
    for layer in file.layer.iter().filter(|l| l.pure) {
        for krate in &layer.crates {
            let id = meta
                .packages
                .iter()
                .find(|p| &p.name == krate && meta.workspace_members.contains(&p.id))
                .ok_or_else(|| bad(&format!("pure crate {krate} is not a workspace member")))?
                .id
                .as_str();
            let mut seen = BTreeSet::from([id]);
            let mut queue = std::collections::VecDeque::from([(id, krate.clone())]);
            while let Some((id, chain)) = queue.pop_front() {
                for &dep in &normal[id] {
                    if !seen.insert(dep) {
                        continue;
                    }
                    let chain = format!("{chain} -> {}", names[dep]);
                    if FORBIDDEN.contains(&names[dep]) {
                        found.insert(Violation::ForbiddenReach(chain.clone()));
                    }
                    queue.push_back((dep, chain));
                }
            }
        }
    }
    Ok(found.into_iter().collect())
}
