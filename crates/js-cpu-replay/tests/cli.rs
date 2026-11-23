//! The command line: its parse, its run function and the program itself.

use std::io;
use std::path::PathBuf;
use std::process::Command as Process;

use clap::Parser as _;
use js_cpu_replay::{Cli, Error, run};

/// A scratch directory of one test, `<CARGO_TARGET_TMPDIR>/cli_<test>-<pid>`, removed with
/// everything in it when dropped, so tests running in other processes never share it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(test: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("cli_{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The path of `name` in the directory, as text.
    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }

    /// Writes `bytes` to `name` and returns its path.
    fn put(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One job's record: ten seconds at two cores, ended at 20 s, booked a core and 4 GiB.
const RECORD: &str = "{\"booked\":{\"cpuMillis\":\"1000\",\"memoryBytes\":\"4294967296\"},\
    \"endedAtMs\":\"20000\",\"hostId\":\"node-1\",\"jobId\":\"a\",\"queueWaitMs\":\"0\",\
    \"runMs\":\"10000\",\"used\":{\"cpuUsec\":\"20000000\",\"peakMemoryBytes\":\"1073741824\"}}\n";

/// Its agent's result line.
const RESULT: &str = "job: result run=a class=compile cpu_usage_usec=20000000 \
    cpu_pressure_some_usec=0 cpu_peak_millicores=8000\n";

/// The trace of [`RECORD`] without its result line.
const TRACE: &str =
    "# js-cpu-replay trace 1\n0\t0\t0\t10000\t20000\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0\n";

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("js-cpu-replay").chain(args.iter().copied())).unwrap()
}

/// Runs `args`, returning what it said.
fn said(args: &[&str]) -> Result<String, Error> {
    let mut out = Vec::new();
    run(&parse(args), &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

#[test]
fn an_export_converts_to_a_trace_that_replays_compressed_or_not() {
    let scratch = Scratch::new("convert");
    let records = scratch.put("records.jsonl", RECORD.as_bytes());
    let results = scratch.put("results.txt", RESULT.as_bytes());
    let packed = scratch.path("trace.gz");
    let convert = [
        "convert",
        "--records",
        records.as_str(),
        "--results",
        results.as_str(),
        "--since-ms",
        "0",
        "--out",
        packed.as_str(),
    ];
    assert_eq!(
        said(&convert).unwrap(),
        "1 records: 1 jobs in the trace (0 with a store row, 1 with a result line); 0 did not \
         run, 0 ran on a host of under a hundredth of them, 0 started before the cut\n"
    );
    let report = said(&["run", packed.as_str()]).unwrap();
    assert!(
        report.starts_with("# CPU packing replay\n\n1 jobs over 0.0 h on 6 hosts of 88 cores"),
        "{report}"
    );
    // Without the result lines, to a plain file.
    let plain = scratch.path("trace.txt");
    said(&[
        "convert",
        "--records",
        records.as_str(),
        "--since-ms",
        "0",
        "--out",
        plain.as_str(),
    ])
    .unwrap();
    assert_eq!(std::fs::read_to_string(plain.as_str()).unwrap(), TRACE);
    assert!(
        said(&["run", plain.as_str()])
            .unwrap()
            .starts_with("# CPU packing replay\n")
    );
}

#[test]
fn store_rows_are_read_through_a_scratch_store() {
    let scratch = Scratch::new("rows");
    let records = scratch.put("records.jsonl", RECORD.as_bytes());
    let out = scratch.path("trace.txt");
    let dir = scratch.path("");
    // A file of other rows: no job has a row.
    let rows = scratch.put("rows.txt", b"usage/node/x\tAAAA\n");
    let convert = [
        "convert",
        "--records",
        records.as_str(),
        "--rows",
        rows.as_str(),
        "--scratch",
        dir.as_str(),
        "--since-ms",
        "0",
        "--out",
        out.as_str(),
    ];
    assert!(
        said(&convert)
            .unwrap()
            .contains("(0 with a store row, 0 with a result line)")
    );
    assert_eq!(std::fs::read_to_string(out.as_str()).unwrap(), TRACE);
    // A line that is not a row.
    let bad = scratch.put("bad.txt", b"job/a\n");
    let mut with_bad = convert;
    with_bad[4] = bad.as_str();
    let error = said(&with_bad).unwrap_err();
    assert!(matches!(error, Error::Rows(_)));
    assert_eq!(error.to_string(), "line 1 of the rows: no tab");
    // Rows need a scratch directory.
    assert!(
        Cli::try_parse_from([
            "js-cpu-replay",
            "convert",
            "--records",
            "r",
            "--rows",
            "x",
            "--since-ms",
            "0",
            "--out",
            "o"
        ])
        .is_err()
    );
}

#[test]
fn each_failure_names_its_file_or_line() {
    let scratch = Scratch::new("failures");
    let missing = scratch.path("missing.jsonl");
    let error = said(&["run", missing.as_str()]).unwrap_err();
    assert!(matches!(error, Error::Read { .. }));
    assert!(
        error
            .to_string()
            .starts_with(&format!("cannot read {missing}: "))
    );
    // Not gzip although named so, and not UTF-8.
    let fake = scratch.put("fake.gz", b"plain text");
    assert!(matches!(
        said(&["run", fake.as_str()]),
        Err(Error::Read { .. })
    ));
    let binary = scratch.put("binary.txt", &[0xff, 0xfe]);
    assert!(matches!(
        said(&["run", binary.as_str()]),
        Err(Error::Read { .. })
    ));
    // A trace without its header.
    let headless = scratch.put("headless.txt", b"0\t0\n");
    let error = said(&["run", headless.as_str()]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the trace: the first line is not \"# js-cpu-replay trace 1\""
    );
    // Records that are not JSON.
    let bad = scratch.put("bad.jsonl", b"nope\n");
    let out = scratch.path("out.txt");
    let convert = [
        "convert",
        "--records",
        bad.as_str(),
        "--since-ms",
        "0",
        "--out",
        out.as_str(),
    ];
    assert_eq!(
        said(&convert).unwrap_err().to_string(),
        "line 1 of the records: not a JSON object"
    );
    // A trace into a directory that does not exist, plain or compressed.
    let records = scratch.put("records.jsonl", RECORD.as_bytes());
    for name in ["nowhere/out.txt", "nowhere/out.gz"] {
        let out = scratch.path(name);
        let convert = [
            "convert",
            "--records",
            records.as_str(),
            "--since-ms",
            "0",
            "--out",
            out.as_str(),
        ];
        let error = said(&convert).unwrap_err();
        assert!(matches!(error, Error::Write { .. }), "{error}");
        assert!(
            error
                .to_string()
                .starts_with(&format!("cannot write {out}: "))
        );
    }
}

/// A writer that refuses every write.
struct Refusing;

impl io::Write for Refusing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("refused"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn an_output_that_refuses_is_an_error_of_each_command() {
    let scratch = Scratch::new("output");
    let records = scratch.put("records.jsonl", RECORD.as_bytes());
    let trace = scratch.put("trace.txt", TRACE.as_bytes());
    let out = scratch.path("out.txt");
    for args in [
        vec![
            "convert",
            "--records",
            records.as_str(),
            "--since-ms",
            "0",
            "--out",
            out.as_str(),
        ],
        vec!["run", trace.as_str()],
    ] {
        let error = run(&parse(&args), &mut Refusing).unwrap_err();
        assert_eq!(error.to_string(), "cannot write the output: refused");
    }
}

#[test]
fn the_program_answers_version() {
    let output = Process::new(env!("CARGO_BIN_EXE_js-cpu-replay"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("js-cpu-replay ")
    );
}

#[test]
fn the_program_prints_a_report_and_fails_on_a_missing_trace() {
    let scratch = Scratch::new("program");
    let trace = scratch.put("trace.txt", TRACE.as_bytes());
    let output = Process::new(env!("CARGO_BIN_EXE_js-cpu-replay"))
        .args(["run", trace.as_str()])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("# CPU packing replay\n")
    );
    let output = Process::new(env!("CARGO_BIN_EXE_js-cpu-replay"))
        .args(["run", scratch.path("missing.txt").as_str()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("js-cpu-replay: cannot read")
    );
}
