//! Reads an lcov tracefile into one record per source file.
//!
//! `SF:<path>` opens a record, `LF`/`LH` give the lines found and hit, `BRF`/`BRH` the
//! branches found and hit, and `end_of_record` closes it. A summary line a record
//! lacks counts as zero (llvm-cov leaves the branch lines out when branch coverage is
//! off). The counts come from these summaries only.
//!
//! The detail lines name what did not run, for the report of a failing crate
//! ([`crate::uncovered`]): `DA:<line>,<count>[,<checksum>]` with count 0 is an
//! uncovered line, and `BRDA:<line>,<block>,<branch>,<taken>` with taken `0` or `-`
//! an uncovered branch. They need not add up to the summaries: llvm-cov sums a file's
//! summary over its functions and keeps, for a generic function, its best
//! instantiation's count, while it writes one detail line per source line from the
//! counts of every function merged. A line one closure or instantiation leaves unrun
//! while other code on it runs is in the summary and in no detail line. Every other
//! line (`TN`, `FN`, `FNDA`, ...) is skipped.
//!
//! Refused, so that a damaged or merged file cannot report a wrong total or a wrong
//! place: a summary or detail line outside a record, a summary line twice in one
//! record, a count that is not a decimal number, a detail line that is not in the
//! shape above, more hits than items, a record left open at the end, the same source
//! in two records, and a file with no records at all (an empty report would otherwise
//! pass every check).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::{Counts, CrateCoverage};

/// The summary of one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// The source file the record is for.
    pub path: PathBuf,
    /// Its line and branch counts.
    pub coverage: CrateCoverage,
    /// What its detail lines name as not run.
    pub uncovered: Uncovered,
}

/// The lines and branches a record's detail lines name as not run, each once, in
/// source order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uncovered {
    /// Line numbers with a `DA` count of 0.
    pub lines: BTreeSet<u64>,
    /// Branches with a `BRDA` taken count of `0` or `-`.
    pub branches: BTreeSet<Branch>,
}

/// One branch as `BRDA` names it: the line, the block on that line, and the branch
/// within it (llvm-cov numbers a line's conditions as blocks from 0, and their arms
/// as branches 2 x block for true and 2 x block + 1 for false).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Branch {
    /// The source line.
    pub line: u64,
    /// The block on that line.
    pub block: u64,
    /// The branch in that block.
    pub branch: u64,
}

/// Why a tracefile was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LcovError {
    /// A field before any `SF`.
    #[error("line {line}: {field} outside a record (no SF before it)")]
    OutsideRecord {
        /// The 1-based line.
        line: usize,
        /// The field's name.
        field: String,
    },
    /// An `SF` while another record is open.
    #[error("line {line}: SF while the record for {open} is still open")]
    Nested {
        /// The 1-based line.
        line: usize,
        /// The source file of the open record.
        open: String,
    },
    /// A summary field given twice in one record.
    #[error("line {line}: {field} given twice in one record")]
    Repeated {
        /// The 1-based line.
        line: usize,
        /// The field's name.
        field: String,
    },
    /// A summary field whose value is not a count.
    #[error("line {line}: {field} is not a count: {value:?}")]
    BadCount {
        /// The 1-based line.
        line: usize,
        /// The field's name.
        field: String,
        /// The text that is not a count.
        value: String,
    },
    /// A `DA` or `BRDA` line not in its shape.
    #[error("line {line}: {field} is not in the form {form}: {value:?}")]
    BadDetail {
        /// The 1-based line.
        line: usize,
        /// `DA` or `BRDA`.
        field: String,
        /// The form it must have.
        form: &'static str,
        /// The text after the colon.
        value: String,
    },
    /// A record that hits more items than it found.
    #[error("line {line}: record for {path} hits {hit} of {found} {what}")]
    HitAboveFound {
        /// The 1-based line.
        line: usize,
        /// The record's source file.
        path: String,
        /// `lines` or `branches`.
        what: &'static str,
        /// The items found.
        found: u64,
        /// The items hit.
        hit: u64,
    },
    /// Two records for one source file.
    #[error("{path} appears in two records")]
    Duplicate {
        /// The source file.
        path: String,
    },
    /// A record without `end_of_record`.
    #[error("the record for {path} has no end_of_record")]
    Unterminated {
        /// The record's source file.
        path: String,
    },
    /// A tracefile with no records.
    #[error("the tracefile holds no records")]
    Empty,
}

/// A record being read: the source path, each summary field seen so far, and what
/// its detail lines named.
struct Open {
    path: String,
    /// LF, LH, BRF, BRH in that order.
    fields: [Option<u64>; 4],
    uncovered: Uncovered,
}

const FIELDS: [&str; 4] = ["LF", "LH", "BRF", "BRH"];

impl Open {
    fn close(self, line: usize) -> Result<FileRecord, LcovError> {
        let [lf, lh, brf, brh] = self.fields.map(|f| f.unwrap_or(0));
        let counts = |what, found, hit| {
            Counts::new(found, hit).ok_or_else(|| LcovError::HitAboveFound {
                line,
                path: self.path.clone(),
                what,
                found,
                hit,
            })
        };
        let coverage = CrateCoverage {
            lines: counts("lines", lf, lh)?,
            branches: counts("branches", brf, brh)?,
        };
        Ok(FileRecord {
            path: PathBuf::from(&self.path),
            coverage,
            uncovered: self.uncovered,
        })
    }
}

const DA_FORM: &str = "<line>,<count>[,<checksum>]";
const BRDA_FORM: &str = "<line>,<block>,<branch>,<taken>";

/// A source line number: decimal, from 1.
fn line_number(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|n| *n > 0)
}

/// Adds what one `DA` value names; `None` when it is not in [`DA_FORM`].
fn read_da(value: &str, into: &mut Uncovered) -> Option<()> {
    let mut parts = value.splitn(3, ',');
    let line = line_number(parts.next()?)?;
    let count = parts.next()?.parse::<u64>().ok()?;
    if count == 0 {
        into.lines.insert(line);
    }
    Some(())
}

/// Adds what one `BRDA` value names; `None` when it is not in [`BRDA_FORM`].
fn read_brda(value: &str, into: &mut Uncovered) -> Option<()> {
    let parts: Vec<&str> = value.split(',').collect();
    let [line, block, branch, taken] = parts.as_slice() else {
        return None;
    };
    let at = Branch {
        line: line_number(line)?,
        block: block.parse().ok()?,
        branch: branch.parse().ok()?,
    };
    let taken = match *taken {
        "-" => 0,
        t => t.parse::<u64>().ok()?,
    };
    if taken == 0 {
        into.branches.insert(at);
    }
    Some(())
}

/// Parses a tracefile; see the module docs for what it refuses.
pub fn parse(text: &str) -> Result<Vec<FileRecord>, LcovError> {
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    let mut open: Option<Open> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim_end();
        if raw == "end_of_record" {
            let record = open.take().ok_or_else(|| LcovError::OutsideRecord {
                line,
                field: raw.to_owned(),
            })?;
            records.push(record.close(line)?);
            continue;
        }
        let Some((field, value)) = raw.split_once(':') else {
            continue;
        };
        if field == "SF" {
            if let Some(o) = &open {
                return Err(LcovError::Nested {
                    line,
                    open: o.path.clone(),
                });
            }
            if !seen.insert(value.to_owned()) {
                return Err(LcovError::Duplicate {
                    path: value.to_owned(),
                });
            }
            open = Some(Open {
                path: value.to_owned(),
                fields: [None; 4],
                uncovered: Uncovered::default(),
            });
            continue;
        }
        if field == "DA" || field == "BRDA" {
            let record = open.as_mut().ok_or_else(|| LcovError::OutsideRecord {
                line,
                field: field.to_owned(),
            })?;
            let into = &mut record.uncovered;
            let (read, form) = if field == "DA" {
                (read_da(value, into), DA_FORM)
            } else {
                (read_brda(value, into), BRDA_FORM)
            };
            read.ok_or_else(|| LcovError::BadDetail {
                line,
                field: field.to_owned(),
                form,
                value: value.to_owned(),
            })?;
            continue;
        }
        let Some(slot) = FIELDS.iter().position(|f| *f == field) else {
            continue;
        };
        let record = open.as_mut().ok_or_else(|| LcovError::OutsideRecord {
            line,
            field: field.to_owned(),
        })?;
        let count = value.parse::<u64>().map_err(|_| LcovError::BadCount {
            line,
            field: field.to_owned(),
            value: value.to_owned(),
        })?;
        if record.fields[slot].replace(count).is_some() {
            return Err(LcovError::Repeated {
                line,
                field: field.to_owned(),
            });
        }
    }
    if let Some(o) = open {
        return Err(LcovError::Unterminated { path: o.path });
    }
    if records.is_empty() {
        return Err(LcovError::Empty);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
        CrateCoverage {
            lines: Counts::new(lf, lh).expect("valid"),
            branches: Counts::new(brf, brh).expect("valid"),
        }
    }

    fn br(line: u64, block: u64, branch: u64) -> Branch {
        Branch {
            line,
            block,
            branch,
        }
    }

    /// Asserts that none of these defects is present: summary fields read into the wrong slot, a detail line (`DA`, `BRDA`)
    /// counted as a summary, a second record merged into the first, or a run line or
    /// taken branch named as uncovered.
    #[test]
    fn reads_each_record_summary() {
        let text = "TN:\nSF:/w/crates/a/src/lib.rs\nFN:1,f\nDA:1,3\nDA:2,0\nBRDA:1,0,0,1\n\
                    BRDA:1,0,1,-\nLF:2\nLH:1\nBRF:2\nBRH:1\nend_of_record\n\
                    SF:/w/crates/b/src/lib.rs\nLH:4\nLF:5\nend_of_record\n";
        let got = parse(text).expect("valid");
        assert_eq!(
            got,
            vec![
                FileRecord {
                    path: "/w/crates/a/src/lib.rs".into(),
                    coverage: cov(2, 1, 2, 1),
                    uncovered: Uncovered {
                        lines: [2].into(),
                        branches: [br(1, 0, 1)].into(),
                    },
                },
                // No BRF/BRH: branch coverage was off for this file; zero, not an error.
                FileRecord {
                    path: "/w/crates/b/src/lib.rs".into(),
                    coverage: cov(5, 4, 0, 0),
                    uncovered: Uncovered::default(),
                },
            ]
        );
    }

    /// Asserts that the detail lines name every line with count 0 and every branch taken
    /// `0` or `-`, each once and in source order, whatever order the file gives them in;
    /// that a `DA` checksum is accepted; and that a run line or a taken branch (count 1
    /// and above, up to `u64::MAX`) is not named.
    #[test]
    fn names_each_uncovered_line_and_branch_once_in_order() {
        let text = "SF:/w/crates/a/src/lib.rs\nDA:9,0\nDA:3,0,abc==\nDA:4,1\nDA:9,0\n\
                    DA:5,18446744073709551615\nBRDA:7,1,3,0\nBRDA:7,0,0,-\nBRDA:7,0,1,2\n\
                    BRDA:2,0,1,0\nBRDA:7,1,3,0\nend_of_record\n";
        let got = parse(text).expect("valid");
        assert_eq!(
            got[0].uncovered,
            Uncovered {
                lines: [3, 9].into(),
                branches: [br(2, 0, 1), br(7, 0, 0), br(7, 1, 3)].into(),
            }
        );
        let lines: Vec<u64> = got[0].uncovered.lines.iter().copied().collect();
        assert_eq!(lines, [3, 9]);
        let branches: Vec<Branch> = got[0].uncovered.branches.iter().copied().collect();
        assert_eq!(branches, [br(2, 0, 1), br(7, 0, 0), br(7, 1, 3)]);
        // Counts still come from the summaries alone: no LF/LH here, so zero.
        assert_eq!(got[0].coverage, cov(0, 0, 0, 0));
    }

    /// Asserts that each malformed `DA` or `BRDA` shape is refused with its line, field,
    /// form and text, rather than skipped (which would name the wrong place or none), and
    /// that a detail line outside a record is refused like a summary line.
    #[test]
    fn refuses_malformed_detail_lines() {
        for (value, field, form) in [
            ("DA:", "DA", DA_FORM),
            ("DA:7", "DA", DA_FORM),
            ("DA:0,0", "DA", DA_FORM),
            ("DA:x,0", "DA", DA_FORM),
            ("DA:7,-1", "DA", DA_FORM),
            ("DA:7,", "DA", DA_FORM),
            ("BRDA:7,0,0", "BRDA", BRDA_FORM),
            ("BRDA:7,0,0,0,9", "BRDA", BRDA_FORM),
            ("BRDA:0,0,0,0", "BRDA", BRDA_FORM),
            ("BRDA:7,x,0,0", "BRDA", BRDA_FORM),
            ("BRDA:7,0,x,0", "BRDA", BRDA_FORM),
            ("BRDA:7,0,0,x", "BRDA", BRDA_FORM),
        ] {
            let text = format!("SF:a\n{value}\nend_of_record\n");
            let (_, rest) = value.split_once(':').expect("field");
            assert_eq!(
                err(&text),
                LcovError::BadDetail {
                    line: 2,
                    field: field.to_owned(),
                    form,
                    value: rest.to_owned(),
                },
                "{value}"
            );
        }
        assert_eq!(
            err("SF:a\nDA:x\n").to_string(),
            "line 2: DA is not in the form <line>,<count>[,<checksum>]: \"x\""
        );
        assert_eq!(
            err("SF:a\nBRDA:1\n").to_string(),
            "line 2: BRDA is not in the form <line>,<block>,<branch>,<taken>: \"1\""
        );
        for outside in ["DA:1,0\n", "BRDA:1,0,0,0\n"] {
            let field = outside.split_once(':').expect("field").0;
            assert_eq!(
                err(outside),
                LcovError::OutsideRecord {
                    line: 1,
                    field: field.to_owned()
                }
            );
        }
    }

    /// Asserts that none of these defects is present: trailing whitespace or CRLF line ends breaking `end_of_record`.
    #[test]
    fn tolerates_crlf() {
        let got = parse("SF:/w/x.rs\r\nLF:1\r\nLH:1\r\nend_of_record\r\n").expect("valid");
        assert_eq!(got[0].coverage, cov(1, 1, 0, 0));
    }

    fn err(text: &str) -> LcovError {
        parse(text).expect_err(text)
    }

    /// Asserts that none of these defects is present: each malformed shape the module docs list being accepted, which would
    /// let a damaged report pass the ratchet with a wrong total.
    #[test]
    fn refuses_malformed_tracefiles() {
        let field = |f: &str| f.to_owned();
        assert_eq!(
            err("LF:3\n"),
            LcovError::OutsideRecord {
                line: 1,
                field: field("LF")
            }
        );
        assert_eq!(
            err("SF:a\nend_of_record\nend_of_record\n"),
            LcovError::OutsideRecord {
                line: 3,
                field: field("end_of_record")
            }
        );
        assert_eq!(
            err("SF:a\nSF:b\n"),
            LcovError::Nested {
                line: 2,
                open: field("a")
            }
        );
        assert_eq!(
            err("SF:a\nLH:1\nLH:1\n"),
            LcovError::Repeated {
                line: 3,
                field: field("LH")
            }
        );
        assert_eq!(
            err("SF:a\nBRF:-1\n"),
            LcovError::BadCount {
                line: 2,
                field: field("BRF"),
                value: field("-1")
            }
        );
        assert_eq!(
            err("SF:a\nLF:1\nLH:2\nend_of_record\n"),
            LcovError::HitAboveFound {
                line: 4,
                path: field("a"),
                what: "lines",
                found: 1,
                hit: 2
            }
        );
        assert_eq!(
            err("SF:a\nBRF:1\nBRH:2\nend_of_record\n"),
            LcovError::HitAboveFound {
                line: 4,
                path: field("a"),
                what: "branches",
                found: 1,
                hit: 2
            }
        );
        assert_eq!(
            err("SF:a\nend_of_record\nSF:a\nend_of_record\n"),
            LcovError::Duplicate { path: field("a") }
        );
        assert_eq!(
            err("SF:a\nLF:1\n"),
            LcovError::Unterminated { path: field("a") }
        );
        assert_eq!(err(""), LcovError::Empty);
        assert_eq!(err("TN:\nnoise\n"), LcovError::Empty);
    }

    /// Asserts that none of these defects is present: an error message that drops the location or the values a reader needs.
    #[test]
    fn errors_name_the_place() {
        let messages = [
            err("LF:3\n").to_string(),
            err("SF:a\nSF:b\n").to_string(),
            err("SF:a\nLH:1\nLH:1\n").to_string(),
            err("SF:a\nBRF:x\n").to_string(),
            err("SF:a\nLF:1\nLH:2\nend_of_record\n").to_string(),
            err("SF:a\nend_of_record\nSF:a\nend_of_record\n").to_string(),
            err("SF:a\n").to_string(),
            err("").to_string(),
        ];
        assert_eq!(
            messages,
            [
                "line 1: LF outside a record (no SF before it)",
                "line 2: SF while the record for a is still open",
                "line 3: LH given twice in one record",
                "line 2: BRF is not a count: \"x\"",
                "line 4: record for a hits 2 of 1 lines",
                "a appears in two records",
                "the record for a has no end_of_record",
                "the tracefile holds no records",
            ]
        );
    }
}
