//! `js-coverage against-base` end to end: the change's baseline file against the base
//! branch's, as the CI coverage job runs it on a pull request.

use std::path::Path;
use std::process::{Command, Output};

use super::{stderr, stdout, workspace};

fn against_base(root: &Path, base: &str, head: &str) -> Output {
    std::fs::write(root.join("base-baseline"), base).expect("write base");
    std::fs::write(root.join("head-baseline"), head).expect("write head");
    Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("against-base")
        .arg("--base")
        .arg(root.join("base-baseline"))
        .arg("--head")
        .arg(root.join("head-baseline"))
        .output()
        .expect("run js-coverage")
}

/// Asserts that a head file equal to the base's, or one that lowers a count, adds a
/// crate at zero or removes a crate, exits 0 and prints the PASS line.
#[test]
fn passes_when_nothing_rises() {
    let root = workspace("base-passes");
    for head in [
        "a 3 0\nb 0 -\n",
        "a 1 0\nb 0 -\n",
        "a 3 0\nb 0 -\nn 0 0\n",
        "a 3 0\n",
    ] {
        let o = against_base(&root, "a 3 0\nb 0 -\n", head);
        assert_eq!(
            o.status.code(),
            Some(0),
            "{head}: {}{}",
            stdout(&o),
            stderr(&o)
        );
        assert_eq!(stdout(&o), "coverage against base: PASS\n", "{head}");
    }
}

/// Asserts that a pull request that adds uncovered lines and edits its own baseline
/// file to match (`xtask 0 0` to `xtask 10 0`) exits 1, naming the crate, the metric
/// and both counts, and that a new crate entering above zero does the same.
#[test]
fn fails_when_a_count_rises_or_a_new_crate_is_not_at_zero() {
    let root = workspace("base-fails");
    let o = against_base(&root, "xtask 0 0\n", "xtask 10 0\n");
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("xtask: missed lines rose from 0 on the base branch to 10;"),
        "{out}"
    );
    assert!(out.ends_with("coverage against base: FAIL\n"), "{out}");

    let o = against_base(&root, "xtask 0 0\n", "xtask 0 0\nnew 0 4\n");
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("new: a new crate enters at zero, but it records 4 missed branches;"),
        "{}",
        stdout(&o)
    );
}

/// Asserts that an unreadable or malformed file exits 2 with a reason, not 0 or 1, so a
/// missing base file cannot pass the gate.
#[test]
fn bad_inputs_exit_2() {
    let root = workspace("base-bad");
    let o = against_base(&root, "a 1 0\n", "a 1\n");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("head-baseline"), "{}", stderr(&o));
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .args(["against-base", "--base"])
        .arg(root.join("absent"))
        .arg("--head")
        .arg(root.join("head-baseline"))
        .output()
        .expect("run js-coverage");
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("read "), "{}", stderr(&o));
}
