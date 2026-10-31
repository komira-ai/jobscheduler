//! The markdown and the JSON of a report.

mod common;

use std::io::{self, Write};

use common::{arm_runs, rec};
use js_analysis::{Error, Options, Report, RunRecord, analyze, to_markdown, write_json};

fn dataset() -> Vec<RunRecord> {
    let mut r = vec![rec("0.5", "base", 9, "warmup", &[("m", Some(50.0))])];
    r.extend(arm_runs("0.5", "base", &[9.0, 10.0, 11.0]));
    r.extend(arm_runs("0.5", "fast", &[12.0, 13.0, 14.0]));
    r.extend(arm_runs("0.5", "same", &[9.0, 10.5, 11.0]));
    r.extend(arm_runs("0.5", "one", &[5.0]));
    for i in 0..3 {
        r.push(rec("0.5", "gap", i, "", &[("q", None)]));
    }
    let big: Vec<f64> = (0..12).map(|i| 100.0 + f64::from(i)).collect();
    let shifted: Vec<f64> = big.iter().map(|v| v + 20.0).collect();
    r.extend(arm_runs("1.0", "base", &big));
    r.extend(arm_runs("1.0", "x", &shifted));
    r
}

fn options() -> Options {
    Options {
        cv_pairs: vec![("base".to_string(), "fast".to_string())],
        effects: vec![0.05, 0.001],
        ..Options::default()
    }
}

fn report() -> Report {
    analyze(dataset(), &options()).unwrap()
}

/// Asserts the arm tables: a header per metric, a row per group with its counts, NA where a
/// metric has no value, and the warm-up counted but not averaged.
#[test]
fn arm_tables_show_counts_and_na() {
    let md = to_markdown(&report());
    assert!(md.contains("### m\n"));
    assert!(md.contains("### q\n"));
    assert!(md.contains("| setting | arm | n | missing | warm-ups | mean | 95% CI of mean |"));
    assert!(md.contains("| 0.5 | base | 3 | 0 | 1 | 10.0000 | ["));
    assert!(md.contains("| 0.5 | gap | 0 | 3 | 0 | NA | NA | NA | NA | NA | NA | NA | NA |\n"));
}

/// Asserts the comparison table carries every verdict, both permutation methods, the
/// underpowered flag and a signed percentage.
#[test]
fn comparison_table_shows_verdicts_and_methods() {
    let md = to_markdown(&report());
    assert!(md.contains("### Comparisons (Welch 95% interval of the difference of means)"));
    for word in ["differs", "not-distinguished", "not-estimable"] {
        assert!(md.contains(&format!(" | {word} | ")), "{word}");
    }
    assert!(md.contains("(exact, "));
    assert!(md.contains("(monte-carlo, 100000 splits)"));
    assert!(md.contains("| 0.5 | m | base | fast | 3 | 3 | 3.0000 | ["));
    assert!(md.contains("+30.00%"));
    assert!(md.contains(" | yes |"));
}

/// Asserts the std/mean comparison and the run-count tables, with a count above the search
/// limit written as such.
#[test]
fn spread_and_run_count_tables() {
    let md = to_markdown(&report());
    assert!(md.contains("### Comparisons of std/mean"));
    assert!(md.contains("| 0.5 | m | base | fast | 3 | 3 | 0.1000 | 0.0769 |"));
    assert!(md.contains("### Runs per arm needed for an effect"));
    assert!(md.contains("| +5.00% | "));
    assert!(md.contains("| +0.10% | >200 | >200 |"));
}

/// Asserts every run is listed, the warm-up with its label and a missing value as NA.
#[test]
fn every_run_is_listed() {
    let md = to_markdown(&report());
    assert!(md.contains("| 0.5 | base | 9 | warmup | 109 | c0ffee1 | m=50.0000 |\n"));
    assert!(md.contains("| 0.5 | gap | 2 |  | 102 | c0ffee1 | q=NA |\n"));
    let rows = md.lines().filter(|l| l.contains("| c0ffee1 |")).count();
    assert_eq!(rows, dataset().len());
}

/// Asserts a report without comparisons or run counts leaves those sections out.
#[test]
fn sections_without_content_are_left_out() {
    let r = analyze(arm_runs("1", "a", &[1.0, 2.0, 3.0]), &Options::default()).unwrap();
    let md = to_markdown(&r);
    assert!(!md.contains("Comparisons") && !md.contains("needed for an effect"));
    assert!(md.contains("### Runs"));
}

/// Asserts the JSON holds every run, keeps not-available as null, and gives the verdict
/// names the table uses.
#[test]
fn json_keeps_every_run_and_null() {
    let mut out = Vec::new();
    write_json(&report(), &mut out).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let groups = v["groups"].as_array().unwrap();
    let listed: usize = groups
        .iter()
        .map(|g| g["runs"].as_array().unwrap().len())
        .sum();
    assert_eq!(listed, dataset().len());
    assert_eq!(groups[0]["runs"][0]["label"], "warmup");
    let gap = groups.iter().find(|g| g["arm"] == "gap").unwrap();
    assert!(gap["metrics"][0]["summary"].is_null());
    assert_eq!(gap["metrics"][0]["n_missing"], 3);
    assert_eq!(gap["runs"][0]["metrics"]["q"], serde_json::Value::Null);
    let first = &v["comparisons"][0];
    assert_eq!(first["verdict"], "differs");
    assert_eq!(first["permutation"]["method"], "exact");
    assert!(first["diff_ci"]["lo"].as_f64().unwrap() > 0.0);
    let last = v["comparisons"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["permutation"]["method"], "monte-carlo");
}

struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts a failing writer is reported, not swallowed.
#[test]
fn a_failing_writer_is_an_error() {
    let error = write_json(&report(), &mut Broken).unwrap_err();
    assert!(matches!(error, Error::Json(_)));
    assert!(
        error
            .to_string()
            .starts_with("cannot write the JSON report")
    );
}
