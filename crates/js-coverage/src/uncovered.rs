//! The lines and branches no test runs, for each crate the ratchet fails on a rise, so
//! the log of a failing run says where the new misses are.
//!
//! A crate is listed when its status is [`Status::MoreMissed`] or [`Status::Unrecorded`]
//! (a new crate, which must enter at zero) and it misses something. A crate whose
//! baseline is only looser than measured is not: `tighten` fixes it, and no line needs
//! a test.
//!
//! The places come from the tracefile's detail lines ([`crate::lcov`]): each line with
//! a `DA` count of 0 as `<path>:<line>`, and each branch with a `BRDA` taken count of
//! `0` or `-` as `<path>:<line> block <b> branch <n>`, paths relative to the root, in
//! path and then source order. Each list stops at the cap and says how many more there
//! are. llvm-cov's summaries can count misses no detail line names (a line one closure
//! or generic instantiation leaves unrun while other code on it runs); each file where
//! the summary counts more than its detail lines name is listed with that difference,
//! so the miss is at least narrowed to a file.

use std::fmt::Write as _;
use std::path::Path;

use crate::lcov::FileRecord;
use crate::ratchet::{Row, Status, crate_of};

/// How many entries each list shows per crate before it says how many more there are.
pub const CAP: usize = 50;

/// What one crate's records name, and miss without naming.
#[derive(Default)]
struct Places {
    lines: Vec<String>,
    branches: Vec<String>,
    /// Per file: the path and the lines and branches its summary counts beyond what
    /// its detail lines name.
    unnamed: Vec<(String, u64, u64)>,
}

fn places(root: &Path, krate: &str, files: &[FileRecord]) -> Places {
    let base = root.join("crates");
    let mut mine: Vec<(String, &FileRecord)> = files
        .iter()
        .filter(|f| crate_of(&base, &f.path) == Some(krate))
        .filter_map(|f| {
            let rel = Path::new("crates").join(f.path.strip_prefix(&base).ok()?);
            Some((rel.display().to_string(), f))
        })
        .collect();
    mine.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Places::default();
    for (path, f) in mine {
        let u = &f.uncovered;
        out.lines
            .extend(u.lines.iter().map(|line| format!("{path}:{line}")));
        out.branches.extend(
            u.branches
                .iter()
                .map(|b| format!("{path}:{} block {} branch {}", b.line, b.block, b.branch)),
        );
        let beyond = |missed: Option<u64>, named: usize| {
            missed
                .unwrap_or(0)
                .saturating_sub(u64::try_from(named).unwrap_or(u64::MAX))
        };
        let lines = beyond(f.coverage.lines.missed(), u.lines.len());
        let branches = beyond(f.coverage.branches.missed(), u.branches.len());
        if lines > 0 || branches > 0 {
            out.unnamed.push((path, lines, branches));
        }
    }
    out
}

/// `n` and the noun, singular for 1.
fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Writes `items` as a Markdown list under `title`, at most `cap` of them, then how
/// many more there are; nothing when `items` is empty.
fn list(out: &mut String, title: &str, items: &[String], cap: usize) {
    if items.is_empty() {
        return;
    }
    // Writing to a String cannot fail.
    let _ = writeln!(out, "\n{title}:");
    for item in items.iter().take(cap) {
        let _ = writeln!(out, "- {item}");
    }
    if items.len() > cap {
        let _ = writeln!(out, "- and {} more", items.len() - cap);
    }
}

/// The section for every crate in `rows` the ratchet fails on a rise (see the module
/// docs), from the records in `files` under `root` (the tracefile's spelling of the
/// workspace root); empty when there is none.
pub fn render(root: &Path, rows: &[Row], files: &[FileRecord], cap: usize) -> String {
    let mut out = String::new();
    for row in rows {
        let status = row.status();
        let (Status::MoreMissed(_) | Status::Unrecorded, Some(measured)) = (&status, row.measured)
        else {
            continue;
        };
        let lines = measured.lines.missed().unwrap_or(0);
        let branches = measured.branches.missed().unwrap_or(0);
        if lines == 0 && branches == 0 {
            continue;
        }
        let p = places(root, &row.krate, files);
        let _ = writeln!(
            out,
            "\n#### Uncovered in {}\n\n{} and {} no test runs ({status}).",
            row.krate,
            count(lines, "line", "lines"),
            count(branches, "branch", "branches"),
        );
        list(&mut out, "Lines", &p.lines, cap);
        list(&mut out, "Branches", &p.branches, cap);
        let unnamed: Vec<String> = p
            .unnamed
            .iter()
            .map(|(path, l, b)| {
                format!(
                    "{path}: {} and {}",
                    count(*l, "line", "lines"),
                    count(*b, "branch", "branches")
                )
            })
            .collect();
        list(
            &mut out,
            "Counted, but on no DA or BRDA line (code that one closure or generic \
             instantiation leaves unrun while other code on its line runs)",
            &unnamed,
            cap,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::Entry;
    use crate::lcov::{Branch, Uncovered};
    use crate::{Counts, CrateCoverage};

    fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
        CrateCoverage {
            lines: Counts::new(lf, lh).expect("valid"),
            branches: Counts::new(brf, brh).expect("valid"),
        }
    }

    fn file(
        path: &str,
        coverage: CrateCoverage,
        lines: &[u64],
        branches: &[(u64, u64, u64)],
    ) -> FileRecord {
        FileRecord {
            path: path.into(),
            coverage,
            uncovered: Uncovered {
                lines: lines.iter().copied().collect(),
                branches: branches
                    .iter()
                    .map(|&(line, block, branch)| Branch {
                        line,
                        block,
                        branch,
                    })
                    .collect(),
            },
        }
    }

    fn row(krate: &str, measured: Option<CrateCoverage>, recorded: Option<(u64, u64)>) -> Row {
        Row {
            krate: krate.into(),
            measured,
            recorded: recorded.map(|(l, b)| Entry {
                lines: Some(l),
                branches: Some(b),
            }),
        }
    }

    /// Asserts that a crate missing more than its baseline is listed with every uncovered
    /// line and branch by root-relative path, in path and then source order (`a/x.rs`
    /// before `b.rs`, line 3 before 10), and that a file whose detail lines name
    /// everything its summary counts has no entry under the unnamed list.
    #[test]
    fn names_each_place_of_a_crate_that_misses_more() {
        let files = [
            file("/w/crates/k/src/b.rs", cov(5, 4, 2, 1), &[2], &[(1, 0, 1)]),
            file("/w/crates/k/src/a/x.rs", cov(9, 7, 0, 0), &[10, 3], &[]),
            file("/w/crates/other/src/lib.rs", cov(1, 0, 0, 0), &[1], &[]),
        ];
        let rows = [
            row("k", Some(cov(14, 11, 2, 1)), Some((0, 0))),
            row("other", Some(cov(1, 0, 0, 0)), Some((1, 0))),
        ];
        assert_eq!(
            render(Path::new("/w"), &rows, &files, CAP),
            "\n#### Uncovered in k\n\n\
             3 lines and 1 branch no test runs (MORE MISSED THAN BASELINE (lines, branches)).\n\
             \nLines:\n\
             - crates/k/src/a/x.rs:3\n\
             - crates/k/src/a/x.rs:10\n\
             - crates/k/src/b.rs:2\n\
             \nBranches:\n\
             - crates/k/src/b.rs:1 block 0 branch 1\n"
        );
    }

    /// Asserts that a miss the summary counts but no detail line names is narrowed to its
    /// file with the difference, for lines and for branches, and that the named ones are
    /// still listed beside it.
    #[test]
    fn narrows_a_miss_no_detail_line_names_to_its_file() {
        let files = [
            file("/w/crates/k/src/g.rs", cov(209, 208, 20, 18), &[], &[]),
            file(
                "/w/crates/k/src/h.rs",
                cov(4, 2, 4, 2),
                &[3],
                &[(1, 0, 0), (1, 1, 2)],
            ),
            file("/w/crates/k/src/i.rs", cov(2, 1, 3, 1), &[1], &[(1, 0, 0)]),
        ];
        let rows = [row("k", Some(cov(215, 211, 27, 21)), Some((0, 0)))];
        assert_eq!(
            render(Path::new("/w"), &rows, &files, CAP),
            "\n#### Uncovered in k\n\n\
             4 lines and 6 branches no test runs (MORE MISSED THAN BASELINE (lines, branches)).\n\
             \nLines:\n\
             - crates/k/src/h.rs:3\n\
             - crates/k/src/i.rs:1\n\
             \nBranches:\n\
             - crates/k/src/h.rs:1 block 0 branch 0\n\
             - crates/k/src/h.rs:1 block 1 branch 2\n\
             - crates/k/src/i.rs:1 block 0 branch 0\n\
             \nCounted, but on no DA or BRDA line (code that one closure or generic \
             instantiation leaves unrun while other code on its line runs):\n\
             - crates/k/src/g.rs: 1 line and 2 branches\n\
             - crates/k/src/h.rs: 1 line and 0 branches\n\
             - crates/k/src/i.rs: 0 lines and 1 branch\n"
        );
    }

    /// Asserts that each list stops at the cap and says how many more there are, that a
    /// list of exactly the cap has no such line, and that a cap of zero still says how
    /// many there are.
    #[test]
    fn each_list_stops_at_the_cap() {
        let lines: Vec<u64> = (1..=5).collect();
        let files = [file(
            "/w/crates/k/src/lib.rs",
            cov(9, 4, 3, 0),
            &lines,
            &[(1, 0, 0), (1, 0, 1), (2, 0, 0)],
        )];
        let rows = [row("k", Some(cov(9, 4, 3, 0)), Some((0, 0)))];
        let header = "\n#### Uncovered in k\n\n\
                      5 lines and 3 branches no test runs (MORE MISSED THAN BASELINE (lines, branches)).\n";
        assert_eq!(
            render(Path::new("/w"), &rows, &files, 3),
            format!(
                "{header}\nLines:\n\
                 - crates/k/src/lib.rs:1\n\
                 - crates/k/src/lib.rs:2\n\
                 - crates/k/src/lib.rs:3\n\
                 - and 2 more\n\
                 \nBranches:\n\
                 - crates/k/src/lib.rs:1 block 0 branch 0\n\
                 - crates/k/src/lib.rs:1 block 0 branch 1\n\
                 - crates/k/src/lib.rs:2 block 0 branch 0\n"
            )
        );
        assert_eq!(
            render(Path::new("/w"), &rows, &files, 0),
            format!("{header}\nLines:\n- and 5 more\n\nBranches:\n- and 3 more\n")
        );
        assert_eq!(CAP, 50);
    }

    /// Asserts which crates are listed: a rise and a new crate that misses a line or only a
    /// branch are;
    /// a crate that passes, one whose baseline is only looser, one no longer in the
    /// workspace, and a new crate that misses nothing are not. Singular nouns for one.
    #[test]
    fn lists_only_crates_failing_on_a_rise() {
        let files = [
            file("/w/crates/new/src/lib.rs", cov(1, 0, 0, 0), &[1], &[]),
            file("/w/crates/ok/src/lib.rs", cov(2, 1, 0, 0), &[1], &[]),
            file("/w/crates/loose/src/lib.rs", cov(2, 1, 0, 0), &[1], &[]),
            file("/w/crates/clean/src/lib.rs", cov(2, 2, 0, 0), &[], &[]),
            file(
                "/w/crates/br/src/lib.rs",
                cov(2, 2, 2, 1),
                &[],
                &[(5, 0, 1)],
            ),
        ];
        let rows = [
            row("br", Some(cov(2, 2, 2, 1)), None),
            row("clean", Some(cov(2, 2, 0, 0)), None),
            row("gone", None, Some((1, 1))),
            row("loose", Some(cov(2, 1, 0, 0)), Some((2, 0))),
            row("new", Some(cov(1, 0, 0, 0)), None),
            row("ok", Some(cov(2, 1, 0, 0)), Some((1, 0))),
        ];
        assert_eq!(
            render(Path::new("/w"), &rows, &files, CAP),
            "\n#### Uncovered in br\n\n\
             0 lines and 1 branch no test runs (NOT IN BASELINE).\n\
             \nBranches:\n- crates/br/src/lib.rs:5 block 0 branch 1\n\
             \n#### Uncovered in new\n\n\
             1 line and 0 branches no test runs (NOT IN BASELINE).\n\
             \nLines:\n- crates/new/src/lib.rs:1\n"
        );
        assert_eq!(render(Path::new("/w"), &rows[1..3], &files, CAP), "");
    }

    /// Asserts that a crate measured as missing something with no record of its own (as
    /// llvm-cov never writes, but a hand-made file can) still gets its header, with no
    /// lists, and that a record under another root is not taken for the crate's.
    #[test]
    fn a_failing_crate_without_records_gets_its_header_only() {
        let rows = [row("k", Some(cov(1, 0, 0, 0)), Some((0, 0)))];
        assert_eq!(
            render(Path::new("/w"), &rows, &[], CAP),
            "\n#### Uncovered in k\n\n1 line and 0 branches no test runs (MORE MISSED THAN BASELINE (lines)).\n"
        );
        let elsewhere = [file("/v/crates/k/src/lib.rs", cov(1, 0, 0, 0), &[1], &[])];
        assert_eq!(
            render(Path::new("/w"), &rows, &elsewhere, CAP),
            render(Path::new("/w"), &rows, &[], CAP)
        );
    }
}
