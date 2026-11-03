//! The spawn-site check: `js-agent` and `js-mac-session` start every program through one
//! module, `js_mac_session::disclaim`, which on macOS starts it with its privacy (TCC)
//! responsibility disclaimed (that module's documentation says why). The check is used by
//! the `spawn_sites` test rather than a verb: `cargo test -p xtask --test spawn_sites`.
//!
//! [`check`] parses (with `syn`) the production source of the two crates ([`CRATES`]): the
//! module tree from each crate's roots (`src/lib.rs`, `src/main.rs`), following each
//! `mod name;` to `name.rs` or `name/mod.rs`. It leaves out what only a test build
//! compiles: a module, function, `impl` block or method under `#[cfg(test)]`, or under a
//! `#[cfg(all(...))]` that holds `test`. It also leaves out the spawner itself
//! ([`SPAWNER`]). Code built for one platform only is not left out. On every other
//! platform the spawner is the standard library's `Command` unchanged, so one rule holds
//! everywhere and the check needs to know nothing about targets.
//!
//! In what is left, each of these is a [`Site`], the place where a program could start
//! without the disclaim:
//!
//! - a path that ends in `Command::new` (the standard library's command, or tokio's);
//! - a call of a method named `pre_exec` or `exec` (`CommandExt`'s ways to run code before
//!   a program, or to execute one in the caller's place);
//! - a path, an import or a foreign function named for a C library function that executes
//!   a program ([`EXECUTORS`]), or `libc::system` and `libc::popen`, which run a shell;
//! - an import that renames `Command`, which would hide a `Command::new`.
//!
//! The same sequences are looked for in the tokens of every macro call, which `syn` does
//! not parse. A `mod` declaration whose file is missing, a `#[path]` attribute (which the
//! check does not follow), a crate with no root and a spawner that no module declares are
//! errors, never a part of the tree skipped in silence.

use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Meta, Token};

/// The crates whose production code the check reads.
pub const CRATES: [&str; 2] = ["js-agent", "js-mac-session"];

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

/// The crate roots the module tree starts from, in `src/`.
const ROOTS: [&str; 2] = ["lib.rs", "main.rs"];

/// The word sequences that start a program, with the name a [`Site`] gives each, besides
/// one word for each of [`EXECUTORS`]. A path's words are its segments; a method call's
/// are `.` and the method; a macro's are its tokens, without the `:` of a `::`.
const SEQUENCES: [(&[&str], &str); 5] = [
    (&["Command", "new"], "Command::new"),
    (&[".", "pre_exec"], ".pre_exec"),
    (&[".", "exec"], ".exec"),
    (&["libc", "system"], "libc::system"),
    (&["libc", "popen"], "libc::popen"),
];

/// A place in the production code of [`CRATES`] that starts a program without the spawner.
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
    /// A source file could not be read.
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
    /// A `#[path]` attribute, which the check does not follow.
    #[error("{}: `mod {name}` has a #[path] attribute, which the spawn-site check does not follow", .path.display())]
    PathAttribute {
        /// The file that declares the module.
        path: PathBuf,
        /// The module's name.
        name: String,
    },
    /// One of [`CRATES`] has neither root.
    #[error("crates/{krate}/src has neither lib.rs nor main.rs")]
    NoRoot {
        /// The crate.
        krate: String,
    },
    /// No module of `js-mac-session` is [`SPAWNER`].
    #[error("no module of js-mac-session is {SPAWNER}, the one module that may start programs")]
    NoSpawner,
}

/// Every [`Site`] of the tree at `root` (a repository checkout), in file and line order;
/// none when every program is started through [`SPAWNER`].
///
/// # Errors
/// A file cannot be read or parsed, a module has no file or a `#[path]`, a crate has no
/// root, or no module is [`SPAWNER`].
pub fn check(root: &Path) -> Result<Vec<Site>, Error> {
    let mut walk = Walk {
        root,
        sites: Vec::new(),
        spawner: false,
    };
    for krate in CRATES {
        let src = root.join("crates").join(krate).join("src");
        let roots: Vec<PathBuf> = ROOTS
            .iter()
            .map(|name| src.join(name))
            .filter(|path| path.is_file())
            .collect();
        if roots.is_empty() {
            return Err(Error::NoRoot {
                krate: krate.to_owned(),
            });
        }
        for path in roots {
            walk.file(&path, src.clone())?;
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
}

impl Walk<'_> {
    /// Reads the module file `path`, whose child modules are in `children`.
    fn file(&mut self, path: &Path, children: PathBuf) -> Result<(), Error> {
        if path == self.root.join(SPAWNER) {
            self.spawner = true;
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
    /// The directory of the child modules of each module the visit is in, innermost last.
    dirs: Vec<PathBuf>,
    /// The first error of this file or of a module file it declares.
    failed: Option<Error>,
}

impl Visitor<'_, '_> {
    /// Records a [`Site`] for each sequence that starts a program in `words`.
    fn look(&mut self, words: &[(String, Span)]) {
        let singles = EXECUTORS.map(|name| ([name], name));
        let sequences = SEQUENCES.iter().copied().chain(
            singles
                .iter()
                .map(|(sequence, what)| (&sequence[..], *what)),
        );
        for (sequence, what) in sequences {
            for window in words.windows(sequence.len()) {
                if window
                    .iter()
                    .map(|(word, _)| word.as_str())
                    .eq(sequence.iter().copied())
                {
                    self.walk.sites.push(Site {
                        file: self.file.clone(),
                        line: window[0].1.start().line,
                        what: what.to_owned(),
                    });
                }
            }
        }
    }

    /// Keeps `error` unless an earlier one is kept already.
    fn fail(&mut self, error: Error) {
        if self.failed.is_none() {
            self.failed = Some(error);
        }
    }

    /// The directory the child modules of the innermost module are in.
    fn dir(&self) -> &Path {
        self.dirs.last().map_or(Path::new(""), PathBuf::as_path)
    }
}

/// An identifier as a word.
fn word(ident: &syn::Ident) -> (String, Span) {
    (ident.to_string(), ident.span())
}

/// The words of a macro's tokens, flattened (see [`SEQUENCES`]).
fn words(tokens: TokenStream, into: &mut Vec<(String, Span)>) {
    for tree in tokens {
        match tree {
            TokenTree::Group(group) => words(group.stream(), into),
            TokenTree::Punct(punct) if punct.as_char() == ':' => {}
            other => into.push((other.to_string(), other.span())),
        }
    }
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
        if item.content.is_some() {
            self.dirs.push(children);
            visit::visit_item_mod(self, item);
            self.dirs.pop();
            return;
        }
        if item.attrs.iter().any(|attr| attr.path().is_ident("path")) {
            self.fail(Error::PathAttribute {
                path: self.file.clone(),
                name,
            });
            return;
        }
        let flat = self.dir().join(format!("{name}.rs"));
        let nested = children.join("mod.rs");
        let found = [&flat, &nested]
            .into_iter()
            .find(|path| path.is_file())
            .cloned();
        let Some(path) = found else {
            let error = Error::NoModule {
                path: self.file.clone(),
                name,
                flat: self.walk.relative(&flat),
                nested: self.walk.relative(&nested),
            };
            self.fail(error);
            return;
        };
        if let Err(error) = self.walk.file(&path, children) {
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

    fn visit_foreign_item_fn(&mut self, item: &'ast syn::ForeignItemFn) {
        // A foreign function is the C library's: named as `libc::` would name it.
        let libc = ("libc".to_owned(), item.sig.ident.span());
        self.look(&[libc, word(&item.sig.ident)]);
        visit::visit_foreign_item_fn(self, item);
    }

    fn visit_use_name(&mut self, name: &'ast syn::UseName) {
        self.look(&[word(&name.ident)]);
    }

    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        if rename.ident == "Command" {
            self.walk.sites.push(Site {
                file: self.file.clone(),
                line: rename.ident.span().start().line,
                what: format!("Command imported as {}", rename.rename),
            });
        }
        self.look(&[word(&rename.ident)]);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let mut found = Vec::new();
        words(mac.tokens.clone(), &mut found);
        self.look(&found);
        visit::visit_macro(self, mac);
    }
}
