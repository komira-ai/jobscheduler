//! What `js-coverage check` measures and prints: the per-crate coverage summed from a
//! tracefile, and the report built from it and the baseline (the table, then the
//! uncovered lines and branches of each crate failing on a rise, then, on a failure,
//! what `tighten` would do about it). The binary reads the files, forwards here, and
//! prints the report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::CrateCoverage;
use crate::baseline::{Baseline, Entry};
use crate::lcov::{self, FileRecord, LcovError};
use crate::ratchet::{self, Unattributed};
use crate::{tighten, uncovered};

/// What a tracefile measured: each workspace crate's coverage, and the records it was
/// summed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    /// Each workspace crate's coverage, zero for a crate no record names.
    pub crates: BTreeMap<String, CrateCoverage>,
    /// The tracefile's records.
    pub files: Vec<FileRecord>,
}

/// Why a tracefile could not be measured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MeasureError {
    /// The tracefile was refused.
    #[error("{path}: {source}")]
    Lcov {
        /// The tracefile, as the caller named it.
        path: String,
        /// Why it was refused.
        source: LcovError,
    },
    /// A record is outside every workspace crate.
    #[error(transparent)]
    Unattributed(#[from] Unattributed),
}

impl Measured {
    /// Parses the tracefile `text` (read from `path`, named in an error) and sums its
    /// records per crate of `workspace`, whose sources are under `sources/crates`.
    pub fn from_tracefile(
        text: &str,
        path: &Path,
        sources: &Path,
        workspace: &BTreeSet<String>,
    ) -> Result<Self, MeasureError> {
        let files = lcov::parse(text).map_err(|source| MeasureError::Lcov {
            path: path.display().to_string(),
            source,
        })?;
        let crates = ratchet::by_crate(sources, workspace, &files)?;
        Ok(Self { crates, files })
    }

    /// The measurement as a baseline file records it.
    pub fn entries(&self) -> Baseline {
        self.crates
            .iter()
            .map(|(k, c)| (k.clone(), Entry::of(*c)))
            .collect()
    }
}

/// The report `check` prints, and whether the ratchet passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The table, the uncovered places, and on a failure the `tighten` advice.
    pub text: String,
    /// Whether every crate matches its baseline line.
    pub pass: bool,
}

/// Builds the report for `measured` (record paths under `sources`) against
/// `recorded`; `tighten_command` is the command the advice tells the reader to run.
pub fn report(
    measured: &Measured,
    sources: &Path,
    recorded: &Baseline,
    tighten_command: &str,
) -> Report {
    let rows = ratchet::rows(&measured.crates, recorded);
    let mut text = ratchet::render(&rows);
    text.push_str(&uncovered::render(
        sources,
        &rows,
        &measured.files,
        uncovered::CAP,
    ));
    let pass = !rows.iter().any(|r| r.status().fails());
    if !pass {
        text.push('\n');
        text.push_str(&tighten::check_advice(
            recorded,
            &measured.entries(),
            tighten_command,
        ));
    }
    Report { text, pass }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    const TRACE: &str = "SF:/w/crates/a/src/lib.rs\nDA:3,0\nLF:2\nLH:1\nend_of_record\n";

    /// Asserts that the measurement sums the records per workspace crate, keeps the
    /// records (for the uncovered list), and gives a crate no record names a zero entry.
    #[test]
    fn measures_each_workspace_crate() {
        let m = Measured::from_tracefile(
            TRACE,
            Path::new("t.info"),
            Path::new("/w"),
            &set(&["a", "b"]),
        )
        .expect("valid");
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.crates["a"].lines.missed(), Some(1));
        assert_eq!(m.crates["b"], CrateCoverage::default());
        let e = m.entries();
        assert_eq!((e["a"].lines, e["a"].branches), (Some(1), None));
        assert_eq!((e["b"].lines, e["b"].branches), (None, None));
    }

    /// Asserts that a refused tracefile is reported with the path it was read from, and
    /// a record outside every crate with that record's path.
    #[test]
    fn refusals_name_the_file_or_the_record() {
        let bad = Measured::from_tracefile("", Path::new("t.info"), Path::new("/w"), &set(&["a"]));
        assert_eq!(
            bad.expect_err("empty").to_string(),
            "t.info: the tracefile holds no records"
        );
        let outside =
            Measured::from_tracefile(TRACE, Path::new("t.info"), Path::new("/v"), &set(&["a"]));
        assert_eq!(
            outside.expect_err("outside").to_string(),
            "/w/crates/a/src/lib.rs is not under <root>/crates/<crate>/ for any workspace crate"
        );
    }

    /// Asserts the report's parts and order: on a failure the table, then the crate's
    /// uncovered line, then the advice with the given command; on a pass the table alone.
    #[test]
    fn report_puts_the_places_between_the_table_and_the_advice() {
        let m = Measured::from_tracefile(TRACE, Path::new("t.info"), Path::new("/w"), &set(&["a"]))
            .expect("valid");
        let zero: Baseline = [(
            "a".to_owned(),
            Entry {
                lines: Some(0),
                branches: None,
            },
        )]
        .into();
        let r = report(&m, Path::new("/w"), &zero, "TIGHTEN");
        assert!(!r.pass);
        let table = r.text.find("coverage ratchet: FAIL").expect("table");
        let place = r.text.find("- crates/a/src/lib.rs:3\n").expect("place");
        let advice = r
            .text
            .find("`js-coverage tighten` cannot fix")
            .expect("advice");
        let mut order = [table, place, advice];
        order.sort_unstable();
        assert_eq!(order, [table, place, advice], "{}", r.text);

        let r = report(&m, Path::new("/w"), &m.entries(), "TIGHTEN");
        assert!(r.pass);
        assert!(r.text.ends_with("coverage ratchet: PASS\n"), "{}", r.text);
        assert!(!r.text.contains("Uncovered"), "{}", r.text);
    }
}
