//! `js-coverage` end to end on a fixture crate with one uncovered line and one
//! uncovered branch: the failing run names both, by path and line, in its output and
//! in the job summary, and a passing run names nothing.

use super::{run, stderr, stdout, workspace};

/// Rewrites the fixture's tracefile so crate `a` has a line (12) and a branch (line 11,
/// block 0, branch 1) no test runs, with the detail lines llvm-cov writes for them.
fn tracefile_with_one_miss(root: &std::path::Path) {
    let r = root.display();
    let lcov = format!(
        "SF:{r}/crates/a/src/lib.rs\nDA:10,4\nDA:11,4\nDA:12,0\nDA:13,4\n\
         BRDA:11,0,0,4\nBRDA:11,0,1,0\nBRF:2\nBRH:1\nLF:4\nLH:3\nend_of_record\n\
         SF:{r}/crates/b/src/main.rs\nDA:1,1\nLF:1\nLH:1\nend_of_record\n"
    );
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
}

const SECTION: &str = "\n#### Uncovered in a\n\n\
                       1 line and 1 branch no test runs (MORE MISSED THAN BASELINE (lines, branches)).\n\
                       \nLines:\n\
                       - crates/a/src/lib.rs:12\n\
                       \nBranches:\n\
                       - crates/a/src/lib.rs:11 block 0 branch 1\n";

/// Asserts that a crate whose new line and branch no test runs fails the ratchet with
/// both named (`crates/a/src/lib.rs:12` and line 11's branch) right after the table,
/// that the job summary gets the same section, and that the crate that passes (`b`) is
/// not listed.
#[test]
fn a_failing_crate_names_its_uncovered_line_and_branch() {
    let root = workspace("names-misses");
    tracefile_with_one_miss(&root);
    let summary = root.join("summary.md");
    let o = run(
        &root,
        "a 0 0\nb 0 -\nempty - -\n",
        &["--summary-file", summary.to_str().expect("UTF-8")],
    );
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    let table_end = "\ncoverage ratchet: FAIL (1 crate(s) need attention; see status)\n";
    assert!(out.contains(&format!("{table_end}{SECTION}\n")), "{out}");
    assert!(!out.contains("Uncovered in b"), "{out}");
    let written = std::fs::read_to_string(&summary).expect("read summary");
    assert_eq!(written, out);
}

/// Asserts that a run that passes, with the same uncovered line and branch recorded in
/// the baseline, names nothing: the list is for failures only.
#[test]
fn a_passing_run_names_nothing() {
    let root = workspace("names-nothing");
    tracefile_with_one_miss(&root);
    let o = run(&root, "a 1 1\nb 0 -\nempty - -\n", &[]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(!out.contains("Uncovered"), "{out}");
    assert!(out.ends_with("coverage ratchet: PASS\n"), "{out}");
}
