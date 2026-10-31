//! The command line: the three commands on files, the dry run on a simulated farm, the
//! live flags, and the errors.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use clap::Parser;
use js_measure::{Cli, CliError, ExperimentError, SetError, execute, read_records};
use js_testkit::dir::TestDir;

const PLAN: &str = r#"{"parameter":"fill","settings":["0.8","1.0"],"arms":[{"label":"whole"},{"label":"booked","control":true}],"runs_per_arm":1,"warmups_per_arm":1,"seed":11}"#;

const WORKLOAD: &str = r#"{"jobs":[{"name":"build","count":2,"image":"img","cpu_millis":4000,"memory_bytes":8589934592,"wall_ms":60000}]}"#;

fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn words(args: &[&str]) -> Vec<String> {
    std::iter::once("js-measure")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn go(args: &[&str]) -> (Result<(), CliError>, String) {
    let cli = Cli::try_parse_from(words(args)).unwrap();
    let mut out = Vec::new();
    let result = execute(cli.command, &mut out);
    (result, String::from_utf8(out).unwrap())
}

struct Files {
    dir: TestDir,
}

impl Files {
    fn new(name: &str, plan: &str, workload: &str) -> Self {
        let dir = dir(name);
        fs::write(dir.path().join("plan.json"), plan).unwrap();
        fs::write(dir.path().join("workload.json"), workload).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    fn run(&self, extra: &[&str]) -> (Result<(), CliError>, String) {
        let (plan, workload, out) = (
            self.path("plan.json"),
            self.path("workload.json"),
            self.path("out"),
        );
        let mut args: Vec<&str> = vec![
            "run",
            "--plan",
            plan.as_str(),
            "--workload",
            workload.as_str(),
            "--out",
            out.as_str(),
            "--commit",
            "abc123",
        ];
        args.extend_from_slice(extra);
        go(&args)
    }

    fn lines(&self, name: &str) -> Vec<String> {
        let text = fs::read_to_string(self.dir.path().join("out").join(name)).unwrap();
        text.lines().map(str::to_owned).collect()
    }
}

/// Asserts that `schedule` prints one line per run in the executed order, warm-ups first,
/// the arm by its label, and that the order is the plan's own.
#[test]
fn schedule_prints_the_order() {
    let files = Files::new("cli-schedule", PLAN, WORKLOAD);
    let (result, out) = go(&["schedule", "--plan", &files.path("plan.json")]);
    result.unwrap();
    let plan: js_measure::Plan = serde_json::from_str(PLAN).unwrap();
    let expected: String = plan
        .schedule()
        .unwrap()
        .iter()
        .map(|slot| {
            let kind = if slot.warmup { "warmup" } else { "run" };
            format!(
                "{} {kind} {} {}\n",
                slot.order, plan.settings[slot.setting], plan.arms[slot.arm].label
            )
        })
        .collect();
    assert_eq!(out, expected);
    assert_eq!(out.lines().count(), 8);
}

/// Asserts the dry run end to end: three files in the output directory, one record per
/// run with the commit and the seed, a row per job and a row per host-minute, a line saying
/// how many runs were recorded, and that a second run appends and does not overwrite.
#[test]
fn run_writes_the_three_files_and_appends() {
    let files = Files::new("cli-run", PLAN, WORKLOAD);
    let (result, out) = files.run(&["--fake-hosts", "1", "--poll-ms", "10000"]);
    result.unwrap();
    assert!(out.starts_with("8 runs recorded in "), "{out}");
    let records = read_records(&files.lines("records.jsonl").join("\n")).unwrap();
    assert_eq!(records.len(), 8);
    assert!(
        records
            .iter()
            .all(|r| r.commit == "abc123" && r.seed == 11 && r.setting.parse::<f64>().is_ok())
    );
    assert_eq!(records.iter().filter(|r| r.label == "warmup").count(), 4);
    let order: Vec<u32> = records.iter().map(|r| r.run_index).collect();
    assert_eq!(order, (0..8).collect::<Vec<u32>>());
    assert!(records.iter().all(|r| r.metrics["jobs"] == Some(2.0)));
    assert_eq!(files.lines("jobs.jsonl").len(), 16);
    assert!(!files.lines("minutes.jsonl").is_empty());
    files.run(&["--fake-hosts", "1"]).0.unwrap();
    assert_eq!(files.lines("records.jsonl").len(), 16);
}

/// Asserts that `summary` groups a records file by setting and arm and lists the runs, and that the
/// file of a dry run can be summarised.
#[test]
fn summary_lists_the_groups() {
    let files = Files::new("cli-summary", PLAN, WORKLOAD);
    files.run(&["--fake-hosts", "2"]).0.unwrap();
    let (result, out) = go(&["summary", "--records", &files.path("out/records.jsonl")]);
    result.unwrap();
    assert_eq!(out.lines().count(), 4);
    assert!(
        out.starts_with("0.8 booked: 2 runs (1 warm-up); run_index "),
        "{out}"
    );
    assert!(
        out.contains("\n1.0 whole: 2 runs (1 warm-up); run_index "),
        "{out}"
    );
}

/// Asserts the setter flag: a program that succeeds lets the plan run, one that fails stops
/// it before the first run is recorded.
#[test]
fn apply_command_runs_before_each_run() {
    let files = Files::new("cli-apply", PLAN, WORKLOAD);
    files.run(&["--apply-command", "true"]).0.unwrap();
    assert_eq!(files.lines("records.jsonl").len(), 8);
    let again = Files::new("cli-apply-fails", PLAN, WORKLOAD);
    let (result, _) = again.run(&["--apply-command", "false"]);
    assert!(matches!(
        result,
        Err(CliError::Experiment(ExperimentError::Set(
            SetError::Failed(..)
        )))
    ));
    assert!(again.lines("records.jsonl").is_empty());
}

/// Asserts the live mode against a server that is not there: the flags are accepted and
/// the client built, every lease request fails, and the run still records each run with
/// its failures counted and its makespan not available (nothing is invented).
#[test]
fn live_mode_records_failures_when_the_server_is_down() {
    let files = Files::new("cli-live", PLAN, WORKLOAD);
    let token = files.dir.path().join("token");
    fs::write(&token, "secret\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let token = token.display().to_string();
    let (result, _) = files.run(&["--live", "--server", &closed, "--token-file", &token]);
    result.unwrap();
    let records = read_records(&files.lines("records.jsonl").join("\n")).unwrap();
    assert_eq!(records.len(), 8);
    assert!(
        records
            .iter()
            .all(|r| r.metrics["jobs_failed"] == Some(2.0))
    );
    assert!(records.iter().all(|r| r.metrics["makespan_ms"].is_none()));
    let (result, _) = files.run(&[
        "--live",
        "--server",
        "http://example.com",
        "--token-file",
        &token,
    ]);
    assert!(matches!(result, Err(CliError::Client(_))));
}

/// Asserts that the live flags go together: `--live` needs the server and the token, and
/// the server flags mean nothing without `--live`.
#[test]
fn the_live_flags_go_together() {
    let base = [
        "run",
        "--plan",
        "p",
        "--workload",
        "w",
        "--out",
        "o",
        "--commit",
        "c",
    ];
    let with = |extra: &[&str]| {
        let mut args = base.to_vec();
        args.extend_from_slice(extra);
        Cli::try_parse_from(words(&args))
    };
    assert!(with(&["--live"]).is_err());
    assert!(with(&["--live", "--server", "http://127.0.0.1:1"]).is_err());
    assert!(with(&["--server", "http://127.0.0.1:1"]).is_err());
    assert!(
        with(&[
            "--live",
            "--server",
            "http://127.0.0.1:1",
            "--token-file",
            "t"
        ])
        .is_ok()
    );
    assert!(with(&[]).is_ok());
}

fn error_of(result: Result<(), CliError>) -> String {
    result.unwrap_err().to_string()
}

/// Asserts the errors of the files: a missing file and a file that is not the format name
/// the path, a plan with no arm, a workload with no job, a records file with a bad line
/// names the line, and an output directory that cannot be made or written is named.
#[test]
fn bad_files_are_named() {
    let files = Files::new("cli-bad", PLAN, WORKLOAD);
    let missing = error_of(go(&["schedule", "--plan", &files.path("nope.json")]).0);
    assert!(missing.starts_with(&files.path("nope.json")), "{missing}");
    fs::write(files.dir.path().join("junk.json"), "{").unwrap();
    let junk = error_of(go(&["schedule", "--plan", &files.path("junk.json")]).0);
    assert!(junk.starts_with(&files.path("junk.json")), "{junk}");

    let empty = Files::new(
        "cli-bad-plan",
        r#"{"parameter":"p","settings":["1"],"arms":[],"runs_per_arm":1,"seed":1}"#,
        WORKLOAD,
    );
    assert_eq!(error_of(empty.run(&[]).0), "the plan has no arms");
    assert_eq!(
        error_of(go(&["schedule", "--plan", &empty.path("plan.json")]).0),
        "the plan has no arms"
    );
    let nothing = Files::new("cli-bad-workload", PLAN, r#"{"jobs":[]}"#);
    assert_eq!(error_of(nothing.run(&[]).0), "the workload has no jobs");

    fs::write(files.dir.path().join("bad.jsonl"), "{}\n").unwrap();
    let bad = error_of(go(&["summary", "--records", &files.path("bad.jsonl")]).0);
    assert!(bad.starts_with("line 1: "), "{bad}");

    let blocked = files.path("plan.json/out");
    let (plan, workload) = (files.path("plan.json"), files.path("workload.json"));
    let (result, _) = go(&[
        "run",
        "--plan",
        &plan,
        "--workload",
        &workload,
        "--out",
        &blocked,
        "--commit",
        "c",
    ]);
    assert!(error_of(result).starts_with(&blocked));

    let unwritable = Files::new("cli-bad-out", PLAN, WORKLOAD);
    fs::create_dir_all(unwritable.dir.path().join("out/records.jsonl")).unwrap();
    let (result, _) = unwritable.run(&[]);
    assert!(error_of(result).contains("records.jsonl"));
}

/// Asserts that a record that cannot be written stops the plan with an error naming the
/// write (the full-device file), after the plan was accepted.
#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_stops_the_run() {
    let files = Files::new("cli-full", PLAN, WORKLOAD);
    let out = files.dir.path().join("out");
    fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new("/dev/full"), out.join("records.jsonl"))
        .unwrap();
    let (result, _) = files.run(&[]);
    assert!(matches!(
        result,
        Err(CliError::Experiment(ExperimentError::Emit(_)))
    ));
}
