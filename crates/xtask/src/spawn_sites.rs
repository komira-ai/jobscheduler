//! The spawn-site check: `js-agent` and `js-mac-session` start every program through one
//! module, `js_mac_session::disclaim`, which on macOS starts it with its privacy (TCC)
//! responsibility disclaimed (that module's documentation says why). The check is used by
//! the `spawn_sites` test rather than a verb: `cargo test -p xtask --test spawn_sites`.
//!
//! **Which crates.** [`crates`] starts from [`START_CRATES`] and follows, through each
//! `crates/<name>/Cargo.toml`, every `path` dependency of `[dependencies]` and of
//! `[target.<cfg>.dependencies]`, transitively. The one target table it does not follow is
//! the one whose key is exactly `cfg(target_os = "linux")` (code that is not built for
//! macOS). Dev- and build-dependencies are not followed, and neither is an `optional`
//! dependency of a crate whose `default` feature is absent or empty: a build of the agent
//! links it only when something turns a feature of that crate on, which the `agent_features`
//! test refuses for the job client (`js-client`'s `jobs`). A crate with a non-empty
//! `default` feature has every dependency followed. A manifest that cannot be read is an
//! error.
//!
//! **Which files.** [`check`] parses (with `syn`) the production source of those crates.
//! The roots of a crate are the `[lib] path`, each `[[bin]] path`, and the files of the
//! default layout that exist: `src/lib.rs`, `src/main.rs`, `src/bin/*.rs` and
//! `src/bin/*/main.rs`. From each root it follows every `mod name;` to `name.rs` or
//! `name/mod.rs`, or a `#[path = "literal"] mod name;` (outside an inline module) to the
//! file at that path from the directory of the declaring file; that file's own child
//! modules are in its own directory, as for a `mod.rs`. A crate with no root is an error. It leaves out what only a test build
//! compiles: a module, function, `use`, `impl` block or method under `#[cfg(test)]`, or
//! under a `#[cfg(all(...))]` that holds `test`. It also leaves out the spawner itself
//! ([`SPAWNER`]). Code built for one platform only is not left out: on every other
//! platform the spawner is the standard library's `Command` unchanged, so one rule holds
//! everywhere and the check needs to know nothing about targets.
//!
//! **What is a site.** In what is left, each of these is a [`Site`], a place where a
//! program could start without the disclaim. The words of a path, of a type path and of
//! every import (a `use` tree is expanded to the full path of each leaf, and a rename is
//! matched by the original name) are looked at in sequence:
//!
//! - `process` then `Command` (`std::process::Command`, `tokio::process::Command`), even
//!   as a bare import or a type alias: `process::Command`;
//! - `CommandExt`, which covers `exec` and `pre_exec` called as plain functions:
//!   `CommandExt`;
//! - `Command` then `new`: `Command::new`;
//! - a method call named `pre_exec` or `exec`: `.pre_exec`, `.exec`;
//! - `libc` then a C library function that executes a program ([`EXECUTORS`]) or runs a
//!   shell (`system`, `popen`), such as `libc::system`, also as a foreign function of that
//!   name; and any of [`EXECUTORS`] by itself, such as `execv`;
//! - a glob import of `std::process`, `tokio::process`, `std::os::unix::process` or
//!   `libc`: `process::*`, `libc::*`;
//! - an import that renames the `libc` crate or one of the `process` modules above
//!   (`libc renamed`, `std::process renamed`, ...), and an `extern crate libc` under
//!   another name: under it `Command`, `system` and `popen` would be named without the
//!   words looked for (`use std::process as p; type C = p::Command;`).
//!
//! The same sequences are looked for in the tokens of every macro call, which `syn` does
//! not parse; a braced import list there is expanded with its prefix as in code. A `mod`
//! declaration whose file is missing, a `#[path]` attribute on an inline module, inside
//! an inline module or with a value that is not a string literal, a `#[cfg_attr(..)]`
//! that may set a module's `path` (the file would depend on the target), an `include!`
//! (the check follows none of these), a crate with no root and a spawner that no module
//! declares are errors, never a part of the tree skipped in silence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Meta, Token, UseTree};

/// The crates the check starts from; it adds the crates they depend on (see [`crates`]).
pub const START_CRATES: [&str; 2] = ["js-agent", "js-mac-session"];

/// The one module that may start programs, from the repository root.
pub const SPAWNER: &str = "crates/js-mac-session/src/disclaim.rs";

/// The C library's functions that execute a program.
pub const EXECUTORS: [&str; 11] = [
    "execl",
    "execle",
    "execlp",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "fexecve",
    "posix_spawn",
    "posix_spawnp",
    "vfork",
];

/// The key of the one target table of a manifest that is not followed.
const LINUX_ONLY: &str = r#"cfg(target_os = "linux")"#;

/// The C library's functions that run a shell, besides [`EXECUTORS`].
const SHELLS: [&str; 2] = ["system", "popen"];

/// The word sequences that start a program, with the name a [`Site`] gives each, besides
/// the `libc` functions and the single words of [`EXECUTORS`]. A path's words are its
/// segments; a method call's are `.` and the method; a macro's are its tokens, without the
/// `:` of a `::`.
const SEQUENCES: [(&[&str], &str); 17] = [
    (&["process", "Command"], "process::Command"),
    (&["CommandExt"], "CommandExt"),
    (&["Command", "new"], "Command::new"),
    (&[".", "pre_exec"], ".pre_exec"),
    (&[".", "exec"], ".exec"),
    (&["std", "process", "*"], "process::*"),
    (&["tokio", "process", "*"], "process::*"),
    (&["unix", "process", "*"], "process::*"),
    (&["libc", "*"], "libc::*"),
    (&["libc", "as"], "libc renamed"),
    (&["libc", "self", "as"], "libc renamed"),
    (&["std", "process", "as"], "std::process renamed"),
    (&["std", "process", "self", "as"], "std::process renamed"),
    (&["tokio", "process", "as"], "tokio::process renamed"),
    (
        &["tokio", "process", "self", "as"],
        "tokio::process renamed",
    ),
    (&["unix", "process", "as"], "std::os::unix::process renamed"),
    (
        &["unix", "process", "self", "as"],
        "std::os::unix::process renamed",
    ),
];

/// The modules whose import under another name is a site: under it, `Command`, `system`
/// or `popen` would be named without the words the check looks for (`use std::process as
/// p; type C = p::Command;`).
const RENAMED: [&[&str]; 4] = [
    &["libc"],
    &["std", "process"],
    &["tokio", "process"],
    &["std", "os", "unix", "process"],
];

/// A place in the production code of the checked crates that starts a program without the
/// spawner.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// The file, from the repository root.
    pub file: PathBuf,
    /// The line, from 1.
    pub line: usize,
    /// What starts the program there, such as `Command::new`.
    pub what: String,
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file.display(), self.line, self.what)
    }
}

/// Why the check could not read the tree. Each path is from the repository root.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A source file or a manifest could not be read.
    #[error("{}: {source}", .path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        source: std::io::Error,
    },
    /// A source file is not Rust that `syn` parses.
    #[error("{}: {source}", .path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The reason.
        source: syn::Error,
    },
    /// A manifest is not TOML.
    #[error("{}: {source}", .path.display())]
    Manifest {
        /// The manifest.
        path: PathBuf,
        /// The reason.
        source: toml::de::Error,
    },
    /// A `mod name;` declaration whose file is neither `name.rs` nor `name/mod.rs`.
    #[error("{}: `mod {name};` has no file ({} or {})", .path.display(), .flat.display(), .nested.display())]
    NoModule {
        /// The file that declares it.
        path: PathBuf,
        /// The module's name.
        name: String,
        /// Where `name.rs` was looked for.
        flat: PathBuf,
        /// Where `name/mod.rs` was looked for.
        nested: PathBuf,
    },
    /// A `#[path]` attribute the check does not follow: one on an inline module, one
    /// inside an inline module, or one whose value is not a string literal.
    #[error("{}: `mod {name}` has a #[path] attribute that the spawn-site check does not follow (it follows a string literal on a `mod name;` outside an inline module)", .path.display())]
    PathAttribute {
        /// The file that declares the module.
        path: PathBuf,
        /// The module's name.
        name: String,
    },
    /// A module whose file a `#[cfg_attr(.., path = ..)]` chooses by target, which the check
    /// does not evaluate.
    #[error("{}: `mod {name}` has a #[cfg_attr(.., path = ..)], which the spawn-site check does not follow: name the file with a plain #[path] or a module of its own", .path.display())]
    ConditionalPath {
        /// The file that declares the module.
        path: PathBuf,
        /// The module's name.
        name: String,
    },
    /// An `include!`, which the check does not follow.
    #[error("{}: `include!` is not followed by the spawn-site check", .path.display())]
    Include {
        /// The file that calls it.
        path: PathBuf,
    },
    /// A checked crate has no source file to start from.
    #[error(
        "crates/{krate}: no crate root (no [lib] or [[bin]] path, src/lib.rs, src/main.rs or src/bin source)"
    )]
    NoRoot {
        /// The crate.
        krate: String,
    },
    /// No module of `js-mac-session` is [`SPAWNER`].
    #[error("no module of js-mac-session is {SPAWNER}, the one module that may start programs")]
    NoSpawner,
}

/// A word of a path or of a macro's tokens, and where it is.
type Word = (String, Span);

/// The directory names, under `crates/`, of the crates the check reads (see the module
/// documentation): [`START_CRATES`] and the crates they depend on through a `path`.
///
/// # Errors
/// A manifest cannot be read or parsed.
pub fn crates(root: &Path) -> Result<BTreeSet<String>, Error> {
    Ok(closure(root)?.into_keys().collect())
}

/// The manifest of each crate of [`crates`], by crate.
fn closure(root: &Path) -> Result<BTreeMap<String, toml::Table>, Error> {
    let mut manifests = BTreeMap::new();
    let mut queue: Vec<String> = START_CRATES.map(str::to_owned).to_vec();
    while let Some(name) = queue.pop() {
        if manifests.contains_key(&name) {
            continue;
        }
        let rel = PathBuf::from("crates").join(&name).join("Cargo.toml");
        let text = std::fs::read_to_string(root.join(&rel)).map_err(|source| Error::Read {
            path: rel.clone(),
            source,
        })?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|source| Error::Manifest { path: rel, source })?;
        queue.extend(path_dependencies(&table));
        manifests.insert(name, table);
    }
    Ok(manifests)
}

/// The directory names of the `path` dependencies of the manifest `table` that are built
/// for macOS: those of `[dependencies]` and of every target table but [`LINUX_ONLY`],
/// without the `optional` ones unless the crate has a non-empty `default` feature.
fn path_dependencies(table: &toml::Table) -> Vec<String> {
    let mut tables: Vec<&toml::Value> = Vec::new();
    tables.extend(table.get("dependencies"));
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for (key, target) in targets {
            if key.as_str() != LINUX_ONLY {
                tables.extend(target.get("dependencies"));
            }
        }
    }
    let defaults = table
        .get("features")
        .and_then(|features| features.get("default"))
        .and_then(toml::Value::as_array)
        .is_some_and(|default| !default.is_empty());
    let mut names = Vec::new();
    for dependencies in tables.into_iter().filter_map(toml::Value::as_table) {
        for dependency in dependencies.values() {
            let optional = dependency.get("optional").and_then(toml::Value::as_bool) == Some(true);
            let path = dependency
                .get("path")
                .and_then(toml::Value::as_str)
                .filter(|_| defaults | !optional);
            let name = path.and_then(|path| Path::new(path).file_name());
            names.extend(name.map(|name| name.to_string_lossy().into_owned()));
        }
    }
    names
}

/// The source files the module tree of the crate `name` starts from.
fn roots(root: &Path, name: &str, manifest: &toml::Table) -> Result<Vec<PathBuf>, Error> {
    let dir = root.join("crates").join(name);
    let src = dir.join("src");
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    let lib = manifest
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(toml::Value::as_str);
    if let Some(lib) = lib {
        found.insert(dir.join(lib));
    }
    if let Some(bins) = manifest.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            let path = bin.get("path").and_then(toml::Value::as_str);
            found.extend(path.map(|path| dir.join(path)));
        }
    }
    let mut implied = vec![src.join("lib.rs"), src.join("main.rs")];
    if let Ok(entries) = std::fs::read_dir(src.join("bin")) {
        for entry in entries.flatten() {
            let path = entry.path();
            implied.push(path.join("main.rs"));
            implied.push(path);
        }
    }
    for path in implied {
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            found.insert(path);
        }
    }
    if found.is_empty() {
        return Err(Error::NoRoot {
            krate: name.to_owned(),
        });
    }
    Ok(found.into_iter().collect())
}

/// Every [`Site`] of the tree at `root` (a repository checkout), in file and line order;
/// none when every program is started through [`SPAWNER`].
///
/// # Errors
/// A manifest or a source file cannot be read or parsed, a module has no file or a
/// `#[path]`, a file calls `include!`, a crate has no root, or no module is [`SPAWNER`].
pub fn check(root: &Path) -> Result<Vec<Site>, Error> {
    let mut walk = Walk {
        root,
        sites: Vec::new(),
        spawner: false,
        seen: BTreeSet::new(),
    };
    for (name, manifest) in closure(root)? {
        for path in roots(root, &name, &manifest)? {
            let children = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
            walk.file(&path, children)?;
        }
    }
    if !walk.spawner {
        return Err(Error::NoSpawner);
    }
    walk.sites.sort();
    Ok(walk.sites)
}

/// The walk over the module tree: what it found, and whether it reached the spawner.
struct Walk<'r> {
    root: &'r Path,
    sites: Vec<Site>,
    spawner: bool,
    /// The files read already: a module two roots declare is read once.
    seen: BTreeSet<PathBuf>,
}

impl Walk<'_> {
    /// Reads the module file `path`, whose child modules are in `children`.
    fn file(&mut self, path: &Path, children: PathBuf) -> Result<(), Error> {
        if path == self.root.join(SPAWNER) {
            self.spawner = true;
            return Ok(());
        }
        if !self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        let file = self.relative(path);
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: file.clone(),
            source,
        })?;
        let ast = syn::parse_file(&text).map_err(|source| Error::Parse {
            path: file.clone(),
            source,
        })?;
        let mut visitor = Visitor {
            walk: self,
            file,
            here: path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
            dirs: vec![children],
            failed: None,
        };
        visitor.visit_file(&ast);
        visitor.failed.map_or(Ok(()), Err)
    }

    /// `path` from the repository root.
    fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(self.root).unwrap_or(path).to_path_buf()
    }
}

/// The visit of one file.
struct Visitor<'w, 'r> {
    walk: &'w mut Walk<'r>,
    /// The file, from the repository root.
    file: PathBuf,
    /// The directory of the file, where a `#[path]` is relative to.
    here: PathBuf,
    /// The directory of the child modules of each module the visit is in, innermost last.
    dirs: Vec<PathBuf>,
    /// The first error of this file or of a module file it declares.
    failed: Option<Error>,
}

impl Visitor<'_, '_> {
    /// Records a [`Site`] for each sequence that starts a program in `words`.
    fn look(&mut self, words: &[Word]) {
        let names: Vec<&str> = words.iter().map(|(name, _)| name.as_str()).collect();
        let mut found: Vec<(usize, String)> = Vec::new();
        for at in 0..names.len() {
            for (sequence, what) in SEQUENCES {
                if names[at..].starts_with(sequence) {
                    found.push((at, what.to_owned()));
                }
            }
            if let ["libc", next, ..] = &names[at..] {
                let runs = EXECUTORS.iter().chain(&SHELLS).any(|name| name == next);
                if runs {
                    found.push((at, format!("libc::{next}")));
                }
            }
            let after_libc = names[..at].last() == Some(&"libc");
            let executes = EXECUTORS.iter().any(|name| *name == names[at]);
            if executes && !after_libc {
                found.push((at, names[at].to_owned()));
            }
        }
        for (at, what) in found {
            self.walk.sites.push(Site {
                file: self.file.clone(),
                line: words[at].1.start().line,
                what,
            });
        }
    }

    /// Looks at the imports of `tree` after the path `prefix`.
    fn use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<Word>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(word(&path.ident));
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) => self.use_leaf(prefix, word(&name.ident)),
            UseTree::Rename(rename) => {
                self.renamed_libc(prefix, &rename.ident);
                self.use_leaf(prefix, word(&rename.ident));
            }
            UseTree::Glob(glob) => {
                self.use_leaf(prefix, ("*".to_owned(), glob.star_token.spans[0]))
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    self.use_tree(item, prefix);
                }
            }
        }
    }

    /// Records a [`Site`] when an import renames the C library crate (`use libc as l`,
    /// `use libc::{self as l}`): under another name its `system` and `popen` would not be
    /// seen.
    fn renamed_libc(&mut self, prefix: &[Word], ident: &syn::Ident) {
        let mut full: Vec<String> = prefix.iter().map(|(name, _)| name.clone()).collect();
        full.push(ident.to_string());
        if full.last().is_some_and(|last| last == "self") {
            full.pop();
        }
        if RENAMED.iter().any(|module| full == *module) {
            self.renamed(ident.span(), &full.join("::"));
        }
    }

    /// Records the module `module` renamed at `span`.
    fn renamed(&mut self, span: Span, module: &str) {
        self.walk.sites.push(Site {
            file: self.file.clone(),
            line: span.start().line,
            what: format!("{module} renamed"),
        });
    }

    /// Looks at the full path of one import: `prefix` and the name `last`.
    fn use_leaf(&mut self, prefix: &[Word], last: Word) {
        let mut path = prefix.to_vec();
        path.push(last);
        self.look(&path);
    }

    /// Keeps `error` unless an earlier one is kept already.
    fn fail(&mut self, error: Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    /// Fails with a `#[path]` the check does not follow, on the module `name`.
    fn refuse_path(&mut self, name: String) {
        let error = Error::PathAttribute {
            path: self.file.clone(),
            name,
        };
        self.fail(error);
    }

    /// The file of the module `name;` and the directory of its child modules: `name.rs` or
    /// `name/mod.rs` (`children`). A missing file is an error.
    fn locate(&mut self, name: String, children: PathBuf) -> Option<(PathBuf, PathBuf)> {
        let flat = self.dir().join(format!("{name}.rs"));
        let nested = children.join("mod.rs");
        let found = [&flat, &nested]
            .into_iter()
            .find(|path| path.is_file())
            .cloned();
        if found.is_none() {
            let error = Error::NoModule {
                path: self.file.clone(),
                name,
                flat: self.walk.relative(&flat),
                nested: self.walk.relative(&nested),
            };
            self.fail(error);
        }
        found.map(|path| (path, children))
    }

    /// The file of the module `name;` that has the attribute `attr`, and the directory of
    /// its child modules (its own, as for a `mod.rs`). Only a string literal on a module
    /// outside an inline module is followed; any other `#[path]` is an error.
    fn locate_path(&mut self, attr: &Attribute, name: String) -> Option<(PathBuf, PathBuf)> {
        let followed = path_literal(attr).filter(|_| self.dirs.len() == 1);
        let Some(text) = followed else {
            self.refuse_path(name);
            return None;
        };
        let file = self.here.join(text);
        let below = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        Some((file, below))
    }

    /// The directory the child modules of the innermost module are in.
    fn dir(&self) -> &Path {
        self.dirs.last().map_or(Path::new(""), PathBuf::as_path)
    }
}

/// An identifier as a word.
fn word(ident: &syn::Ident) -> Word {
    (ident.to_string(), ident.span())
}

/// The words of a macro's tokens, flattened (see [`SEQUENCES`]), after the path `prefix`
/// they belong to. A braced list after a `::` is the list of an import: each of its
/// comma-separated parts is read after the path that precedes the list.
fn words(tokens: TokenStream, prefix: &[Word], into: &mut Vec<Word>) {
    into.extend(prefix.iter().cloned());
    let mut run: Vec<Word> = prefix.to_vec();
    let mut colons = !prefix.is_empty();
    for tree in tokens {
        match tree {
            TokenTree::Punct(punct) => {
                if punct.as_char() == ':' {
                    colons = true;
                } else {
                    into.push((punct.to_string(), punct.span()));
                    run.clear();
                    colons = false;
                }
            }
            TokenTree::Group(group) => {
                if colons && group.delimiter() == Delimiter::Brace {
                    into.truncate(into.len() - run.len());
                    for part in comma_parts(group.stream()) {
                        words(part, &run, into);
                    }
                } else {
                    words(group.stream(), &[], into);
                }
                run.clear();
                colons = false;
            }
            TokenTree::Ident(ident) => {
                if !colons {
                    run.clear();
                }
                run.push(word(&ident));
                into.push(word(&ident));
                colons = false;
            }
            TokenTree::Literal(literal) => {
                into.push((literal.to_string(), literal.span()));
                run.clear();
                colons = false;
            }
        }
    }
}

/// `tokens` cut at its commas.
fn comma_parts(tokens: TokenStream) -> Vec<TokenStream> {
    let mut parts = Vec::new();
    let mut current = TokenStream::new();
    for tree in tokens {
        let comma = matches!(&tree, TokenTree::Punct(punct) if punct.as_char() == ',');
        if comma {
            parts.push(std::mem::take(&mut current));
        } else {
            current.extend([tree]);
        }
    }
    parts.push(current);
    parts
}

/// Whether `attr` is a `#[cfg_attr(..)]` that may set the module's `path`, at any depth
/// (a `cfg_attr` nested in another holds its attributes in a group): the file a build
/// reads would then depend on the target, which the check does not evaluate.
fn sets_path(attr: &Attribute) -> bool {
    let Meta::List(list) = &attr.meta else {
        return false;
    };
    list.path.is_ident("cfg_attr") && names_path(list.tokens.clone())
}

/// Whether `tokens`, or a group in them at any depth, hold the identifier `path`.
fn names_path(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|tree| match tree {
        TokenTree::Ident(ident) => ident == "path",
        TokenTree::Group(group) => names_path(group.stream()),
        TokenTree::Punct(_) | TokenTree::Literal(_) => false,
    })
}

/// The string literal of a `#[path = "..."]` attribute, if it is one.
fn path_literal(attr: &Attribute) -> Option<String> {
    let Meta::NameValue(pair) = &attr.meta else {
        return None;
    };
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(text),
        ..
    }) = &pair.value
    else {
        return None;
    };
    Some(text.value())
}

/// Whether `attrs` make the item exist only in a test build: a `#[cfg(test)]`, or a
/// `#[cfg(all(...))]` that holds `test`.
fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| needs_test(&meta))
    })
}

/// Whether the cfg predicate `meta` holds only in a test build.
fn needs_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|all| all.iter().any(needs_test)),
        _ => false,
    }
}

impl<'ast> Visit<'ast> for Visitor<'_, '_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if test_only(&item.attrs) {
            return;
        }
        let name = item.ident.to_string();
        let children = self.dir().join(&name);
        if item.attrs.iter().any(sets_path) {
            let error = Error::ConditionalPath {
                path: self.file.clone(),
                name,
            };
            self.fail(error);
            return;
        }
        let path_attr = item.attrs.iter().find(|attr| attr.path().is_ident("path"));
        if item.content.is_some() {
            if path_attr.is_some() {
                self.refuse_path(name);
                return;
            }
            self.dirs.push(children);
            visit::visit_item_mod(self, item);
            self.dirs.pop();
            return;
        }
        let located = match path_attr {
            None => self.locate(name, children),
            Some(attr) => self.locate_path(attr, name),
        };
        let Some((path, below)) = located else {
            return;
        };
        if let Err(error) = self.walk.file(&path, below) {
            self.fail(error);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_item_fn(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !test_only(&item.attrs) {
            visit::visit_item_impl(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if !test_only(&item.attrs) {
            self.use_tree(&item.tree, &mut Vec::new());
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let words: Vec<_> = path
            .segments
            .iter()
            .map(|segment| word(&segment.ident))
            .collect();
        self.look(&words);
        visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let dot = (".".to_owned(), call.method.span());
        self.look(&[dot, word(&call.method)]);
        visit::visit_expr_method_call(self, call);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if item.ident == "libc" && item.rename.is_some() {
            self.renamed(item.ident.span(), "libc");
        }
        visit::visit_item_extern_crate(self, item);
    }

    fn visit_foreign_item_fn(&mut self, item: &'ast syn::ForeignItemFn) {
        // A foreign function is the C library's: named as `libc::` would name it.
        let libc = ("libc".to_owned(), item.sig.ident.span());
        self.look(&[libc, word(&item.sig.ident)]);
        visit::visit_foreign_item_fn(self, item);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|last| last.ident == "include")
        {
            self.fail(Error::Include {
                path: self.file.clone(),
            });
        }
        let mut found = Vec::new();
        words(mac.tokens.clone(), &[], &mut found);
        self.look(&found);
        visit::visit_macro(self, mac);
    }
}
