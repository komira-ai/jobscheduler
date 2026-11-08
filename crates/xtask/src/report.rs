//! `gen-report [--root <dir>] [--check]`: the generated block of the technical report
//! ([`REPORT`]), and the check that holds the rest of the report to the tree ([`check`]).
//!
//! **The generated block.** The report's table of section 8 rows and the scenarios that
//! cover them is not typed by hand. [`rows`] renders it from two tables: the one under
//! [`SCENARIO_HEADING`] in [`SIMULATION`] (row, scenario, family file, what the scenario
//! shows), and section 8 of [`DESIGN`] (the row's failure and the state the design gives
//! it). It renders one table row per scenario, in the order of [`SIMULATION`], then one
//! paragraph that lists the rows of section 8 no scenario covers. [`splice`] puts it
//! between the report's two marker lines ([`BEGIN`], [`END`]) and changes nothing else.
//! The verb writes the block into the report under `--root`, or with `--check` fails when
//! the block is not what it renders; the test `cargo test -p xtask --test report` runs the
//! check on this tree.
//!
//! **Markdown in a cell.** The text of a failure, a state and what a scenario shows is
//! escaped for HTML (`&`, `<`, `>`), a typographic apostrophe replaces `'`, and a code span
//! (between two backticks) becomes a `code` element. The scenario and its family file are
//! identifiers and are taken as they stand, without their backticks: the scenario becomes a
//! `code` element whose `data-test` names its file under [`SIM_CELL`], which [`check`] then
//! holds to the file.

use std::path::{Path, PathBuf};

mod check;

pub use check::{Stale, check};

/// The report, from the repository root.
pub const REPORT: &str = "docs/report/jobscheduler.html";

/// The simulation cell's page, which lists the scenarios of section 8 rows.
pub const SIMULATION: &str = "docs/simulation.md";

/// The design, whose section 8 names each row's failure and state.
pub const DESIGN: &str = "docs/design/jobscheduler-design.md";

/// The crate whose files the scenario table names, from the repository root.
pub const SIM_CELL: &str = "crates/js-sim-cell";

/// The heading of the scenario table in [`SIMULATION`].
pub const SCENARIO_HEADING: &str = "## Which section 8 rows have a scenario";

/// The heading of section 8 in [`DESIGN`].
pub const ROWS_HEADING: &str = "## 8. Races and failures the current code loses";

/// The line before the generated block.
pub const BEGIN: &str = "<!-- gen-report rows begin -->";

/// The line after the generated block.
pub const END: &str = "<!-- gen-report rows end -->";

/// Why the report could not be generated or checked.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// A file could not be read, or the report could not be written.
    #[error("{}: {source}", path.display())]
    File {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A source table is missing or malformed, or the report's markers are wrong.
    #[error("{file}: {why}")]
    Source {
        /// The file, from the repository root.
        file: &'static str,
        /// What is wrong.
        why: String,
    },
    /// `--check`: the report does not hold to the tree.
    #[error("{REPORT} does not hold to the tree: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Stale(Vec<Stale>),
}

/// The flags of `gen-report`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The repository root.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Write nothing: fail when the generated block is not current or the report names a
    /// path, test, constant or metric the tree does not have.
    #[arg(long)]
    pub check: bool,
}

/// Runs the verb: the line to print when it passed.
///
/// # Errors
///
/// [`ReportError::File`] for a file that cannot be read or a report that cannot be written,
/// [`ReportError::Source`] for a source table or marker that is wrong and, with `--check`,
/// [`ReportError::Stale`].
pub fn run(args: &Args) -> Result<String, ReportError> {
    if args.check {
        let stale = check(&args.root)?;
        return match stale.is_empty() {
            true => Ok(format!("{REPORT} holds to the tree")),
            false => Err(ReportError::Stale(stale)),
        };
    }
    let page = generated(&args.root)?;
    let path = args.root.join(REPORT);
    std::fs::write(&path, page).map_err(file_error(&path))?;
    Ok(format!("wrote the generated block of {REPORT}"))
}

/// The report under `root` with its generated block rendered from the sources under `root`.
///
/// # Errors
///
/// [`ReportError::File`] and [`ReportError::Source`].
pub fn generated(root: &Path) -> Result<String, ReportError> {
    let simulation = read(root, SIMULATION)?;
    let design = read(root, DESIGN)?;
    let page = read(root, REPORT)?;
    let block = rows(&simulation, &design)?;
    splice(&page, &block).map_err(|why| ReportError::Source { file: REPORT, why })
}

/// Reads `file` under `root`.
fn read(root: &Path, file: &str) -> Result<String, ReportError> {
    let path = root.join(file);
    std::fs::read_to_string(&path).map_err(file_error(&path))
}

/// The error of `path` from the error of reading or writing it.
fn file_error(path: &Path) -> impl FnOnce(std::io::Error) -> ReportError + '_ {
    move |source| ReportError::File {
        path: path.to_path_buf(),
        source,
    }
}

/// The generated block: the table rows of each scenario, then the paragraph of the rows no
/// scenario covers.
///
/// # Errors
///
/// [`ReportError::Source`] when either table is missing, a row has too few cells, a row
/// number is not a number, or a scenario names a row section 8 does not have.
pub fn rows(simulation: &str, design: &str) -> Result<String, ReportError> {
    let scenarios = table(simulation, SCENARIO_HEADING, 4).map_err(|why| ReportError::Source {
        file: SIMULATION,
        why,
    })?;
    let failures =
        table(design, ROWS_HEADING, 3).map_err(|why| ReportError::Source { file: DESIGN, why })?;
    let mut out = String::from(
        "<div class=\"tblw wide\"><table class=\"tbl\">\n<thead><tr><th>Row</th><th>Failure</th><th>State in the design</th><th>Scenario</th><th>What the scenario shows</th></tr></thead>\n<tbody>\n",
    );
    let mut covered = Vec::new();
    for cells in &scenarios {
        let row = &cells[0];
        let failure =
            failures
                .iter()
                .find(|f| &f[0] == row)
                .ok_or_else(|| ReportError::Source {
                    file: SIMULATION,
                    why: format!("row {row} is not a row of section 8 of {DESIGN}"),
                })?;
        covered.push(row.clone());
        let scenario = cells[1].trim_matches('`');
        let file = cells[2].trim_matches('`');
        out.push_str(&format!(
            "<tr><td>{row}</td><td>{}</td><td>{}</td><td><code data-test=\"{SIM_CELL}/{file}\">{scenario}</code></td><td>{}</td></tr>\n",
            inline(&failure[1]),
            inline(&failure[2]),
            inline(&cells[3]),
        ));
    }
    out.push_str("</tbody></table></div>\n");
    let open: Vec<&str> = failures
        .iter()
        .map(|f| f[0].as_str())
        .filter(|row| !covered.iter().any(|c| c == row))
        .collect();
    out.push_str(&format!(
        "<p>Rows of section 8 with no scenario ({} of {}): {}.</p>\n",
        open.len(),
        failures.len(),
        open.join(", ")
    ));
    Ok(out)
}

/// The rows of the first table after the line `heading` in `text`, without its header and
/// separator, each with at least `cells` cells and a number in its first.
fn table(text: &str, heading: &str, cells: usize) -> Result<Vec<Vec<String>>, String> {
    let rest = text
        .split_once(&format!("\n{heading}\n"))
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("no heading \"{heading}\""))?;
    let rows: Vec<Vec<String>> = rest
        .lines()
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect()
        })
        .collect();
    if rows.is_empty() {
        return Err(format!("no table rows under \"{heading}\""));
    }
    match rows
        .iter()
        .find(|row| row.len() < cells || row[0].parse::<u32>().is_err())
    {
        Some(row) => Err(format!(
            "a row under \"{heading}\" has fewer than {cells} cells or no row number: {}",
            row.join(" | ")
        )),
        None => Ok(rows),
    }
}

/// A cell's markdown as HTML: escaped, `'` as a typographic apostrophe, code spans as `code`.
fn inline(cell: &str) -> String {
    let escaped = cell
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&rsquo;");
    escaped
        .split('`')
        .enumerate()
        .map(|(i, part)| match i % 2 {
            1 => format!("<code>{part}</code>"),
            _ => part.to_owned(),
        })
        .collect()
}

/// `page` with what stands between its [`BEGIN`] and [`END`] lines replaced by `block`.
///
/// # Errors
///
/// When either marker is not in the page exactly once, or the end comes first.
pub fn splice(page: &str, block: &str) -> Result<String, String> {
    let once = |marker: &str| match page.matches(marker).count() {
        1 => page.find(marker).ok_or_else(String::new),
        n => Err(format!("the marker {marker} is there {n} times, not once")),
    };
    let begin = once(BEGIN)? + BEGIN.len();
    let end = once(END)?;
    match begin <= end {
        true => Ok(format!("{}\n{block}{}", &page[..begin], &page[end..])),
        false => Err(format!("{END} comes before {BEGIN}")),
    }
}
