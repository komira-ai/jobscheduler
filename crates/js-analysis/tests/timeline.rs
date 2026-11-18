//! The start timeline: the lines it reads, the quantiles, the table and the program.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::Parser as _;
use js_analysis::timeline::{StartTimelineCli, Timelines, nearest_rank, run};

/// A start with both new fields, a runtime that keeps steps, and a journal prefix.
const NEW: &str = "Nov 18 14:10:01 node-1 js-agent[812]: js-agent: job: Started { run_id: \"r1\", \
    timeline: StartTimeline { received_ms: 5120, placed_at_ms: Some(1000), received_at_ms: \
    Some(1040), steps: [(\"taken\", 0), (\"admitted\", 1), (\"ready\", 9), (\"created\", 12)], \
    runtime: [(\"begin\", 1050), (\"running\", 1070)] } }";

/// A start of an older agent: neither new field, and no runtime steps.
const OLD: &str = "js-agent: job: Started { run_id: \"r2\", timeline: StartTimeline { \
    received_ms: 7, steps: [(\"taken\", 0), (\"admitted\", 3), (\"ready\", 5), (\"created\", 8)], \
    runtime: [] } }";

/// A line of the journal that is not a start.
const OTHER: &str =
    "Nov 18 14:10:02 node-1 js-agent[812]: js-agent: job: Exited { run_id: \"r1\" }";

/// A `Started` line cut short.
const CUT: &str =
    "js-agent: job: Started { run_id: \"r3\", timeline: StartTimeline { received_ms: 1, ste";

/// Reads `text` into a fresh collection.
fn read(text: &str) -> Timelines {
    let mut timelines = Timelines::new();
    timelines.add_text(text);
    timelines
}

/// The `Started` line of one start with the given lifecycle steps and the optional fields.
fn line(placed: &str, received: &str, steps: &str, runtime: &str) -> String {
    format!(
        "job: Started {{ run_id: \"r\", timeline: StartTimeline {{ received_ms: 0, \
         {placed}{received}steps: [{steps}], runtime: [{runtime}] }} }}\n"
    )
}

/// The table is exact for two starts, one with and one without the new fields, with a
/// journal prefix, a line that is not a start and a start that is cut short: the first is
/// the only one with the `placed` rows, and the cut one is counted and left out.
#[test]
fn the_table_of_two_starts_is_exact() {
    let text = format!("{NEW}\n{OTHER}\n{OLD}\n{CUT}\n");
    let timelines = read(&text);
    assert_eq!((timelines.starts(), timelines.malformed()), (2, 1));
    let expected = "\
# Start timeline

Starts: 2. Malformed Started lines: 1.

All values are milliseconds, signed. Percentiles are nearest-rank. `placed->received` and \
`placed->running` span the server's clock and the agent's clock, so a skew between the hosts \
shows in them.

| segment | count | p50 | p90 | p99 | max |
|---|---:|---:|---:|---:|---:|
| placed->received | 1 | 40 | 40 | 40 | 40 |
| received->taken | 2 | 0 | 0 | 0 | 0 |
| received->admitted | 2 | 1 | 3 | 3 | 3 |
| received->ready | 2 | 5 | 9 | 9 | 9 |
| received->created | 2 | 8 | 12 | 12 | 12 |
| taken->admitted | 2 | 1 | 3 | 3 | 3 |
| admitted->ready | 2 | 2 | 8 | 8 | 8 |
| ready->created | 2 | 3 | 3 | 3 | 3 |
| begin->running | 1 | 20 | 20 | 20 | 20 |
| received->running | 1 | 30 | 30 | 30 | 30 |
| placed->running | 1 | 70 | 70 | 70 | 70 |
";
    assert_eq!(timelines.render(), expected);
}

/// An input with no start prints the header, the counts and a line saying there are no
/// starts, and no table.
#[test]
fn an_input_without_starts_says_so() {
    let expected = "\
# Start timeline

Starts: 0. Malformed Started lines: 0.

All values are milliseconds, signed. Percentiles are nearest-rank. `placed->received` and \
`placed->running` span the server's clock and the agent's clock, so a skew between the hosts \
shows in them.

No starts.
";
    assert_eq!(read("").render(), expected);
    assert_eq!(read(OTHER).render(), expected);
}

/// A field that is `None` in the line counts as missing: its rows are not made.
#[test]
fn a_none_field_makes_no_placed_row() {
    let text = line(
        "placed_at_ms: None, ",
        "received_at_ms: Some(50), ",
        "(\"taken\", 0)",
        "(\"running\", 80)",
    );
    let table = read(&text).render();
    assert!(!table.contains("| placed->"));
    assert!(table.contains("| received->running | 1 | 30 | 30 | 30 | 30 |"));
    let text = line(
        "placed_at_ms: Some(10), ",
        "received_at_ms: None, ",
        "(\"taken\", 0)",
        "(\"running\", 80)",
    );
    let table = read(&text).render();
    assert!(!table.contains("| placed->received"));
    assert!(!table.contains("| received->running"));
    assert!(table.contains("| placed->running | 1 | 70 | 70 | 70 | 70 |"));
}

/// A runtime without a `running` step makes no `running` rows.
#[test]
fn a_runtime_without_running_makes_no_running_rows() {
    let text = line(
        "placed_at_ms: Some(10), ",
        "received_at_ms: Some(20), ",
        "(\"taken\", 0)",
        "(\"begin\", 30), (\"image\", 33)",
    );
    let table = read(&text).render();
    assert!(table.contains("| begin->image | 1 | 3 | 3 | 3 | 3 |"));
    assert!(!table.contains("->running |"));
}

/// A reply that arrived before it was placed (clock skew) keeps its negative differences.
#[test]
fn negative_skew_is_kept_signed() {
    let text = line(
        "placed_at_ms: Some(1000), ",
        "received_at_ms: Some(990), ",
        "(\"taken\", 0)",
        "(\"running\", 985)",
    );
    let table = read(&text).render();
    assert!(table.contains("| placed->received | 1 | -10 | -10 | -10 | -10 |"));
    assert!(table.contains("| received->running | 1 | -5 | -5 | -5 | -5 |"));
    assert!(table.contains("| placed->running | 1 | -15 | -15 | -15 | -15 |"));
}

/// A step that goes back in the line gives a negative difference, not a wrapped one.
#[test]
fn a_backward_step_is_negative() {
    let table = read(&line("", "", "(\"a\", 9), (\"b\", 4)", "")).render();
    assert!(table.contains("| a->b | 1 | -5 | -5 | -5 | -5 |"));
}

/// The nearest-rank percentile is the sample at rank ceil(p/100*n): for one sample every
/// percentile is that sample, for two the p50 is the first and the p90 and p99 the second,
/// for ten p90 is the ninth and p99 the tenth, and for a hundred p50, p90 and p99 are the
/// 50th, 90th and 99th.
#[test]
fn nearest_rank_picks_the_sample_at_the_ceiling_rank() {
    let one = [7];
    assert_eq!(
        [50, 90, 99, 100].map(|p| nearest_rank(&one, p)),
        [7, 7, 7, 7]
    );
    let two = [1, 2];
    assert_eq!(
        [50, 90, 99, 100].map(|p| nearest_rank(&two, p)),
        [1, 2, 2, 2]
    );
    let ten: Vec<i64> = (1..=10).collect();
    assert_eq!(
        [50, 90, 99, 100].map(|p| nearest_rank(&ten, p)),
        [5, 9, 10, 10]
    );
    let hundred: Vec<i64> = (1..=100).collect();
    assert_eq!(
        [50, 90, 99, 100].map(|p| nearest_rank(&hundred, p)),
        [50, 90, 99, 100]
    );
    assert_eq!(nearest_rank(&hundred, 1), 1);
}

/// The table sorts the samples of a segment: they come in any order and the percentiles are
/// of the sorted list.
#[test]
fn the_table_sorts_the_samples() {
    let mut text = String::new();
    for value in [30, 10, 20] {
        text.push_str(&line("", "", &format!("(\"taken\", {value})"), ""));
    }
    let table = read(&text).render();
    assert!(table.contains("| received->taken | 3 | 20 | 30 | 30 | 30 |"));
}

/// Every cut of a good line is either not a start or a malformed one, never a start: the
/// parser checks each part of the line and reports the cut at any place as malformed.
#[test]
fn every_cut_of_a_started_line_is_malformed_or_skipped() {
    let full = NEW.trim_start_matches("Nov 18 14:10:01 node-1 js-agent[812]: ");
    for end in (0..full.len()).filter(|&at| full.is_char_boundary(at)) {
        let timelines = read(&full[..end]);
        assert_eq!(timelines.starts(), 0, "cut at {end}");
    }
    let timelines = read(&full[..full.len() - 1]);
    assert_eq!(timelines.malformed(), 1);
}

/// Text that has the shape of a line but is wrong in one place is malformed: a number past
/// the range, a field that is neither `None` nor `Some(..)`, a name with no closing quote.
#[test]
fn a_wrong_part_makes_a_line_malformed() {
    let huge = line("", "", "(\"taken\", 99999999999999999999)", "");
    let wrong = line("placed_at_ms: Maybe, ", "", "", "");
    let open = "job: Started { run_id: \"r1\n";
    let timelines = read(&format!("{huge}{wrong}{open}"));
    assert_eq!((timelines.starts(), timelines.malformed()), (0, 3));
}

/// With no file named the program reads standard input: bytes that are not UTF-8 in another
/// line do not stop the read, and the table of the start is printed.
#[test]
fn the_program_reads_standard_input_and_files() {
    let exe = env!("CARGO_BIN_EXE_js-start-timeline");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = b"\xff\xfe junk\n".to_vec();
    input.extend_from_slice(OLD.as_bytes());
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Starts: 1. Malformed Started lines: 0."));
    assert!(text.contains("| ready->created | 1 | 3 | 3 | 3 | 3 |"));
}

/// A file named on the command line is read instead of standard input, two files are read
/// as one input, and a file that cannot be read fails the program with its path in the
/// message and nothing on standard output.
#[test]
fn files_are_read_in_order_and_a_missing_file_fails() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("timeline_files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.log"), dir.join("b.log"));
    std::fs::write(&a, format!("{NEW}\n")).unwrap();
    std::fs::write(&b, format!("{OLD}\n")).unwrap();

    let cli = StartTimelineCli::parse_from([
        "js-start-timeline",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
    ]);
    let mut out = Vec::new();
    run(&cli, &mut std::io::empty(), &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("Starts: 2."));

    let missing = dir.join("none.log");
    let exe = env!("CARGO_BIN_EXE_js-start-timeline");
    let out = Command::new(exe).arg(&missing).output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("js-start-timeline: cannot read "));
    assert!(err.contains("none.log"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A write to a closed output is reported as an error, not dropped.
#[test]
fn a_failed_write_is_an_error() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let cli = StartTimelineCli::parse_from(["js-start-timeline"]);
    let error = run(&cli, &mut std::io::empty(), &mut Closed).unwrap_err();
    assert_eq!(error.to_string(), "cannot write the output: closed");
}

/// A failed read of standard input is reported as an error.
#[test]
fn a_failed_read_of_standard_input_is_an_error() {
    struct Broken;
    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("broken"))
        }
    }
    let cli = StartTimelineCli::parse_from(["js-start-timeline"]);
    let error = run(&cli, &mut Broken, &mut Vec::new()).unwrap_err();
    assert_eq!(error.to_string(), "cannot write the output: broken");
}
