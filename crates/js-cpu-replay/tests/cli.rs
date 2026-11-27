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
        "1 records: 1 jobs in the trace (0 with a store row, 1 with a result line, 0 with a CPU \
         series); 0 did not run, 0 ran on a host of under a hundredth of them, 0 started before \
         the cut\n"
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

/// [`RECORD`] with the series its host recorded: five points of 2 s at 2 cores.
const RECORD_WITH_SERIES: &str = "{\"booked\":{\"cpuMillis\":\"1000\",\"memoryBytes\":\"4294967296\"},\
    \"endedAtMs\":\"20000\",\"hostId\":\"node-1\",\"jobId\":\"a\",\"queueWaitMs\":\"0\",\
    \"runMs\":\"10000\",\"used\":{\"cpuUsec\":\"20000000\",\"peakMemoryBytes\":\"1073741824\"},\
    \"cpuSeries\":{\"granularitySecs\":2,\"points\":[32,32,32,32,32],\"runUsec\":\"10000000\"}}\n";

/// Asserts that a record's CPU series is counted and written into the trace under the second
/// header, that `run --learn-days 2` adds the gate's section after the report (here saying that
/// nothing is held out, as the one job arrives on the first day), that without the flag the
/// report has no gate, and that fewer than two learning days are refused. Catches a series the
/// command line drops, a gate printed when not asked for, and a gate taken on one learning day.
#[test]
fn a_series_reaches_the_trace_and_the_gate_is_asked_for_by_learning_days() {
    let scratch = Scratch::new("series");
    let records = scratch.put("records.jsonl", RECORD_WITH_SERIES.as_bytes());
    let out = scratch.path("trace.txt");
    let convert = [
        "convert",
        "--records",
        records.as_str(),
        "--since-ms",
        "0",
        "--out",
        out.as_str(),
    ];
    assert!(said(&convert).unwrap().contains("1 with a CPU series"));
    assert_eq!(
        std::fs::read_to_string(out.as_str()).unwrap(),
        "# js-cpu-replay trace 2\n0\t0\t0\t10000\t20000\t-\t-\t1024\t1000\t4096\t0\tok\t1\t-\t0\t\
         2:10000000:whole:32,32,32,32,32\n"
    );
    let gated = said(&["run", out.as_str(), "--learn-days", "2"]).unwrap();
    assert!(
        gated.ends_with(
            "## The gate on held-out days\n\n\
             No job arrived after the trace's first 2 days: nothing is held out.\n"
        ),
        "{gated}"
    );
    let plain = said(&["run", out.as_str()]).unwrap();
    assert!(!plain.contains("## The gate"), "{plain}");
    assert!(gated.starts_with(&plain));
    for days in ["1", "0", "x"] {
        assert!(
            Cli::try_parse_from(["js-cpu-replay", "run", "t", "--learn-days", days]).is_err(),
            "{days}"
        );
    }
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
            .contains("(0 with a store row, 0 with a result line, 0 with a CPU series)")
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
        "the trace: the first line is not \"# js-cpu-replay trace 1\" or \
         \"# js-cpu-replay trace 2\""
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

/// A check line of the server's log, compressed or not.
const CHECK: &str = "2026-11-24T16:48:06.974256Z  INFO js_sched_placer::sizes::profile: a run \
    was checked against its class's profile job=k1 class=build cpu=\"learned\" cpu_runs=3 \
    cpu_shaped=false cpu_mean_millicores=1500 cpu_level_millicores=1437 \
    cpu_peak_millicores=1937 memory=\"learned\" memory_runs=3 memory_shaped=false \
    memory_mean_mib=144 memory_level_mib=91 memory_peak_mib=301\n";

/// A check line of the current release: the run peaked at 3 cores over a predicted peak of 2.
const CHECK_NOW: &str = "2026-11-24T16:48:07.974256Z  INFO js_sched_placer::sizes::profile: a run \
    was checked against its class's profile job=k2 class=tool cpu=\"learned\" cpu_runs=9 \
    cpu_shaped=true cpu_mean_millicores=1000 cpu_level_millicores=1000 \
    cpu_peak_millicores=3000 cpu_predicted_peak_millicores=2000 cpu_curve_error=100 \
    cpu_flat_error=1000 memory=\"no-series\"\n";

/// Asserts the table prints a log of the release before (no predicted peak) as `-` for the
/// overruns and `no` for the curve, the same log compressed and a line with no class by line,
/// and the current release's line as one overrun of one scored line (a class of one line is not yet `yes`).
#[test]
fn shadow_prints_the_table_of_a_logs_check_lines() {
    let scratch = Scratch::new("shadow");
    let plain = scratch.put("server.log", CHECK.as_bytes());
    let table = said(&["shadow", plain.as_str(), "--min-runs", "1"]).unwrap();
    let now = scratch.put("now.log", CHECK_NOW.as_bytes());
    let table_now = said(&["shadow", now.as_str(), "--min-runs", "1"]).unwrap();
    assert!(
        table_now.contains(
            "| tool | 1 | 1 | 0.0 | 33.3 | 10.0 | yes | 100.0 | 100.0 | no | 0.9 | 0 | - | - | - | - |\n"
        ),
        "{table_now}"
    );
    assert!(
        table.contains(
            "| build | 1 | 0 | 4.2 | - | - | no | - | 100.0 | no | 0.0 | 0 | 36.8 | - | - | no |\n"
        ),
        "{table}"
    );
    // Compressed, at the default of 16 lines a class.
    let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut packed, CHECK.as_bytes()).unwrap();
    let packed = scratch.put("server.log.gz", &packed.finish().unwrap());
    assert!(said(&["shadow", packed.as_str()]).unwrap().starts_with(
        "1 check lines (and 0 repeating a job's line, left out), 1 classes, 0 with at \
                 least 16 lines\n"
    ));
    // A check line with no class names its line.
    let broken = scratch.put(
        "broken.log",
        b"a run was checked against its class's profile\n",
    );
    let error = said(&["shadow", broken.as_str()]).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the log: line 1: no job, class and CPU status"
    );
}
