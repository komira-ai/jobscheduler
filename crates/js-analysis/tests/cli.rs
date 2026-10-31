//! The command line: parsing, the run function and the program itself.

mod common;

use std::path::PathBuf;
use std::process::Command;

use clap::Parser as _;
use common::{arm_runs, rec};
use js_analysis::{Cli, Error, run};

/// A path under the test scratch directory.
fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Writes a run file of three arms at one setting, with a warm-up and a missing value, and
/// returns its path.
fn write_runs(name: &str) -> PathBuf {
    let mut records = vec![rec("0.5", "base", 7, "warmup", &[("m", Some(99.0))])];
    records.extend(arm_runs("0.5", "base", &[9.0, 10.0, 11.0]));
    records.extend(arm_runs("0.5", "fast", &[12.0, 13.0, 14.0]));
    records.push(rec("0.5", "fast", 5, "", &[("m", None)]));
    let lines: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    let path = scratch(name);
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("js-analysis").chain(args.iter().copied()))
}

/// Asserts the defaults and that a given `--effect` replaces them.
#[test]
fn defaults_and_effects() {
    let cli = parse(&["runs.jsonl"]).unwrap();
    assert_eq!(cli.effects, [0.05, 0.1]);
    assert!(cli.baseline.is_none() && !cli.ladder && cli.json.is_none());
    let cli = parse(&["runs.jsonl", "--effect", "0.2", "--effect", "0.3"]).unwrap();
    assert_eq!(cli.effects, [0.2, 0.3]);
}

/// Asserts `--compare-cv` takes exactly two arms, and `--ladder` and `--baseline` exclude
/// each other.
#[test]
fn compare_cv_takes_a_pair_and_ladder_excludes_baseline() {
    let cli = parse(&["r.jsonl", "--compare-cv", "shared", "whole host"]).unwrap();
    assert_eq!(cli.compare_cv, ["shared", "whole host"]);
    assert!(parse(&["r.jsonl", "--compare-cv", "shared"]).is_err());
    assert!(parse(&["r.jsonl", "--ladder", "--baseline", "a"]).is_err());
}

/// Asserts a run prints the markdown and writes the JSON file, both with every run.
#[test]
fn a_run_prints_markdown_and_writes_json() {
    let input = write_runs("cli_run.jsonl");
    let json = scratch("cli_run.json");
    let cli = parse(&[
        input.to_str().unwrap(),
        "--baseline",
        "base",
        "--compare-cv",
        "base",
        "fast",
        "--json",
        json.to_str().unwrap(),
    ])
    .unwrap();
    let mut out = Vec::new();
    run(&cli, &mut out).unwrap();
    let md = String::from_utf8(out).unwrap();
    assert!(md.contains("| 0.5 | base | 3 | 0 | 1 | 10.0000 |"), "{md}");
    assert!(md.contains("| 0.5 | fast | 3 | 1 | 0 | 13.0000 |"), "{md}");
    assert!(md.contains("### Comparisons of std/mean"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
    assert_eq!(v["groups"][0]["runs"].as_array().unwrap().len(), 4);
}

/// Asserts a missing input file is reported with its path.
#[test]
fn a_missing_input_is_reported() {
    let path = scratch("cli_no_such.jsonl");
    let cli = parse(&[path.to_str().unwrap()]).unwrap();
    let error = run(&cli, &mut Vec::new()).unwrap_err();
    assert!(matches!(error, Error::Read { .. }));
    assert!(error.to_string().contains("cli_no_such.jsonl"));
}

/// Asserts a bad line, an unknown base arm, an output that cannot be created, a JSON file
/// that cannot be written and standard output that cannot be written are each an error.
#[test]
fn failures_are_errors() {
    let bad = scratch("cli_bad.jsonl");
    std::fs::write(&bad, "oops\n").unwrap();
    let cli = parse(&[bad.to_str().unwrap()]).unwrap();
    assert!(matches!(
        run(&cli, &mut Vec::new()),
        Err(Error::Line { .. })
    ));

    let input = write_runs("cli_fail.jsonl");
    let cli = parse(&[input.to_str().unwrap(), "--baseline", "zzz"]).unwrap();
    let error = run(&cli, &mut Vec::new()).unwrap_err();
    assert!(matches!(error, Error::UnknownArm(_)));
    assert_eq!(error.to_string(), "no run of arm \"zzz\"");

    let directory = scratch("");
    let cli = parse(&[
        input.to_str().unwrap(),
        "--json",
        directory.to_str().unwrap(),
    ])
    .unwrap();
    let error = run(&cli, &mut Vec::new()).unwrap_err();
    assert!(matches!(error, Error::Io(_)));
    assert!(error.to_string().starts_with("cannot write the output: "));

    // /dev/full accepts the open and refuses every write.
    let cli = parse(&[input.to_str().unwrap(), "--json", "/dev/full"]).unwrap();
    assert!(matches!(run(&cli, &mut Vec::new()), Err(Error::Json(_))));

    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let cli = parse(&[input.to_str().unwrap()]).unwrap();
    assert!(matches!(run(&cli, &mut Closed), Err(Error::Io(_))));
}

/// Asserts the program answers `--version`, which catches a `main` that is not wired up.
#[test]
fn the_program_answers_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-analysis"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("js-analysis ")
    );
}

/// Asserts the program prints the report of a file and exits 0.
#[test]
fn the_program_prints_a_report() {
    let input = write_runs("cli_program.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_js-analysis"))
        .arg(&input)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("### m\n")
    );
}

/// Asserts the program exits nonzero and names itself on standard error for a missing file.
#[test]
fn the_program_fails_on_a_missing_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_js-analysis"))
        .arg(scratch("cli_program_missing.jsonl"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("js-analysis: cannot read")
    );
}
