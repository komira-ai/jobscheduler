//! The summary step: groups the records by setting and arm and hands the groups on.
//!
//! This crate computes no statistics. The analysis (means, intervals, tests of two arms)
//! is another crate's; it reads the same JSON lines, or takes the [`Group`]s of
//! [`group`] through the [`Summarise`] seam.

use std::collections::BTreeMap;

use crate::metric::{Record, WARMUP};

/// Why records cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {why}")]
pub struct ReadError {
    /// The line, from 1.
    pub line: usize,
    /// What serde said.
    pub why: String,
}

/// The runs of one arm of one setting, in the order they were executed.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The setting.
    pub setting: String,
    /// The arm.
    pub arm: String,
    /// Every run of the arm, warm-ups included (they carry their label), by `run_index`.
    pub runs: Vec<Record>,
}

/// What turns groups into a report. The analysis crate's entry point is an
/// implementation of this; [`Listing`] is the one this crate has.
pub trait Summarise {
    /// The report of `groups`, as text.
    fn summarise(&self, groups: &[Group]) -> String;
}

/// Reads records, one JSON object per line; blank lines are skipped.
///
/// # Errors
///
/// [`ReadError`] naming the first line that is not a record.
pub fn read_records(text: &str) -> Result<Vec<Record>, ReadError> {
    todo!("red: not implemented")
}

/// Groups records by setting and arm (sorted by both), each group by `run_index`.
#[must_use]
pub fn group(records: Vec<Record>) -> Vec<Group> {
    todo!("red: not implemented")
}

/// Lists each group: how many runs (and warm-ups), their indexes, and how many runs are
/// missing each metric. It computes nothing from the values.
#[derive(Debug, Clone, Copy, Default)]
pub struct Listing;

impl Summarise for Listing {
    fn summarise(&self, groups: &[Group]) -> String {
        let mut text = String::new();
        for one in groups {
            let warmups = one.runs.iter().filter(|run| run.label == WARMUP).count();
            let indexes: Vec<String> = one.runs.iter().map(|r| r.run_index.to_string()).collect();
            let mut missing: BTreeMap<&str, usize> = BTreeMap::new();
            for run in &one.runs {
                for (name, metric) in &run.metrics {
                    if metric.is_none() {
                        *missing.entry(name).or_default() += 1;
                    }
                }
            }
            let missing: Vec<String> = missing.iter().map(|(n, c)| format!("{n}={c}")).collect();
            text.push_str(&format!(
                "{} {}: {} runs ({} warm-up); run_index {}; not available: {}\n",
                one.setting,
                one.arm,
                one.runs.len(),
                warmups,
                indexes.join(" "),
                if missing.is_empty() {
                    "none".to_owned()
                } else {
                    missing.join(" ")
                },
            ));
        }
        text
    }
}
