//! The documentation check: every `js` command the runbooks show, and every `/v1/` route the
//! runbooks and the gateway's page name, exists. It is used by the `doc_refs` test rather
//! than a verb: `cargo test -p xtask --test doc_refs`.
//!
//! **Which documents.** Every `.md` file of [`RUNBOOKS`], in name order, then
//! [`GATEWAY_DOC`] ([`documents`]).
//!
//! **What is a command.** A line of a fenced block (between two lines that start with three
//! backticks), joined with the lines after it while it ends with `\`, that starts with `js `
//! once its indentation and a `$ ` prompt are dropped; and an inline code span (between two
//! backticks) outside a fenced block that starts with `js ` ([`commands`]). A continued line
//! that a fence closes is read as it stands. `js-agent` and other programs are not `js`, and
//! neither is a `js` behind `sudo`, a variable assignment or `$(`, nor in a `~~~` fence: the
//! documents show none.
//!
//! **What a command must be** ([`resolve`]). Its words are read as the parser of `js` reads
//! them, against the tree it parses with (`js_cli::command()`, built so that the global flags
//! and `--help` are on every command): a `--name` or `--name=value` must be a flag of the
//! command the words so far name (a flag that takes a value takes the next word when it has
//! no `=`), and while that command has subcommands the next other word must be one of them.
//! A word after a command without subcommands is its argument. A placeholder such as
//! `<client token>` or `'a b'` is one word. The words end at `--` (the rest is the program
//! a command runs), at `...` (the text leaves the rest out), and at a shell operator (`|`,
//! `||`, `&&`, `;`, an output redirect, a `#` comment). Flags are read by their long names
//! only: a word with one dash is read as a command word.
//!
//! **What is a route.** Every text that starts `/v1/`, anywhere in the document (in a URL,
//! code or prose), up to the first character that cannot be in a route template (a
//! letter, a digit, or one of `_-{}:.*%/`), without a final `.`, `:` or `,` ([`routes`]).
//!
//! **What a route must be** ([`route_exists`]). It names a route of the gateway or of the
//! server, or the start of one (`/v1/agent` is the start of `/v1/agent/hello`), segment by
//! segment: a literal segment equals the route's; a route's variable (`{job_id}`) takes any
//! segment, an empty one too, and a variable with a custom verb (`{job_id}:cancel`) one
//! with the same verb (a whole variable also takes `x:verb`, as the server's
//! `/v1/hosts/{target}` does); a wildcard (`{*rest}`) of either takes the rest. A final `/` names
//! the routes under the path. The gateway's routes are its route table
//! (`js_api_gateway::ROUTES`, which its build reads from the `google.api.http` options of
//! the job service); the server's are the literal first argument, when it starts `/v1/`, of
//! every `.route(...)` call in the production source of [`SERVER_SRC`] ([`server_routes`]):
//! not in a module, function or `impl` under a `#[cfg(...)]` that names `test` outside a
//! `not(...)`, nor in a `#[test]` function.

use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{
    Attribute, Expr, ExprLit, ExprMethodCall, ImplItemFn, ItemFn, ItemImpl, ItemMod, Lit, Meta,
};

/// The runbooks, from the repository root.
pub const RUNBOOKS: &str = "docs/runbooks";

/// The gateway's page, from the repository root.
pub const GATEWAY_DOC: &str = "docs/http-gateway.md";

/// The source of the server whose own routes the documents may name, from the repository
/// root.
pub const SERVER_SRC: &str = "crates/js-lease-server/src";

/// Why the check could not run.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// A document, a directory or a source file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file or directory.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A source file of the server is not Rust that parses.
    #[error("cannot parse {}: {message}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        message: String,
    },
}

/// A place where a document names a command or a route that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stale {
    /// The document, from the repository root.
    pub file: String,
    /// The line, from 1.
    pub line: usize,
    /// The word or the route that does not resolve.
    pub token: String,
    /// What is missing.
    pub why: String,
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: {}: {}",
            self.file, self.line, self.token, self.why
        )
    }
}

/// A command or a route a document shows, and its line (from 1; a command's first line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The line.
    pub line: usize,
    /// The text of the command or the route.
    pub text: String,
}

/// Every place under `root` where a document of [`documents`] names a command that `tree`
/// does not parse, or a route that is neither one of `gateway` (route templates) nor one
/// the server's source routes; in the order of the documents, then of their lines.
///
/// # Errors
///
/// [`DocError::Read`] for a document, the runbooks' directory or a source file of the
/// server that cannot be read (a `mod` whose file is missing), and [`DocError::Parse`] for
/// a source file that does not parse.
pub fn check(root: &Path, tree: &clap::Command, gateway: &[&str]) -> Result<Vec<Stale>, DocError> {
    let mut tree = tree.clone();
    tree.build();
    let mut known: Vec<String> = gateway.iter().map(|route| (*route).to_owned()).collect();
    known.extend(server_routes(&root.join(SERVER_SRC))?);
    let mut stale = Vec::new();
    for file in documents(root)? {
        let text = read(&root.join(&file))?;
        let mut found: Vec<Stale> = Vec::new();
        for mention in commands(&text) {
            if let Err((token, why)) = resolve(&tree, &mention.text) {
                found.push(Stale {
                    file: file.clone(),
                    line: mention.line,
                    token,
                    why,
                });
            }
        }
        for mention in routes(&text) {
            if !route_exists(&mention.text, &known) {
                found.push(Stale {
                    file: file.clone(),
                    line: mention.line,
                    why: "no route of the gateway or of the server starts so".to_owned(),
                    token: mention.text,
                });
            }
        }
        found.sort();
        stale.extend(found);
    }
    Ok(stale)
}

/// The documents the check reads, from the repository root: every `.md` file of
/// [`RUNBOOKS`] in name order, then [`GATEWAY_DOC`].
///
/// # Errors
///
/// [`DocError::Read`] when the runbooks' directory cannot be listed.
pub fn documents(root: &Path) -> Result<Vec<String>, DocError> {
    let dir = root.join(RUNBOOKS);
    let failed = |source| DocError::Read {
        path: dir.clone(),
        source,
    };
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(failed)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".md") {
            names.push(format!("{RUNBOOKS}/{name}"));
        }
    }
    names.sort();
    names.push(GATEWAY_DOC.to_owned());
    Ok(names)
}

/// The `js` commands `text` shows: in its fenced blocks and in its inline code spans.
#[must_use]
pub fn commands(text: &str) -> Vec<Mention> {
    let mut found = Vec::new();
    let mut fenced = false;
    let mut joined: Option<Mention> = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            found.extend(joined.take().and_then(command_of));
            continue;
        }
        if !fenced {
            let spans = line.split('`').skip(1).step_by(2);
            let spans = spans.filter(|span| span.starts_with("js "));
            found.extend(spans.map(|span| Mention {
                line: index + 1,
                text: span.to_owned(),
            }));
            continue;
        }
        let mut logical = joined.take().unwrap_or(Mention {
            line: index + 1,
            text: String::new(),
        });
        let part = line.trim_end();
        logical
            .text
            .push_str(part.strip_suffix('\\').unwrap_or(part));
        logical.text.push(' ');
        if part.ends_with('\\') {
            joined = Some(logical);
            continue;
        }
        found.extend(command_of(logical));
    }
    found
}

/// The `js` command a fenced line (joined across continuations) shows, if it shows one.
fn command_of(logical: Mention) -> Option<Mention> {
    let shown = logical.text.trim_start();
    let shown = shown.strip_prefix("$ ").unwrap_or(shown);
    shown.starts_with("js ").then(|| Mention {
        line: logical.line,
        text: shown.trim_end().to_owned(),
    })
}

/// The `/v1/` routes `text` names, each with its line.
#[must_use]
pub fn routes(text: &str) -> Vec<Mention> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(at) = rest.find("/v1/") {
            let tail = &rest[at..];
            let end = tail.find(|c: char| !in_route(c)).unwrap_or(tail.len());
            let route = tail[..end].trim_end_matches(['.', ':', ',']);
            found.push(Mention {
                line: index + 1,
                text: route.to_owned(),
            });
            rest = &tail[end..];
        }
    }
    found
}

/// Whether `c` can be part of a route template.
fn in_route(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-{}:.*%/".contains(c)
}

/// Whether `command` (starting with `js`) is a command of `tree` (a built tree); when it is
/// not, the first word that does not resolve and what is missing.
///
/// # Errors
///
/// The word and the reason, for a flag the command so far does not have or a word that is
/// none of its subcommands.
pub fn resolve(tree: &clap::Command, command: &str) -> Result<(), (String, String)> {
    let words = split(command);
    let mut at = tree;
    let mut path = vec![tree.get_name().to_owned()];
    let mut rest = words.iter().skip(1);
    while let Some(word) = rest.next() {
        let word = word.as_str();
        if ends(word) {
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, value) = long.split_once('=').unzip();
            let name = name.unwrap_or(long);
            let flag = at.get_arguments().find(|arg| arg.get_long() == Some(name));
            let missing = || {
                (
                    word.to_owned(),
                    format!("`{}` has no flag --{name}", path.join(" ")),
                )
            };
            let flag = flag.ok_or_else(missing)?;
            if flag.get_action().takes_values() && value.is_none() {
                rest.next();
            }
            continue;
        }
        if at.has_subcommands() {
            let missing = || {
                (
                    word.to_owned(),
                    format!("`{}` has no command {word}", path.join(" ")),
                )
            };
            at = at.find_subcommand(word).ok_or_else(missing)?;
            path.push(word.to_owned());
        }
    }
    Ok(())
}

/// Whether `word` ends the words of `js`: the program's own arguments, an elision, or a
/// shell operator.
fn ends(word: &str) -> bool {
    ["--", "...", "|", "||", "&&", ";"].contains(&word)
        || word.starts_with(['>', '#'])
        || word.starts_with("2>")
}

/// The words of `command`, split at white space except inside `<...>`, `'...'` and `"..."`.
fn split(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut until: Option<char> = None;
    for c in command.chars() {
        match until {
            Some(close) => {
                word.push(c);
                until = (c != close).then_some(close);
            }
            None if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            None => {
                until = match c {
                    '<' => Some('>'),
                    '\'' | '"' => Some(c),
                    _ => None,
                };
                word.push(c);
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Whether `route` names one of `known` (route templates), or the start of one.
#[must_use]
pub fn route_exists(route: &str, known: &[String]) -> bool {
    let named = segments(route);
    known
        .iter()
        .any(|template| matches(&named, &segments(template)))
}

/// The segments of `path`, without its leading `/` and a final `/`.
fn segments(path: &str) -> Vec<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    path.split('/').collect()
}

/// Whether the segments `named` are the segments `template`, or their start.
fn matches(named: &[&str], template: &[&str]) -> bool {
    match (named.split_first(), template.split_first()) {
        (Some((one, _)), Some((other, _))) if one.starts_with("{*") || other.starts_with("{*") => {
            true
        }
        (Some((one, named)), Some((other, template))) => {
            segment_matches(one, other) && matches(named, template)
        }
        (Some(_), None) => false,
        (None, _) => true,
    }
}

/// Whether the segment `named` is the template's segment `template`.
fn segment_matches(named: &str, template: &str) -> bool {
    match template.rsplit_once(':') {
        Some((base, custom)) => {
            let (named_base, named_custom) = named.rsplit_once(':').unzip();
            named_custom == Some(custom) && (variable(base) || named_base == Some(base))
        }
        None => variable(template) || named == template,
    }
}

/// Whether `segment` is a whole variable, such as `{job_id}`.
fn variable(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}')
}

/// The `/v1/` routes the production source under `src` gives: the literal first argument of
/// each `.route(...)` call, in the modules `lib.rs` declares (transitively), leaving out a
/// module, function or `impl` under a `#[cfg(...)]` that names `test` (outside a
/// `not(...)`) and a `#[test]` function. A `mod name;` is `name.rs` or `name/mod.rs` in the
/// directory of its parent's modules, below the inline modules it stands in; a
/// `#[path = "..."]` one is the file at that path from the directory of the declaring file
/// (below the inline modules it stands in), and its own modules are in its own directory.
///
/// # Errors
///
/// [`DocError::Read`] for a file that cannot be read (a module whose file is missing) and
/// [`DocError::Parse`] for one that does not parse.
pub fn server_routes(src: &Path) -> Result<Vec<String>, DocError> {
    let mut found = Vec::new();
    let mut files = vec![(src.join("lib.rs"), src.to_path_buf())];
    while let Some((file, dir)) = files.pop() {
        let text = read(&file)?;
        let parsed = syn::parse_file(&text).map_err(|error| DocError::Parse {
            path: file.clone(),
            message: error.to_string(),
        })?;
        let mut visit = Routes::default();
        visit.visit_file(&parsed);
        found.extend(visit.routes);
        for Declared { within, name, path } in visit.modules {
            let (child, child_dir) = match path {
                Some(path) => {
                    let base = file.parent().unwrap_or(&dir).to_path_buf();
                    let child = within
                        .iter()
                        .fold(base, |at, part| at.join(part))
                        .join(path);
                    let child_dir = child.parent().unwrap_or(&dir).to_path_buf();
                    (child, child_dir)
                }
                None => {
                    let dir = within.iter().fold(dir.clone(), |at, part| at.join(part));
                    let flat = dir.join(format!("{name}.rs"));
                    let child = match flat.exists() {
                        true => flat,
                        false => dir.join(&name).join("mod.rs"),
                    };
                    (child, dir.join(&name))
                }
            };
            files.push((child, child_dir));
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// A module one source file declares in a file of its own: the inline modules it stands
/// in, its name, and its `#[path]` when it has one.
struct Declared {
    within: Vec<String>,
    name: String,
    path: Option<String>,
}

/// What one source file gives: its routes, and the modules it declares in files of their
/// own.
#[derive(Default)]
struct Routes {
    routes: Vec<String>,
    modules: Vec<Declared>,
    within: Vec<String>,
}

impl<'ast> Visit<'ast> for Routes {
    fn visit_item_mod(&mut self, module: &'ast ItemMod) {
        if module.attrs.iter().any(test_only) {
            return;
        }
        let name = module.ident.to_string();
        match module.content {
            Some(_) => {
                self.within.push(name);
                syn::visit::visit_item_mod(self, module);
                self.within.pop();
            }
            None => self.modules.push(Declared {
                within: self.within.clone(),
                name,
                path: module.attrs.iter().find_map(path_literal),
            }),
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !item.attrs.iter().any(test_code) {
            syn::visit::visit_item_fn(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if !item.attrs.iter().any(test_code) {
            syn::visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if !item.attrs.iter().any(test_only) {
            syn::visit::visit_item_impl(self, item);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == "route"
            && let Some(Expr::Lit(ExprLit {
                lit: Lit::Str(route),
                ..
            })) = call.args.first()
            && route.value().starts_with("/v1/")
        {
            self.routes.push(route.value());
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

/// Whether `attr` is a `#[cfg(...)]` that names `test` outside a `not(...)`.
fn test_only(attr: &Attribute) -> bool {
    match &attr.meta {
        Meta::List(list) if list.path.is_ident("cfg") => names_test(list.tokens.clone()),
        _ => false,
    }
}

/// Whether `attr` makes a function test code: [`test_only`], or `#[test]`.
fn test_code(attr: &Attribute) -> bool {
    attr.path().is_ident("test") || test_only(attr)
}

/// Whether `tokens` hold the word `test`, at any depth but inside a `not(...)`.
fn names_test(tokens: TokenStream) -> bool {
    let mut negated = false;
    tokens.into_iter().any(|token| {
        let after_not = std::mem::take(&mut negated);
        match token {
            TokenTree::Ident(ident) => {
                negated = ident == "not";
                ident == "test"
            }
            TokenTree::Group(group) => !after_not && names_test(group.stream()),
            _ => false,
        }
    })
}

/// The file a `#[path = "..."]` attribute names, if `attr` is one.
fn path_literal(attr: &Attribute) -> Option<String> {
    match &attr.meta {
        Meta::NameValue(pair) if pair.path.is_ident("path") => match &pair.value {
            Expr::Lit(ExprLit {
                lit: Lit::Str(path),
                ..
            }) => Some(path.value()),
            _ => None,
        },
        _ => None,
    }
}

/// The text of `path`.
fn read(path: &Path) -> Result<String, DocError> {
    std::fs::read_to_string(path).map_err(|source| DocError::Read {
        path: path.to_path_buf(),
        source,
    })
}
