//! The check of the report: what it quotes from the tree is in the tree.
//!
//! [`check`] reads [`REPORT`](super::REPORT) and returns each place where it does not hold
//! to the repository under `root`, as a [`Stale`]:
//!
//! - **The generated block** is what [`generated`](super::generated) renders now.
//! - **Paths.** A `code` element whose text starts `crates/`, `docs/`, `.github/`,
//!   `packaging/`, `tools/` or `site/` and has no space, and a link into this repository's
//!   `main` (`https://github.com/komira-ai/jobscheduler/blob/main/...` or `/tree/main/...`),
//!   name a file or directory that exists.
//! - **Anchors.** A link's `#` fragment names a heading of the `.md` file it links to
//!   (its slug as GitHub makes it: lower case, spaces as `-`, every character but letters,
//!   digits, `-` and `_` dropped), and a link `#x` within the page names an `id` of the page.
//! - **Crates.** A `code` element whose whole text is a `js-` name or `xtask` names a crate:
//!   `crates/<name>/Cargo.toml` exists.
//! - **Test names.** A `code` element with a `data-test` attribute names a test in that
//!   file: the file, read without whitespace, holds `fn<name>(` or
//!   `Scenario::new("<name>"`. A `code` element without it whose text reads as a test name
//!   (lower case, digits and underscores, five words or more) and is not a metric is a
//!   test name with no file, which is refused.
//! - **Test commands.** In a `code` element that holds `cargo test`, `-p <crate>` names a
//!   crate and `--test <name>` a test target of it (`tests/<name>.rs` or
//!   `tests/<name>/main.rs`).
//! - **Cells.** Each `span` with a `data-cell` has an id of its own and is one of two kinds.
//!   A cell of the code has `data-source` (a file) and `data-item` (a `const` or `static`
//!   of it), and one numeric literal of the item's value (after the first `=`, up to the
//!   `;`, read without `_`) is the first number of the cell's text. An open cell has
//!   `data-open` (the experiment that will measure it) and prints no digit; the metrics that
//!   will carry it are the `code` elements inside it, each a name of [`METRICS`] (the value
//!   of one of its `pub const`s) or a `reported_` word of [`MEASURE_DOC`], and a cell with
//!   none says `no metric yet`.

use std::path::Path;

use super::{REPORT, ReportError, generated, read};

/// The metric names of js-measure, from the repository root.
pub const METRICS: &str = "crates/js-measure/src/metric.rs";

/// The crate docs of js-measure, which name the `reported_` metrics of its plans.
pub const MEASURE_DOC: &str = "crates/js-measure/src/lib.rs";

/// The start of a link into this repository's `main`.
const REPO: &str = "https://github.com/komira-ai/jobscheduler/";

/// The starts of the text of a `code` element that is a path from the repository root.
const PATH_STARTS: [&str; 6] = [
    "crates/",
    "docs/",
    ".github/",
    "packaging/",
    "tools/",
    "site/",
];

/// One place where the report does not hold to the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// The generated block is not what `cargo xtask gen-report` renders.
    Rows,
    /// A quoted path names nothing.
    Path(String),
    /// A quoted crate name is not a crate of the workspace.
    Crate(String),
    /// A quoted test is not in the file it names.
    Test {
        /// The file.
        file: String,
        /// The test.
        name: String,
    },
    /// A quoted test name names no file.
    Unfiled(String),
    /// A quoted `cargo test --test` names no test target.
    Target(String),
    /// A cell of the code names an item its file does not have, or one that does not hold
    /// the cell's number.
    Item {
        /// The cell's id.
        cell: String,
        /// What is wrong.
        why: String,
    },
    /// An open cell prints a number.
    Number(String),
    /// An open cell names a metric js-measure does not have.
    Metric {
        /// The cell's id.
        cell: String,
        /// The metric.
        metric: String,
    },
    /// A cell is of neither kind, or its id is taken.
    Cell(String),
    /// A link's fragment names no heading of its file, or no `id` of the page.
    Anchor(String),
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rows => write!(
                f,
                "the generated block is not what `cargo xtask gen-report` renders"
            ),
            Self::Path(path) => write!(f, "{path} does not exist"),
            Self::Crate(name) => write!(f, "{name} is not a crate of the workspace"),
            Self::Test { file, name } => write!(f, "{file} has no test {name}"),
            Self::Unfiled(name) => write!(f, "{name} reads as a test name and has no data-test"),
            Self::Target(command) => write!(f, "`{command}` names no test target"),
            Self::Item { cell, why } => write!(f, "cell {cell}: {why}"),
            Self::Number(cell) => write!(f, "open cell {cell} prints a number"),
            Self::Metric { cell, metric } => {
                write!(f, "cell {cell}: js-measure has no metric {metric}")
            }
            Self::Cell(why) => write!(f, "{why}"),
            Self::Anchor(link) => write!(f, "{link} names no heading or id"),
        }
    }
}

/// One element of the page: its attributes and its text.
struct Element {
    /// The attributes, each `name="value"`.
    attrs: Vec<(String, String)>,
    /// What stands between the tag and its end tag.
    text: String,
}

impl Element {
    /// The value of the attribute `name`.
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Every place the report under `root` does not hold to the tree under `root`.
///
/// # Errors
///
/// [`ReportError::File`] for the report or a source of it that cannot be read, and
/// [`ReportError::Source`] for a source table or a marker that is wrong.
pub fn check(root: &Path) -> Result<Vec<Stale>, ReportError> {
    let page = read(root, REPORT)?;
    let mut stale = Vec::new();
    if generated(root)? != page {
        stale.push(Stale::Rows);
    }
    let metrics = metric_names(&read(root, METRICS)?, &read(root, MEASURE_DOC)?);
    for code in elements(&page, "code") {
        stale.extend(code_span(root, &code, &metrics));
    }
    let ids: Vec<String> = page
        .split(" id=\"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(id, _)| id.to_owned()))
        .collect();
    for link in elements(&page, "a") {
        stale.extend(link_target(
            root,
            link.attr("href").unwrap_or_default(),
            &ids,
        ));
    }
    let mut cells: Vec<String> = Vec::new();
    for span in elements(&page, "span") {
        if let Some(id) = span.attr("data-cell") {
            if cells.iter().any(|seen| seen == id) {
                stale.push(Stale::Cell(format!("the cell id {id} is used twice")));
            }
            cells.push(id.to_owned());
            stale.extend(cell(root, id, &span, &metrics));
        }
    }
    Ok(stale)
}

/// What is wrong with the link `target`: a path into this repository that names nothing, or
/// a fragment that names no heading of the markdown file, or no `id` of the page.
fn link_target(root: &Path, target: &str, ids: &[String]) -> Vec<Stale> {
    if let Some(id) = target.strip_prefix('#') {
        return match ids.iter().any(|known| known == id) {
            true => Vec::new(),
            false => vec![Stale::Anchor(target.to_owned())],
        };
    }
    let Some(path) = ["blob/main/", "tree/main/"]
        .iter()
        .find_map(|kind| target.strip_prefix(&format!("{REPO}{kind}")))
    else {
        return Vec::new();
    };
    let (path, fragment) = path.split_once('#').unwrap_or((path, ""));
    if !root.join(path).exists() {
        return vec![Stale::Path(path.to_owned())];
    }
    let text = std::fs::read_to_string(root.join(path)).unwrap_or_default();
    let headed = text
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .any(|heading| slug(heading) == fragment);
    match fragment.is_empty() || !path.ends_with(".md") || headed {
        true => Vec::new(),
        false => vec![Stale::Anchor(format!("{path}#{fragment}"))],
    }
}

/// The anchor GitHub gives a markdown heading (its text after the first `#`).
fn slug(heading: &str) -> String {
    heading
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .map(|c| match c {
            ' ' => '-',
            other => other,
        })
        .collect()
}

/// What is wrong with one `code` element.
fn code_span(root: &Path, code: &Element, metrics: &[String]) -> Vec<Stale> {
    let text = code.text.as_str();
    if let Some(file) = code.attr("data-test") {
        let source = std::fs::read_to_string(root.join(file)).unwrap_or_default();
        let source: String = source.split_whitespace().collect();
        let found = source.contains(&format!("fn{text}("))
            || source.contains(&format!("Scenario::new(\"{text}\""));
        return match found {
            true => Vec::new(),
            false => vec![Stale::Test {
                file: file.to_owned(),
                name: text.to_owned(),
            }],
        };
    }
    if text.contains("cargo test") {
        return target(root, text);
    }
    let path = PATH_STARTS.iter().any(|start| text.starts_with(start));
    if path && !text.contains(' ') && !root.join(text).exists() {
        return vec![Stale::Path(text.to_owned())];
    }
    let crate_name = text == "xtask"
        || (text.starts_with("js-")
            && text
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    if crate_name && !root.join("crates").join(text).join("Cargo.toml").exists() {
        return vec![Stale::Crate(text.to_owned())];
    }
    let test_like = text.split('_').count() >= 5
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    match test_like && !metrics.iter().any(|m| m == text) {
        true => vec![Stale::Unfiled(text.to_owned())],
        false => Vec::new(),
    }
}

/// What is wrong with a quoted `cargo test` command: its `-p` crate and `--test` target.
fn target(root: &Path, command: &str) -> Vec<Stale> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let after = |flag: &str| {
        words
            .iter()
            .position(|w| *w == flag)
            .and_then(|i| words.get(i + 1))
            .copied()
    };
    let krate = root.join("crates").join(after("-p").unwrap_or_default());
    let found = match after("--test") {
        Some(test) => {
            let tests = krate.join("tests");
            tests.join(format!("{test}.rs")).exists() || tests.join(test).join("main.rs").exists()
        }
        None => krate.join("Cargo.toml").exists(),
    };
    match found {
        true => Vec::new(),
        false => vec![Stale::Target(command.to_owned())],
    }
}

/// What is wrong with the cell `id`.
fn cell(root: &Path, id: &str, span: &Element, metrics: &[String]) -> Vec<Stale> {
    if let (Some(file), Some(item)) = (span.attr("data-source"), span.attr("data-item")) {
        let source = std::fs::read_to_string(root.join(file)).unwrap_or_default();
        let at = [format!("const {item}:"), format!("static {item}:")]
            .iter()
            .find_map(|decl| source.find(decl.as_str()));
        let why = match at {
            None => format!("{file} has no const or static {item}"),
            Some(at) => {
                let statement = source[at..].split(';').next().unwrap_or_default();
                let value = statement.split_once('=').map_or("", |(_, value)| value);
                let number = first_number(&span.text);
                let held = value
                    .replace('_', "")
                    .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .any(|literal| literal.trim_end_matches('.') == number);
                match held && !number.is_empty() {
                    true => return Vec::new(),
                    false => format!("{item} in {file} does not hold the number {number:?}"),
                }
            }
        };
        return vec![Stale::Item {
            cell: id.to_owned(),
            why,
        }];
    }
    if span.attr("data-open").is_some_and(|e| !e.is_empty()) {
        let mut stale = Vec::new();
        if span.text.chars().any(|c| c.is_ascii_digit()) {
            stale.push(Stale::Number(id.to_owned()));
        }
        let named = elements(&span.text, "code");
        if named.is_empty() && !span.text.contains("no metric yet") {
            stale.push(Stale::Cell(format!(
                "the open cell {id} names no metric and does not say no metric yet"
            )));
        }
        stale.extend(
            named
                .iter()
                .filter(|m| !metrics.contains(&m.text))
                .map(|m| Stale::Metric {
                    cell: id.to_owned(),
                    metric: m.text.clone(),
                }),
        );
        return stale;
    }
    vec![Stale::Cell(format!(
        "the cell {id} has neither data-source and data-item nor data-open"
    ))]
}

/// The first number in `text`, without its thousands commas, or empty.
fn first_number(text: &str) -> String {
    let start = text
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(text.len());
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .filter(|c| *c != ',')
        .collect::<String>()
        .trim_end_matches('.')
        .to_owned()
}

/// The metric names of js-measure: the value of each `pub const NAME: &str = "..."` of
/// `metrics`, and each `reported_` word of `doc`.
fn metric_names(metrics: &str, doc: &str) -> Vec<String> {
    let consts = metrics.lines().filter_map(|line| {
        let value = line
            .trim()
            .strip_prefix("pub const ")?
            .split_once(": &str = \"")?
            .1;
        value.split_once('"').map(|(v, _)| v.to_owned())
    });
    let reported = doc
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.len() > "reported_".len() && w.starts_with("reported_"))
        .map(str::to_owned);
    consts.chain(reported).collect()
}

/// Each `<tag ...>...</tag>` of `page`, in order.
fn elements(page: &str, tag: &str) -> Vec<Element> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(at) = rest.find(&open) {
        rest = &rest[at + open.len()..];
        let Some(end) = rest.find('>') else { break };
        let head = &rest[..end];
        if !(head.is_empty() || head.starts_with(' ')) {
            continue;
        }
        let body = &rest[end + 1..];
        let text = body.find(&close).map_or(body, |stop| &body[..stop]);
        let pieces: Vec<&str> = head.split('"').collect();
        let attrs = pieces
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| {
                let name = pair[0].trim().trim_end_matches('=').trim();
                (name.to_owned(), pair[1].to_owned())
            })
            .collect();
        out.push(Element {
            attrs,
            text: text.to_owned(),
        });
    }
    out
}
