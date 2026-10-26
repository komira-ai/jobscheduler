//! The `js-coverage` binary end to end, on a small workspace built in the test's
//! temporary directory (inside the target directory).
//!
//! Asserts that none of these defects is present: the binary passing a run the ratchet fails (a crate missing more, or a
//! baseline looser than measured) or the reverse, exiting 1 on a bad input instead of
//! 2, a measured baseline that the binary cannot read back as passing, a directory
//! under `crates/` without `Cargo.toml` taken for a crate, a crate with no records
//! left out of the table, and a `--summary-file` truncated, left without the table or
//! not written.
//!
//! Each test builds its own workspace in a fresh directory named for the test and the
//! process, so no test reads a file an earlier run (or a concurrent one) left behind.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod relative_root;
mod unmeasured;

/// A fixture workspace, removed when dropped.
struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Workspace {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// A workspace with crates `a`, `b` and `empty` (no records), plus a loose file and a
/// directory without `Cargo.toml` under `crates/`, which are not crates. It lives in
/// `<name>-<pid>`, emptied first: a directory left by a run that was killed, or by an
/// earlier process with the same pid, never leaks files into this one.
fn workspace(name: &str) -> Workspace {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    // Absent on a normal run; anything else surfaces as the create or write below
    // failing, or as the `measured` freshness check.
    let _ = std::fs::remove_dir_all(&root);
    for krate in ["a", "b", "empty"] {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(&dir).expect("create crate dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("write Cargo.toml");
    }
    std::fs::create_dir_all(root.join("crates/notacrate")).expect("create dir");
    std::fs::write(root.join("crates/notes.txt"), "").expect("write file");
    let r = root.display();
    let lcov = format!(
        "SF:{r}/crates/a/src/lib.rs\nLF:4\nLH:3\nBRF:2\nBRH:2\nend_of_record\n\
         SF:{r}/crates/b/src/main.rs\nLF:10\nLH:10\nend_of_record\n"
    );
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
    Workspace(root)
}

fn run(root: &Path, baseline: &str, extra: &[&str]) -> Output {
    std::fs::write(root.join("coverage-baseline"), baseline).expect("write baseline");
    Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .args(extra)
        .output()
        .expect("run js-coverage")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `a` misses 1 of 4 lines and none of 2 branches; `b` misses none of 10 lines.
const MATCHING: &str = "a 1 0\nb 0 -\nempty - -\n";

/// Asserts that a run whose coverage equals the baseline exits 0 and prints the table.
#[test]
fn passes_when_coverage_matches_the_baseline() {
    let root = workspace("matches");
    let o = run(&root, MATCHING, &[]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("| a | 3/4 | 75.00 | 25.00 | 1 | 2/2 | 100.00 | 0.00 | 0 | 1 / 0 | ok |"),
        "{out}"
    );
    assert!(
        out.contains("| empty | - | - | - | - | - | - | - | - | - / - | ok |"),
        "{out}"
    );
    assert!(
        !out.contains("notacrate") && !out.contains("notes"),
        "{out}"
    );
    assert!(out.ends_with("coverage ratchet: PASS\n"), "{out}");
}

/// Asserts that a run fails when a crate misses more than its baseline records, as when a
/// test is deleted from crate `a`.
#[test]
fn fails_when_a_crate_drops_below_its_baseline() {
    let root = workspace("drops");
    let o = run(&root, "a 0 0\nb 0 -\nempty - -\n", &[]);
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("MORE MISSED THAN BASELINE (lines)"),
        "{}",
        stdout(&o)
    );
}

/// Asserts that a baseline with slack fails the run. `a` misses 1 line
/// but the file allows 2, and `b` has measured lines where the file says `-`; either
/// would let a later change leave that much new code uncovered.
#[test]
fn fails_when_the_baseline_is_looser_than_measured() {
    let root = workspace("loose");
    for (baseline, want) in [
        (
            "a 2 0\nb 0 -\nempty - -\n",
            "| a | 3/4 | 75.00 | 25.00 | 1 | 2/2 | 100.00 | 0.00 | 0 | 2 / 0 | BASELINE LOOSER THAN MEASURED (lines); run js-coverage tighten |",
        ),
        (
            "a 1 0\nb - -\nempty - -\n",
            "| b | 10/10 | 100.00 | 0.00 | 0 | - | - | - | - | - / - | BASELINE LOOSER THAN MEASURED (lines); run js-coverage tighten |",
        ),
    ] {
        let o = run(&root, baseline, &[]);
        assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
        let out = stdout(&o);
        assert!(out.contains(want), "{out}");
        assert!(
            out.contains("\ncoverage ratchet: FAIL (1 crate(s) need attention; see status)\n"),
            "{out}"
        );
    }
}

/// Asserts that the file `--write-baseline` writes is accepted as the baseline of the next run, even after a failing run.
#[test]
fn measured_baseline_reads_back_as_passing() {
    let root = workspace("writes");
    let measured = root.join("measured");
    assert!(!measured.exists(), "fresh workspace");
    // An empty baseline fails (every crate unrecorded) but still writes the file.
    let o = run(
        &root,
        "",
        &["--write-baseline", measured.to_str().expect("UTF-8")],
    );
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("NOT IN BASELINE"), "{}", stdout(&o));
    let written = std::fs::read_to_string(&measured).expect("a failing run writes the file");
    let o = run(&root, &written, &[]);
    assert_eq!(o.status.code(), Some(0), "{written}\n{}", stdout(&o));
}

/// Asserts that unreadable or malformed inputs exit 2 with a reason, not 1.
#[test]
fn bad_inputs_exit_2_with_a_reason() {
    let root = workspace("bad");
    let expect_2 = |o: Output, want: &str| {
        assert_eq!(o.status.code(), Some(2), "{want}: {}", stderr(&o));
        assert!(stderr(&o).contains(want), "{want}: {}", stderr(&o));
    };
    expect_2(
        run(&root, "a 75.00 -\n", &[]),
        "coverage-baseline: line 1: not a count",
    );
    expect_2(
        run(&root, MATCHING, &["--write-baseline", "/nonexistent-dir/x"]),
        "write /nonexistent-dir/x:",
    );
    std::fs::remove_file(root.join("coverage-baseline")).expect("remove baseline");
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(&*root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run js-coverage");
    expect_2(o, "coverage-baseline: No such file");

    // A root without a crates directory: the crates cannot be listed.
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(root.join("crates/a"))
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run js-coverage");
    expect_2(o, "list crates under");

    // A tracefile that does not parse, one naming a file outside every crate, and a
    // missing one.
    for (lcov, want) in [
        ("", "lcov.info: the tracefile holds no records"),
        (
            "SF:/elsewhere/x.rs\nend_of_record\n",
            "/elsewhere/x.rs is not under",
        ),
    ] {
        std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
        expect_2(run(&root, MATCHING, &[]), want);
    }
    std::fs::remove_file(root.join("lcov.info")).expect("remove lcov");
    expect_2(run(&root, MATCHING, &[]), "lcov.info: No such file");
}

/// Asserts that none of these defects is present: `--summary-file` truncating the file (a job summary holds earlier steps'
/// text, which `tee -a` kept), writing something other than the table stdout gets, or
/// writing nothing; and, on a failing ratchet, the table left out of the summary or
/// the exit status lost (the reason the old `| tee` step needed `pipefail`).
#[test]
fn summary_file_gets_the_table_appended_on_pass_and_fail() {
    let root = workspace("summary");
    let summary = root.join("summary.md");
    let flag = ["--summary-file", summary.to_str().expect("UTF-8")];
    for (baseline, code, last) in [
        (MATCHING, 0, "coverage ratchet: PASS\n"),
        (
            "a 0 0\nb 0 -\nempty - -\n",
            1,
            "coverage ratchet: FAIL (1 crate(s) need attention; see status)\n",
        ),
    ] {
        std::fs::write(&summary, "earlier step\n").expect("write summary");
        let o = run(&root, baseline, &flag);
        assert_eq!(o.status.code(), Some(code), "{}{}", stdout(&o), stderr(&o));
        let out = stdout(&o);
        assert!(out.contains(last), "{out}");
        let written = std::fs::read_to_string(&summary).expect("read summary");
        assert_eq!(written, format!("earlier step\n{out}"));
    }
    // Absent: created.
    std::fs::remove_file(&summary).expect("remove summary");
    let o = run(&root, MATCHING, &flag);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(&summary).expect("read summary"),
        stdout(&o)
    );
}

/// Asserts that none of these defects is present: a summary file that cannot be written ignored, so the job summary goes
/// missing while the step passes.
#[test]
fn an_unwritable_summary_file_exits_2() {
    let root = workspace("summary-bad");
    let dir = root.join("crates");
    let o = run(
        &root,
        MATCHING,
        &["--summary-file", dir.to_str().expect("UTF-8")],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains(&format!("append to {}:", dir.display())),
        "{}",
        stderr(&o)
    );
}

/// A header like the real file's, so the tests see comments kept.
const HEADER: &str = "# Coverage ratchet.\n# crate lines-missed branches-missed\n";

/// A workspace for `tighten` alone (no tracefile): crates named by the pending pull
/// requests' cases (js-front, js-sim-cell, the new js-storage), plus `js-zz`,
/// which sorts last.
fn tighten_workspace(name: &str) -> Workspace {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for krate in ["js-front", "js-sim-cell", "js-storage", "js-zz"] {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(&dir).expect("create crate dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("write Cargo.toml");
    }
    Workspace(root)
}

/// The recorded file before the pending pull requests, in the real file's format
/// (names padded to the longest, values right-aligned in six columns), with a crate
/// `js-gone` whose directory no longer exists.
fn recorded_file() -> String {
    format!(
        "{HEADER}\
         js-front        47      8\n\
         js-gone          3      1\n\
         js-sim-cell      4     15\n\
         js-zz            -      -\n"
    )
}

/// The measured file CI writes for the same tree: js-sim-cell's branches 15 -> 12
/// js-front's lines 47 -> 45 and the new crate
/// js-storage at `0 -`, and js-gone absent.
const MEASURED: &str = "# measured\n\
                        js-front        45      8\n\
                        js-sim-cell      4     12\n\
                        js-storage       0      -\n\
                        js-zz            -      -\n";

/// Runs `js-coverage tighten` in `root` with `args`, after writing `baseline` as its
/// `coverage-baseline` and `measured` as `measured` (both relative to `root`).
fn tighten(root: &Path, baseline: &str, measured: &str, args: &[&str]) -> Output {
    std::fs::write(root.join("coverage-baseline"), baseline).expect("write baseline");
    std::fs::write(root.join("measured"), measured).expect("write measured");
    Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .current_dir(root)
        .arg("tighten")
        .args(args)
        .output()
        .expect("run js-coverage tighten")
}

fn baseline_of(root: &Path) -> String {
    std::fs::read_to_string(root.join("coverage-baseline")).expect("read baseline")
}

/// Asserts that none of these defects is present: a value lowered to something other than the measurement, a new crate
/// added out of order or a gone one kept, the header lost, the columns realigned
/// differently from the file CI writes, or a change left out of the report. The
/// expected file is the whole file, byte for byte.
#[test]
fn tighten_lowers_to_the_measurement_and_reports_each_change() {
    let root = tighten_workspace("tighten-lowers");
    let o = tighten(
        &root,
        &recorded_file(),
        MEASURED,
        &["--measured", "measured"],
    );
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    assert_eq!(
        baseline_of(&root),
        format!(
            "{HEADER}\
             js-front        45      8\n\
             js-sim-cell      4     12\n\
             js-storage       0      -\n\
             js-zz            -      -\n"
        )
    );
    assert_eq!(
        stdout(&o),
        "js-coverage tighten: lowered ./coverage-baseline to the measurement:\n\
         \x20 js-front lines: 47 -> 45\n\
         \x20 js-gone: removed (was 3 1; crates/js-gone/Cargo.toml is gone)\n\
         \x20 js-sim-cell branches: 15 -> 12\n\
         \x20 js-storage: added as 0 -\n"
    );
}

/// Asserts that none of these defects is present: a second run that changes the file again, or says it changed something.
#[test]
fn tighten_twice_changes_nothing_the_second_time() {
    let root = tighten_workspace("tighten-twice");
    let o = tighten(
        &root,
        &recorded_file(),
        MEASURED,
        &["--measured", "measured"],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let once = baseline_of(&root);
    let o = tighten(&root, &once, MEASURED, &["--measured", "measured"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(baseline_of(&root), once);
    assert_eq!(
        stdout(&o),
        "js-coverage tighten: ./coverage-baseline already matches the measurement; nothing to change\n"
    );
}

/// The advice after a refusal that says the measured file is of another tree.
const OTHER_TREE: &str = "\nCI measures the pull request merged with main: bring the branch up \
     to date with main, re-run CI, and run the command its coverage job prints\n";

/// The advice after any other refusal of a measured file.
const MAYBE: &str = "\nA measured file from CI is of the pull request merged with main: if the \
     branch is behind main, main's changes can cause these refusals; bring the branch up to \
     date with main, re-run CI, and run the command its coverage job prints. A refusal that \
     stays is this branch's to fix\n";

/// Asserts that none of these defects is present: any rise written (or the run's lowerings written alongside one), a
/// refusal that exits 0, or one that does not name the crate, the metric and both
/// values. Each case starts from the recorded file and must leave it byte-identical.
#[test]
fn tighten_refuses_every_rise_and_leaves_the_file_untouched() {
    let root = tighten_workspace("tighten-refuses");
    let measured = |front: &str, sim: &str, storage: &str, zz: &str| {
        let mut m = format!("js-front {front}\njs-sim-cell {sim}\njs-zz {zz}\n");
        if !storage.is_empty() {
            m.push_str(&format!("js-storage {storage}\n"));
        }
        m
    };
    for (m, want) in [
        // A higher count, with js-sim-cell's lowering in the same run. Every
        // refusal of a measured file ends with advice: CI's file is of the merge
        // with main, so a branch behind main can see main's changes refused.
        (
            measured("48 8", "4 12", "0 -", "- -"),
            vec![
                "js-front lines: recorded 47, measured 48: more missed than recorded",
                MAYBE,
            ],
        ),
        // `-` to a count above 0.
        (
            measured("47 8", "4 15", "0 -", "1 -"),
            vec![
                "js-zz lines: recorded -, measured 1: more missed than recorded",
                MAYBE,
            ],
        ),
        // A branch count to no data, and a second rise: both named.
        (
            measured("47 -", "5 15", "0 -", "- -"),
            vec![
                "js-front branches: recorded 8, measured - while its lines are measured: the missed \
                 branches were removed rather than covered",
                "js-sim-cell lines: recorded 4, measured 5: more missed than recorded",
                MAYBE,
            ],
        ),
        // A new crate with uncovered code.
        (
            measured("47 8", "4 15", "1 -", "- -"),
            vec![
                "js-storage: not in the baseline, measured 1 -: a new crate must arrive covered",
                MAYBE,
            ],
        ),
        // A rise next to a workspace crate the measured file lacks: the measurement
        // is of another tree, so the firm advice, once.
        (
            measured("48 8", "4 15", "", "- -"),
            vec![
                "js-front lines: recorded 47, measured 48: more missed than recorded",
                "js-storage: crates/js-storage/Cargo.toml exists but the measured file has no line for it",
                OTHER_TREE,
            ],
        ),
        // A workspace crate the measured file lacks, then the advice for a
        // measurement of another tree (a branch behind main).
        (
            measured("47 8", "4 15", "", "- -"),
            vec![
                "js-storage: crates/js-storage/Cargo.toml exists but the measured file has no line for it",
                OTHER_TREE,
            ],
        ),
        // A measured crate with no Cargo.toml here, and the same advice.
        (
            measured("47 8", "4 15", "0 -", "- -") + "js-other 0 0\n",
            vec![
                "js-other: measured, but there is no crates/js-other/Cargo.toml",
                OTHER_TREE,
            ],
        ),
    ] {
        let before = recorded_file();
        let o = tighten(&root, &before, &m, &["--measured", "measured"]);
        assert_eq!(
            o.status.code(),
            Some(1),
            "{m}: {}{}",
            stdout(&o),
            stderr(&o)
        );
        assert_eq!(baseline_of(&root), before, "{m}");
        assert_eq!(stdout(&o), "", "{m}");
        let err = stderr(&o);
        assert!(err.starts_with(REFUSED), "{err}");
        // One line per refusal and one for the advice after the first, no more; the
        // advice last, the firm form only after a refusal of another tree's
        // measurement.
        assert_eq!(err.lines().count(), 1 + want.len(), "{m}: {err}");
        assert!(err.ends_with(want.last().expect("advice")), "{m}: {err}");
        assert_eq!(
            err.contains(OTHER_TREE),
            want.contains(&OTHER_TREE),
            "{m}: {err}"
        );
        assert_eq!(err.contains(MAYBE), want.contains(&MAYBE), "{m}: {err}");
        for w in want {
            assert!(err.contains(w), "{m}: want {w:?} in {err}");
        }
    }
}

/// Asserts that none of these defects is present: `--lcov` measured differently from `check` (the same tracefile must give
/// the file `check --write-baseline` writes), `-` with a measured 0 left as `-`, or
/// the file realigned in a way `check` cannot read back as passing.
#[test]
fn tighten_from_a_tracefile_then_check_passes() {
    let root = workspace("tighten-lcov");
    // `a` misses 1 line (2 recorded); `b` measures 0 lines missed where `-` is recorded.
    std::fs::write(
        root.join("coverage-baseline"),
        "# kept\na 2 0\nb - -\nempty - -\n",
    )
    .expect("write baseline");
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("tighten")
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(&*root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run js-coverage tighten");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        baseline_of(&root),
        "# kept\na          1      0\nb          0      -\nempty      -      -\n"
    );
    assert!(
        stdout(&o).contains("\n  a lines: 2 -> 1\n  b lines: - -> 0\n"),
        "{}",
        stdout(&o)
    );
    let o = run(&root, &baseline_of(&root), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
}

/// Asserts that none of these defects is present: a file that cannot be written reported as lowered (or left half
/// written), and unreadable inputs exiting 1 (a refusal) instead of 2.
#[test]
fn tighten_bad_inputs_exit_2() {
    let root = tighten_workspace("tighten-bad");
    let o = tighten(&root, &recorded_file(), MEASURED, &["--measured", "absent"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("read absent:"), "{}", stderr(&o));
    let o = tighten(
        &root,
        &recorded_file(),
        "x 1.5 -\n",
        &["--measured", "measured"],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("measured: line 1: not a count"),
        "{}",
        stderr(&o)
    );
    let o = tighten(
        &root,
        &recorded_file(),
        MEASURED,
        &["--measured", "measured", "--root", "crates"],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("list crates under crates:"),
        "{}",
        stderr(&o)
    );
    // The new file cannot be written beside the baseline: a directory holds its
    // name (a permission bit would not stop root).
    let temp = root.join("coverage-baseline.tighten-tmp");
    std::fs::create_dir(&temp).expect("mkdir");
    let o = tighten(
        &root,
        &recorded_file(),
        MEASURED,
        &["--measured", "measured"],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("write ./coverage-baseline:"),
        "{}",
        stderr(&o)
    );
    assert_eq!(baseline_of(&root), recorded_file());
    std::fs::remove_dir(&temp).expect("rmdir");
    // Written whole: the run succeeds, and leaves no file beside the baseline.
    let o = tighten(
        &root,
        &recorded_file(),
        MEASURED,
        &["--measured", "measured"],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!temp.exists());
}

/// Asserts that none of these defects is present: `--baseline` resolved against the working directory instead of `--root`,
/// and a `--root` that is not this workspace taken as one where every crate is gone,
/// which would delete every line. The file must come out untouched.
#[test]
fn tighten_baseline_follows_the_root_and_a_foreign_root_is_refused() {
    let root = tighten_workspace("tighten-root");
    std::fs::write(root.join("coverage-baseline"), recorded_file()).expect("write");
    std::fs::write(root.join("measured"), MEASURED).expect("write");
    let elsewhere = tighten_workspace("tighten-root-cwd");
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .current_dir(&*elsewhere)
        .arg("tighten")
        .arg("--measured")
        .arg(root.join("measured"))
        .arg("--root")
        .arg(&*root)
        .output()
        .expect("run js-coverage tighten");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(baseline_of(&root).contains("js-sim-cell      4     12\n"));
    assert!(!elsewhere.join("coverage-baseline").exists());

    // A root with an empty crates/ and an empty measured file (nothing is in the
    // workspace, so every line would go), and one whose only crate the file lacks.
    let foreign = |name: &str, crates: &[&str], measured: &str| {
        let dir =
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates")).expect("mkdir");
        for krate in crates {
            std::fs::create_dir_all(dir.join("crates").join(krate)).expect("mkdir");
            std::fs::write(dir.join("crates").join(krate).join("Cargo.toml"), "").expect("write");
        }
        std::fs::write(dir.join("measured"), measured).expect("write");
        let dir = Workspace(dir);
        let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
            .arg("tighten")
            .arg("--measured")
            .arg(dir.join("measured"))
            .arg("--root")
            .arg(&*dir)
            .arg("--baseline")
            .arg(root.join("coverage-baseline"))
            .output()
            .expect("run js-coverage tighten");
        drop(dir);
        o
    };
    let before = recorded_file();
    std::fs::write(root.join("coverage-baseline"), &before).expect("write");
    let o = foreign("tighten-root-empty", &[], "");
    let o2 = foreign("tighten-root-other", &["x"], "x 0 0\n");
    for o in [o, o2] {
        assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains(
                "none of the baseline's 4 crate(s) has a crates/<name>/Cargo.toml under the root; \
                 pass the workspace root as --root"
            ),
            "{}",
            stderr(&o)
        );
        assert_eq!(baseline_of(&root), before);
    }
}

/// Asserts that none of these defects is present: the default (no subcommand) form CI runs today broken by the
/// subcommands, `check` named explicitly behaving differently, and malformed calls
/// (nothing given, both or neither of `tighten`'s inputs, `--run-id` with no file to
/// download) accepted instead of exiting 2.
#[test]
fn subcommands_and_usage_errors() {
    let root = workspace("usage");
    std::fs::write(root.join("coverage-baseline"), MATCHING).expect("write baseline");
    let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .arg("check")
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(&*root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run js-coverage check");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), stdout(&run(&root, MATCHING, &[])));
    for args in [
        &[][..],
        &["tighten"][..],
        &["tighten", "--measured", "m", "--lcov", "l"][..],
        &[
            "--lcov",
            "l",
            "--root",
            "r",
            "--baseline",
            "b",
            "--run-id",
            "1",
        ][..],
        &[
            "--lcov",
            "l",
            "--root",
            "r",
            "--baseline",
            "b",
            "--write-baseline",
            "w",
            "--repo",
            "o/r",
        ][..],
        &["--lcov", "l", "tighten"][..],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_js-coverage"))
            .args(args)
            .output()
            .expect("run js-coverage");
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
        assert!(!stderr(&o).is_empty(), "{args:?}");
    }
}

/// Runs the check as CI does (in the workspace, relative baseline path) with `extra`.
fn ci_check(root: &Path, baseline: &str, extra: &[&str]) -> Output {
    std::fs::write(root.join("coverage-baseline"), baseline).expect("write baseline");
    Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .current_dir(root)
        .args([
            "--lcov",
            "lcov.info",
            "--baseline",
            "coverage-baseline",
            "--root",
        ])
        .arg(root)
        .args(extra)
        .output()
        .expect("run js-coverage")
}

/// Runs a printed `d=$(mktemp -d) && gh run download ... --dir "$d" && cargo run -p
/// js-coverage -- <args>` in `root`, without a shell: the mktemp part makes a fresh
/// directory for `$d`, the gh part creates `measured`'s file in the `--dir` and fails
/// if it is there already (gh will not overwrite a file), and the cargo part runs the
/// binary. Panics on any other shape, so a changed command cannot pass unrun.
fn run_printed(root: &Path, command: &str, measured: &str) -> Output {
    static FRESH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let rest = command
        .strip_prefix("d=$(mktemp -d) && ")
        .expect("a fresh directory per run");
    let n = FRESH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = root.join(format!("mktemp-{n}"));
    std::fs::create_dir(&d).expect("fresh dir");
    let d = d.display().to_string();
    let (gh, cargo) = rest.split_once(" && ").expect("gh && cargo");
    let gh: Vec<String> = gh.split(' ').map(|w| w.replace("\"$d\"", &d)).collect();
    assert_eq!(gh[..3], ["gh", "run", "download"], "{command}");
    // gh makes the directory when it is missing, relative to where it runs.
    let dir = root.join(&gh[gh.iter().position(|w| w == "--dir").expect("--dir") + 1]);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join("coverage-baseline.measured"))
        .expect("gh run download does not overwrite a file");
    std::io::Write::write_all(&mut file, measured.as_bytes()).expect("write");
    let words: Vec<String> = cargo.split(' ').map(|w| w.replace("\"$d\"", &d)).collect();
    assert_eq!(
        words[..5],
        ["cargo", "run", "-p", "js-coverage", "--"],
        "{command}"
    );
    Command::new(env!("CARGO_BIN_EXE_js-coverage"))
        .current_dir(root)
        .args(&words[5..])
        .output()
        .expect("run tighten")
}

/// Asserts that none of these defects is present: a failure `tighten` fixes printed without the command (or with one that
/// names another run, artifact or file, that does not run, or that fails when run
/// again), a raise told to run it, and the advice left out of the job summary. The
/// command is checked exactly, then run twice on the file the check wrote; the second
/// run changes nothing, and the check passes.
#[test]
fn a_failure_tighten_fixes_prints_the_command_with_the_run_id() {
    let root = workspace("ci-message");
    std::fs::create_dir_all(root.join("target/coverage")).expect("mkdir");
    let summary = root.join("summary.md");
    let flags = [
        "--write-baseline",
        "target/coverage/coverage-baseline.measured",
        "--run-id",
        "38801912372",
        "--repo",
        "komira-ai/jobscheduler",
        "--summary-file",
        summary.to_str().expect("UTF-8"),
    ];
    // `a` misses 1 line where 2 are recorded; `b` measures 0 where `-` is recorded.
    let o = ci_check(&root, "a 2 0\nb - -\nempty - -\n", &flags);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let out = stdout(&o);
    let command = "d=$(mktemp -d) && gh run download 38801912372 --repo komira-ai/jobscheduler \
                   --name coverage --dir \"$d\" && cargo run -p js-coverage -- tighten \
                   --measured \"$d\"/coverage-baseline.measured";
    assert!(
        out.ends_with(&format!(
            "coverage ratchet: FAIL (2 crate(s) need attention; see status)\n\n\
             Every failure above is one `js-coverage tighten` fixes: coverage improved (fewer items \
             missed than recorded), a crate was added with its lines measured and none missed (`0 0` \
             or `0 -`), or a crate was deleted. Running it is \
             required, and it is not a bypass: it only lowers counts. Run this, then commit \
             coverage-baseline in this pull request:\n\n```sh\n{command}\n```\n"
        )),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&summary).expect("summary"), out);

    // The whole command, twice, as a shell would run it: `run_printed` stands in for
    // mktemp and for gh, which will not overwrite a file. The second run downloads
    // again and changes nothing.
    let measured = std::fs::read_to_string(root.join("target/coverage/coverage-baseline.measured"))
        .expect("measured");
    let first = run_printed(&root, command, &measured);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let once = baseline_of(&root);
    let second = run_printed(&root, command, &measured);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(baseline_of(&root), once);
    assert!(
        stdout(&second).ends_with("already matches the measurement; nothing to change\n"),
        "{}",
        stdout(&second)
    );
    let o = ci_check(&root, &baseline_of(&root), &flags);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));

    // A raise next to a lowering: no command, and the reason named.
    let o = ci_check(&root, "a 0 0\nb - -\nempty - -\n", &flags);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(!out.contains("gh run download"), "{out}");
    assert!(
        out.ends_with(
            "`js-coverage tighten` cannot fix this failure: it only lowers counts, and it refuses:\n\n\
             - a lines: recorded 0, measured 1: more missed than recorded; cover the code\n"
        ),
        "{out}"
    );
}

/// Asserts that none of these defects is present: the command naming another file than `--write-baseline`'s, another
/// artifact, a repository flag nobody gave, or a path or value the shell would split
/// or drop (a space, an empty value) left unquoted.
#[test]
fn the_command_follows_the_flags_and_quotes_for_the_shell() {
    let root = workspace("ci-flags");
    let loose = "a 2 0\nb 0 -\nempty - -\n";
    let command = |extra: &[&str]| {
        let o = ci_check(&root, loose, extra);
        assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
        let out = stdout(&o);
        let start = out.find("```sh\n").expect("a command") + 6;
        out[start..].lines().next().expect("a line").to_owned()
    };
    assert_eq!(
        command(&["--write-baseline", "out m"]),
        "cargo run -p js-coverage -- tighten --measured 'out m'"
    );
    assert_eq!(
        command(&[
            "--write-baseline",
            "out m",
            "--run-id",
            "7",
            "--artifact",
            "cov x"
        ]),
        "d=$(mktemp -d) && gh run download 7 --name 'cov x' --dir \"$d\" && \
         cargo run -p js-coverage -- tighten --measured \"$d\"/'out m'"
    );
    assert_eq!(
        command(&["--write-baseline", "m", "--run-id", "7", "--repo", ""]),
        "d=$(mktemp -d) && gh run download 7 --repo '' --name coverage --dir \"$d\" && \
         cargo run -p js-coverage -- tighten --measured \"$d\"/m"
    );
}

/// The first line `tighten` prints on a refusal, for a baseline at `coverage-baseline`
/// under the working directory.
const REFUSED: &str = "js-coverage tighten: refused; ./coverage-baseline is unchanged. \
     tighten only lowers counts:\n";

/// Asserts that none of these defects is present: a failed rename leaving `coverage-baseline.tighten-tmp` behind (the next
/// run, or a commit, would pick it up) or reported as success, and the baseline
/// changed by a replace that failed.
#[test]
fn a_failed_rename_leaves_no_temporary_file() {
    let root = tighten_workspace("tighten-rename");
    let path = root.join("coverage-baseline");
    std::fs::write(&path, "old\n").expect("write");
    let got = js_coverage::tighten::replace_with(&path, "new\n", |from, to| {
        assert_eq!(from, root.join("coverage-baseline.tighten-tmp"));
        assert_eq!(
            std::fs::read_to_string(from).expect("written first"),
            "new\n"
        );
        assert_eq!(to, path);
        Err(std::io::Error::other("planted"))
    });
    assert_eq!(got.map_err(|e| e.to_string()), Err("planted".to_owned()));
    assert!(!root.join("coverage-baseline.tighten-tmp").exists());
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "old\n");
    js_coverage::tighten::replace(&path, "new\n").expect("replace");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
    assert!(!root.join("coverage-baseline.tighten-tmp").exists());
}
