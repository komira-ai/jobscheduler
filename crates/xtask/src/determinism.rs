//! The determinism lint of the simulation cell: no clock or random source is read in the
//! crates a `js-sim-cell` run links, except at the sites [`ALLOWED`] names, each with its
//! reason. A run of the cell is a function of its seed (docs/simulation.md): a stray
//! `Instant::now` or `getrandom` in that code makes a reported seed replay differently, or
//! not fail again at all. The lint is the verb `cargo xtask determinism` and the test
//! `cargo test -p xtask --test determinism`, which runs it on this tree.
//!
//! **Which crates.** [`CRATES`], by their directory under `crates/`: `js-sim-cell` and every
//! workspace crate it depends on through `[dependencies]` (and target dependency tables),
//! transitively. The list is written out here rather than found by a walk of the manifests,
//! so a new dependency of the cell is a reviewed change to this file; the test
//! `the_crate_list_is_every_crate_the_sim_cell_links` fails when the list and the cell's
//! dependencies, as `cargo metadata` resolves them, differ.
//!
//! **Which files.** Every `.rs` file under each crate's `src/`, parsed with `syn`, except
//! what only a test build compiles: an item under `#[cfg(test)]` (or a `#[cfg(all(..))]`
//! that holds `test`), and the file of a module declared so (`#[cfg(test)] mod tests;`),
//! with the files of its own child modules. Comments, doc comments and string literals are
//! not code and are not read. Code built for one platform only is read like any other.
//!
//! **What is a site.** [`Token`]: the words of a path (an expression, a type or an
//! import, each leaf of a `use` tree read with its full prefix) or of a macro call's tokens,
//! looked at in sequence: `SystemTime` then `now`, `Instant` then `now`, `Uuid` then
//! `new_v4`, and the word `getrandom` or `thread_rng` anywhere. A site is named by its
//! file, its line, the token, and its item: the names of the items it is inside, outermost
//! first and joined by `::` (an inline module, an `impl` block by the last word of its type
//! with any `&` taken off, or `impl` for a type that is not a path, a trait, a function, a
//! constant or a static), such as `SystemClock::now`; empty at the top level of the file. A
//! qualified path is read with its type first: `<Instant>::now` and `<Instant as T>::now`
//! are `Instant` then `now`.
//!
//! **What it does not see.** A name brought in under another name (`use std::time::Instant
//! as Clock;`, `type Clock = Instant;`) is looked for under its own name only; the code of
//! crates outside the workspace is not read; and clocks and random sources other than the
//! five tokens (`rand::random`, `Utc::now`, a `HashMap`'s random hasher) are not looked for.
//! The cell's own crate keeps clippy's bans on `Instant::now`, `SystemTime::now` and
//! `HashMap` (`clippy.toml`).
//!
//! **The allow-list.** A site passes when an [`Allowed`] entry names its file, item and
//! token; the entry also says how many sites (lines) it allows, so a second read added to an
//! allowed function is not allowed with the first. [`check`] fails listing every site no
//! entry allows (`file:line`, the token and the item), every entry that allows no site
//! (stale: the call moved or went, so the entry goes too), every entry whose count is not
//! the number of its sites, and every entry with no reason.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Attribute, UseTree};

use crate::spawn_sites::test_only;

/// The crates the lint reads, by their directory under `crates/`: `js-sim-cell` and the
/// workspace crates it links (see the module documentation).
pub const CRATES: &[&str] = &[
    "js-agent",
    "js-agent-usage",
    "js-agent-wire",
    "js-alert",
    "js-api-gateway",
    "js-bundle",
    "js-caps",
    "js-client",
    "js-conformance",
    "js-contain",
    "js-elastic",
    "js-jobs",
    "js-lease",
    "js-lease-server",
    "js-logmint",
    "js-logstore",
    "js-mac-pool",
    "js-mac-provision",
    "js-mac-session",
    "js-oci",
    "js-platform",
    "js-pool-static",
    "js-proto",
    "js-provider",
    "js-provider-lambdalabs",
    "js-registry",
    "js-scaler",
    "js-sched",
    "js-sim",
    "js-sim-cell",
    "js-sizing",
    "js-state",
    "js-testkit",
    "js-treestore",
    "js-types",
    "js-usage",
    "js-whole",
];

/// A clock or random source the lint looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Token {
    /// `SystemTime::now`: the wall clock.
    SystemTimeNow,
    /// `Instant::now`: the monotonic clock.
    InstantNow,
    /// `getrandom`: the operating system's random source.
    Getrandom,
    /// `thread_rng`: `rand`'s random source seeded from the operating system.
    ThreadRng,
    /// `Uuid::new_v4`: a random UUID.
    UuidNewV4,
}

impl Token {
    /// Every token, in the order the module documentation lists them.
    pub const ALL: [Token; 5] = [
        Token::SystemTimeNow,
        Token::InstantNow,
        Token::Getrandom,
        Token::ThreadRng,
        Token::UuidNewV4,
    ];

    /// The token as code writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Token::SystemTimeNow => "SystemTime::now",
            Token::InstantNow => "Instant::now",
            Token::Getrandom => "getrandom",
            Token::ThreadRng => "thread_rng",
            Token::UuidNewV4 => "Uuid::new_v4",
        }
    }

    /// The token a run of words starts with, if any.
    fn at(words: &[Word]) -> Option<Token> {
        let first = words.first()?.0.as_str();
        let second = words.get(1).map(|word| word.0.as_str());
        match (first, second) {
            ("getrandom", _) => Some(Token::Getrandom),
            ("thread_rng", _) => Some(Token::ThreadRng),
            ("SystemTime", Some("now")) => Some(Token::SystemTimeNow),
            ("Instant", Some("now")) => Some(Token::InstantNow),
            ("Uuid", Some("new_v4")) => Some(Token::UuidNewV4),
            _ => None,
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One allowed site: its file, item and token, and why it may read the clock or the
/// random source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allowed {
    /// The file, from the repository root.
    pub file: &'static str,
    /// The item the site is inside (see the module documentation), such as
    /// `SystemClock::now`.
    pub item: &'static str,
    /// What it reads.
    pub token: Token,
    /// How many sites (lines of the item naming the token) it allows.
    pub sites: usize,
    /// Why a run of the cell never reaches it, or reaching it decides nothing.
    pub reason: &'static str,
}

impl Allowed {
    /// Whether this entry allows `site`.
    fn allows(&self, site: &Site) -> bool {
        Path::new(self.file) == site.file && self.item == site.item && self.token == site.token
    }
}

/// A place in the read code that names a [`Token`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// The file, from the repository root.
    pub file: PathBuf,
    /// The line, from 1.
    pub line: usize,
    /// The item it is inside; empty at the top level of the file.
    pub item: String,
    /// What it names.
    pub token: Token,
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (file, line, token) = (self.file.display(), self.line, self.token);
        if self.item.is_empty() {
            write!(f, "{file}:{line}: {token} at the top level of the file")
        } else {
            write!(f, "{file}:{line}: {token} in {}", self.item)
        }
    }
}

/// What the lint found wrong with the tree or the allow-list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// A site no entry allows.
    NotAllowed(Site),
    /// An entry that allows no site.
    Stale(Allowed),
    /// An entry with an empty reason.
    NoReason(Allowed),
    /// An entry that allows another number of sites than it says.
    Count {
        /// The entry.
        entry: Allowed,
        /// How many sites it matches.
        found: usize,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::NotAllowed(site) => write!(f, "{site} is not on the allow-list"),
            Problem::Stale(entry) => write!(
                f,
                "{}: the allow-list entry for {} in {} matches no site: remove it",
                entry.file, entry.token, entry.item
            ),
            Problem::NoReason(entry) => write!(
                f,
                "{}: the allow-list entry for {} in {} gives no reason",
                entry.file, entry.token, entry.item
            ),
            Problem::Count { entry, found } => write!(
                f,
                "{}: the allow-list entry for {} in {} allows {} sites, and there are {found}: \
                 each site needs its reason",
                entry.file, entry.token, entry.item, entry.sites
            ),
        }
    }
}

/// Why the lint failed. Each path is from the repository root.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A listed crate has no `src/` directory.
    #[error("crates/{krate}/src: no such directory (a crate of the determinism lint's list)")]
    NoCrate {
        /// The crate's directory under `crates/`.
        krate: String,
    },
    /// A directory or a source file could not be read.
    #[error("{}: {source}", .path.display())]
    Read {
        /// The directory or file.
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
    /// The tree or the allow-list is wrong, every way it is, in order.
    #[error("{}", lines(.0))]
    Found(Vec<Problem>),
}

/// The problems, one a line.
fn lines(problems: &[Problem]) -> String {
    let lines: Vec<String> = problems.iter().map(Problem::to_string).collect();
    lines.join("\n")
}

/// Every site in the read code of `crates` (directories under `crates/` of `root`), in
/// file and line order.
///
/// # Errors
///
/// A crate has no `src/`, or a file cannot be read or parsed.
pub fn sites(root: &Path, crates: &[&str]) -> Result<Vec<Site>, Error> {
    let mut files = Vec::new();
    for krate in crates {
        let src = root.join("crates").join(krate).join("src");
        if !src.is_dir() {
            let krate = (*krate).to_owned();
            return Err(Error::NoCrate { krate });
        }
        rust_files(root, &src, &mut files)?;
    }
    let mut found = Vec::new();
    let mut tests = Tests::default();
    for file in &files {
        let text = std::fs::read_to_string(root.join(file)).map_err(|source| Error::Read {
            path: file.clone(),
            source,
        })?;
        let ast = syn::parse_file(&text).map_err(|source| Error::Parse {
            path: file.clone(),
            source,
        })?;
        let mut visitor = Visitor {
            file,
            items: Vec::new(),
            dirs: vec![children(file)],
            sites: &mut found,
            tests: &mut tests,
        };
        visitor.visit_file(&ast);
    }
    found.retain(|site| !tests.holds(&site.file));
    found.sort();
    found.dedup();
    Ok(found)
}

/// Runs the lint over `crates` of `root` with `allowed`: the number of sites, each
/// allowed.
///
/// # Errors
///
/// [`Error::Found`] with every site no entry allows, every entry that allows no site and
/// every entry whose count is not its number of sites, and every entry with no reason; or
/// the error of [`sites`].
pub fn check(root: &Path, crates: &[&str], allowed: &[Allowed]) -> Result<usize, Error> {
    let found = sites(root, crates)?;
    let mut problems: Vec<Problem> = found
        .iter()
        .filter(|site| !allowed.iter().any(|entry| entry.allows(site)))
        .cloned()
        .map(Problem::NotAllowed)
        .collect();
    for entry in allowed {
        let matched = found.iter().filter(|site| entry.allows(site)).count();
        if matched == 0 {
            problems.push(Problem::Stale(*entry));
        } else if matched != entry.sites {
            let entry = *entry;
            problems.push(Problem::Count {
                entry,
                found: matched,
            });
        }
        if entry.reason.trim().is_empty() {
            problems.push(Problem::NoReason(*entry));
        }
    }
    if problems.is_empty() {
        Ok(found.len())
    } else {
        Err(Error::Found(problems))
    }
}

/// The verb: [`check`] over [`CRATES`] of the tree at `root` with [`ALLOWED`].
///
/// # Errors
///
/// As [`check`].
pub fn run(root: &Path) -> Result<String, Error> {
    let count = check(root, CRATES, ALLOWED)?;
    Ok(format!(
        "determinism: {count} clock and random-source sites in {} crates, each on the allow-list",
        CRATES.len()
    ))
}

/// Adds every `.rs` file under `dir`, from `root`, to `into`, in name order.
fn rust_files(root: &Path, dir: &Path, into: &mut Vec<PathBuf>) -> Result<(), Error> {
    let relative = |path: &Path| path.strip_prefix(root).unwrap_or(path).to_path_buf();
    let unread = |source| Error::Read {
        path: relative(dir),
        source,
    };
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(unread)? {
        entries.push(entry.map_err(unread)?.path());
    }
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(root, &path, into)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(relative(&path));
        }
    }
    Ok(())
}

/// The directory of the child modules of the module file `file`: its own directory for a
/// `lib.rs`, `main.rs` or `mod.rs`, else the directory named as the file.
fn children(file: &Path) -> PathBuf {
    let parent = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    let own = ["lib.rs", "main.rs", "mod.rs"];
    match file.file_name().and_then(|name| name.to_str()) {
        Some(name) if own.contains(&name) => parent,
        _ => file.with_extension(""),
    }
}

/// The files of modules only a test build compiles, and the directories of their child
/// modules.
#[derive(Default)]
struct Tests {
    files: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
}

impl Tests {
    /// Marks the module file `file` as test-only.
    fn add(&mut self, file: PathBuf) {
        self.dirs.insert(children(&file));
        self.files.insert(file);
    }

    /// Whether `file` is a test-only module's, or one of its child modules'.
    fn holds(&self, file: &Path) -> bool {
        self.files.contains(file) || self.dirs.iter().any(|dir| file.starts_with(dir))
    }
}

/// A word of a path or of a macro's tokens, and where it is.
type Word = (String, Span);

/// The visit of one file.
struct Visitor<'v> {
    /// The file, from the repository root.
    file: &'v Path,
    /// The names of the items the visit is inside, outermost first.
    items: Vec<String>,
    /// The directory of the child modules of each module the visit is in, innermost last.
    dirs: Vec<PathBuf>,
    sites: &'v mut Vec<Site>,
    tests: &'v mut Tests,
}

impl Visitor<'_> {
    /// Records a site for each token a run of `words` starts with.
    fn look(&mut self, words: &[Word]) {
        for start in 0..words.len() {
            if let Some(token) = Token::at(&words[start..]) {
                self.sites.push(Site {
                    file: self.file.to_path_buf(),
                    line: words[start].1.start().line,
                    item: self.items.join("::"),
                    token,
                });
            }
        }
    }

    /// Visits `walk` inside the item `name`.
    fn inside(&mut self, name: String, walk: impl FnOnce(&mut Self)) {
        self.items.push(name);
        walk(self);
        self.items.pop();
    }

    /// Looks at a qualified path, `<T>::a` or `<T as U>::a`, as the last word of `T` followed
    /// by the segments after the qualifier. A path with no qualifier is left to `visit_path`.
    fn qualified(&mut self, qself: Option<&syn::QSelf>, path: &syn::Path) {
        let Some(qself) = qself else {
            return;
        };
        let rest = path.segments.iter().skip(qself.position);
        let words: Vec<Word> = last_word(&qself.ty)
            .into_iter()
            .chain(rest.map(|s| word(&s.ident)))
            .collect();
        self.look(&words);
    }

    /// Looks at each leaf of the import `tree`, after the words of `prefix`.
    fn use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<Word>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(word(&path.ident));
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) => self.use_leaf(prefix, word(&name.ident)),
            UseTree::Rename(rename) => self.use_leaf(prefix, word(&rename.ident)),
            UseTree::Glob(_) => self.look(prefix),
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(tree, prefix);
                }
            }
        }
    }

    /// Looks at the import of `last` after `prefix`.
    fn use_leaf(&mut self, prefix: &[Word], last: Word) {
        let mut words = prefix.to_vec();
        words.push(last);
        self.look(&words);
    }

    /// Marks the file of every `mod name;` among `items`, a test-only inline module's whose
    /// child modules are in `dir`, as test-only, at any depth.
    fn test_modules(&mut self, dir: PathBuf, items: &[syn::Item]) {
        for item in items {
            let syn::Item::Mod(module) = item else {
                continue;
            };
            let name = module.ident.to_string();
            match &module.content {
                Some((_, inner)) => self.test_modules(dir.join(&name), inner),
                None => {
                    self.tests.add(dir.join(format!("{name}.rs")));
                    self.tests.add(dir.join(&name).join("mod.rs"));
                }
            }
        }
    }

    /// The directory the child modules of the innermost module are in.
    fn dir(&self) -> PathBuf {
        self.dirs.last().cloned().unwrap_or_default()
    }
}

/// The last word of the path a type is, under any `&`: `Wall` for `&mut a::Wall`.
fn last_word(ty: &syn::Type) -> Option<Word> {
    match ty {
        syn::Type::Path(path) => path.path.segments.last().map(|s| word(&s.ident)),
        syn::Type::Reference(reference) => last_word(&reference.elem),
        _ => None,
    }
}

/// An identifier as a word.
fn word(ident: &syn::Ident) -> Word {
    (ident.to_string(), ident.span())
}

/// The runs of words in `tokens`, at any depth: identifiers joined by `::`, each run
/// ended by any other token.
fn runs(tokens: TokenStream, into: &mut Vec<Vec<Word>>) {
    let mut run: Vec<Word> = Vec::new();
    let mut joined = false;
    for tree in tokens {
        match tree {
            TokenTree::Ident(ident) => {
                if !joined {
                    into.push(std::mem::take(&mut run));
                }
                run.push((ident.to_string(), ident.span()));
                joined = false;
            }
            TokenTree::Punct(punct) if punct.as_char() == ':' => joined = true,
            TokenTree::Group(group) => {
                into.push(std::mem::take(&mut run));
                runs(group.stream(), into);
                joined = false;
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {
                into.push(std::mem::take(&mut run));
                joined = false;
            }
        }
    }
    into.push(run);
}

/// The string literal of a `#[path = "..."]` attribute among `attrs`, if there is one.
fn path_attribute(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| match &attr.meta {
        syn::Meta::NameValue(syn::MetaNameValue {
            path,
            value:
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(text),
                    ..
                }),
            ..
        }) if path.is_ident("path") => Some(text.value()),
        _ => None,
    })
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let name = item.ident.to_string();
        if test_only(&item.attrs) {
            if let Some((_, items)) = &item.content {
                self.test_modules(self.dir().join(&name), items);
            } else {
                let here = self
                    .file
                    .parent()
                    .map_or_else(PathBuf::new, Path::to_path_buf);
                let files = match path_attribute(&item.attrs) {
                    Some(path) => vec![here.join(path)],
                    None => vec![
                        self.dir().join(format!("{name}.rs")),
                        self.dir().join(&name).join("mod.rs"),
                    ],
                };
                for file in files {
                    self.tests.add(file);
                }
            }
            return;
        }
        let dir = self.dir().join(&name);
        self.dirs.push(dir);
        self.inside(name, |v| visit::visit_item_mod(v, item));
        self.dirs.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if !test_only(&item.attrs) {
            let name = item.sig.ident.to_string();
            self.inside(name, |v| visit::visit_item_fn(v, item));
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if test_only(&item.attrs) {
            return;
        }
        let name = last_word(&item.self_ty).map_or_else(|| "impl".to_owned(), |w| w.0);
        self.inside(name, |v| visit::visit_item_impl(v, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if !test_only(&item.attrs) {
            let name = item.sig.ident.to_string();
            self.inside(name, |v| visit::visit_impl_item_fn(v, item));
        }
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        if !test_only(&item.attrs) {
            let name = item.ident.to_string();
            self.inside(name, |v| visit::visit_item_trait(v, item));
        }
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if !test_only(&item.attrs) {
            let name = item.sig.ident.to_string();
            self.inside(name, |v| visit::visit_trait_item_fn(v, item));
        }
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if !test_only(&item.attrs) {
            let name = item.ident.to_string();
            self.inside(name, |v| visit::visit_item_const(v, item));
        }
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        if !test_only(&item.attrs) {
            let name = item.ident.to_string();
            self.inside(name, |v| visit::visit_item_static(v, item));
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if !test_only(&item.attrs) {
            self.use_tree(&item.tree, &mut Vec::new());
        }
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        self.qualified(expr.qself.as_ref(), &expr.path);
        visit::visit_expr_path(self, expr);
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        self.qualified(ty.qself.as_ref(), &ty.path);
        visit::visit_type_path(self, ty);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let words: Vec<Word> = path.segments.iter().map(|s| word(&s.ident)).collect();
        self.look(&words);
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let mut found = Vec::new();
        runs(mac.tokens.clone(), &mut found);
        for run in found {
            self.look(&run);
        }
        visit::visit_macro(self, mac);
    }
}

/// The allowed sites: the production clocks and random sources of the crates the cell
/// links, each behind a port the cell fills with its own, or on a path a run of the cell
/// never takes.
pub const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "crates/js-agent/src/backoff.rs",
        item: "SystemJitter::up_to",
        token: Token::Getrandom,
        sites: 1,
        reason: "the agent's reconnect jitter behind the Jitter trait; the cell drives the agent lifecycle, which never reconnects through it",
    },
    Allowed {
        file: "crates/js-agent/src/cli_runtime/mod.rs",
        item: "unix_ms",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the Docker CLI runtime's wall clock, passed in as a function; the cell's real agents run the in-memory runtime",
    },
    Allowed {
        file: "crates/js-agent/src/cordon/mod.rs",
        item: "SystemPace::new",
        token: Token::InstantNow,
        sites: 1,
        reason: "the cordon daemon's clock behind the Pace trait; the cell runs no cordon",
    },
    Allowed {
        file: "crates/js-agent/src/crun/mod.rs",
        item: "system_now_ms",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the crun runtime's wall clock, the default of its Seams; the cell's real agents run the in-memory runtime",
    },
    Allowed {
        file: "crates/js-agent/src/native/mod.rs",
        item: "system_clock",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the native runtime's wall clock, set by NativeRuntime::open; the cell's real agents run the in-memory runtime",
    },
    Allowed {
        file: "crates/js-agent/src/quota/xfs.rs",
        item: "probe_name",
        token: Token::Getrandom,
        sites: 1,
        reason: "names the XFS quota probe's directory; the cell configures no XFS quota",
    },
    Allowed {
        file: "crates/js-agent/src/system.rs",
        item: "native_runtime",
        token: Token::Getrandom,
        sites: 1,
        reason: "the native runtime's lease-user term; the cell's real agents run the in-memory runtime",
    },
    Allowed {
        file: "crates/js-agent/src/system.rs",
        item: "System::new",
        token: Token::InstantNow,
        sites: 1,
        reason: "the agent program's clock; the cell gives the agent lifecycle its virtual clock in place of System",
    },
    Allowed {
        file: "crates/js-agent/src/update_hook/ports.rs",
        item: "SystemClock::now_ms",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the update hook's wall clock behind its Clock port; the cell runs no update hook",
    },
    Allowed {
        file: "crates/js-agent/src/usage/clock.rs",
        item: "MonotonicClock::new",
        token: Token::InstantNow,
        sites: 1,
        reason: "the agent's usage clock behind UsageClock; the cell's agents record no usage with it",
    },
    Allowed {
        file: "crates/js-contain/src/oci/crun.rs",
        item: "wait_exited",
        token: Token::InstantNow,
        sites: 2,
        reason: "waits for a real crun child to be reaped; the cell starts no container",
    },
    Allowed {
        file: "crates/js-contain/src/oci/crun.rs",
        item: "wait_gone",
        token: Token::InstantNow,
        sites: 1,
        reason: "the real clock given to wait_gone_by on crun's delete path; the cell starts no container",
    },
    Allowed {
        file: "crates/js-contain/src/oci/supervise.rs",
        item: "wall_ms",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the crun supervisor's finish stamp; the cell runs no supervisor",
    },
    Allowed {
        file: "crates/js-contain/src/running.rs",
        item: "Running::reap",
        token: Token::InstantNow,
        sites: 2,
        reason: "the reap deadline of a real container; the cell holds no Running",
    },
    Allowed {
        file: "crates/js-jobs/src/ids.rs",
        item: "draw",
        token: Token::Getrandom,
        sites: 2,
        reason: "the job service's id source behind IdSource; the cell runs the server with the job service off, so it mints no job id",
    },
    Allowed {
        file: "crates/js-jobs/src/ids.rs",
        item: "from_draw",
        token: Token::Getrandom,
        sites: 1,
        reason: "names getrandom's error type in the job id source's error mapping; reads no randomness",
    },
    Allowed {
        file: "crates/js-lease/src/clock.rs",
        item: "SystemClock::now",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the server's wall clock behind js_lease::Clock; the cell gives the server its ManualClock through SharedClock",
    },
    Allowed {
        file: "crates/js-lease/src/clock.rs",
        item: "SystemMonotonic::default",
        token: Token::InstantNow,
        sites: 1,
        reason: "the server's monotonic origin; the cell replaces the clock with its virtual one (ServerSpec::config) before any reading",
    },
    Allowed {
        file: "crates/js-logstore/src/clock.rs",
        item: "SystemClock::now",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the log uploader's wall clock behind js_logstore::Clock; the cell's agents upload no logs",
    },
    Allowed {
        file: "crates/js-mac-session/src/macos/admin.rs",
        item: "MacAdmin::now",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the macOS admin port's clock; macOS-only code the cell never builds a host of",
    },
    Allowed {
        file: "crates/js-mac-session/src/macos/host.rs",
        item: "MacHost::now",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the macOS host port's clock; macOS-only code the cell never builds a host of",
    },
    Allowed {
        file: "crates/js-mac-session/src/proto.rs",
        item: "recv_by",
        token: Token::InstantNow,
        sites: 1,
        reason: "the receive deadline on the session helper's socket; the cell runs no session helper",
    },
    Allowed {
        file: "crates/js-mac-session/src/server.rs",
        item: "connection",
        token: Token::InstantNow,
        sites: 1,
        reason: "the session helper's clock, given to connection_on; the cell runs no session helper",
    },
    Allowed {
        file: "crates/js-provider-lambdalabs/src/clock.rs",
        item: "SystemClock::new",
        token: Token::InstantNow,
        sites: 1,
        reason: "the Lambda Labs provider's clock for its live API; the cell's pools are static and call no provider",
    },
    Allowed {
        file: "crates/js-registry/src/session.rs",
        item: "OsSessions::fresh",
        token: Token::Getrandom,
        sites: 1,
        reason: "session secrets, which a cell run draws; the agents only echo them and no trace line or decision holds one",
    },
    Allowed {
        file: "crates/js-state/src/clock.rs",
        item: "SystemClock::now",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the state store's wall clock; the cell runs the server with the job service and usage recording off",
    },
    Allowed {
        file: "crates/js-state/src/sqlite/mod.rs",
        item: "SqliteStore::call",
        token: Token::InstantNow,
        sites: 1,
        reason: "times a SQLite commit; the cell opens no SqliteStore",
    },
    Allowed {
        file: "crates/js-state/src/sqlite/writer.rs",
        item: "Writer::run",
        token: Token::InstantNow,
        sites: 2,
        reason: "the SQLite writer's group-commit window; the cell opens no SqliteStore",
    },
    Allowed {
        file: "crates/js-testkit/src/act.rs",
        item: "perform",
        token: Token::InstantNow,
        sites: 2,
        reason: "the act test helper burning CPU for a wall-clock span in its own process; the cell uses only TestDir",
    },
    Allowed {
        file: "crates/js-testkit/src/act.rs",
        item: "wait_for",
        token: Token::InstantNow,
        sites: 2,
        reason: "the act test helper waiting for a file in its own process; the cell uses only TestDir",
    },
    Allowed {
        file: "crates/js-testkit/src/probe.rs",
        item: "clean_since_within",
        token: Token::InstantNow,
        sites: 2,
        reason: "the residue probe of real-kernel tests; the cell uses only TestDir",
    },
    Allowed {
        file: "crates/js-testkit/src/reaped.rs",
        item: "Reaped::drop",
        token: Token::InstantNow,
        sites: 1,
        reason: "the grace of a guard over a real child process; the cell starts no process",
    },
    Allowed {
        file: "crates/js-treestore/src/store.rs",
        item: "Config::default",
        token: Token::SystemTimeNow,
        sites: 1,
        reason: "the tree store's default wall clock, which its Config replaces; the cell uses no tree store",
    },
];
