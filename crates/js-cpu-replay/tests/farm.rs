//! The farm's trace replays to the report `docs/design/cpu-packing.md` cites, so every number
//! the document takes from it is the replay's, of the committed trace.
//!
//! When the replay or the trace changes on purpose, write the report again with
//! `cargo run -p js-cpu-replay -- run crates/js-cpu-replay/tests/fixtures/farm.trace.gz`
//! into `tests/fixtures/farm.report.md`, and carry what changed into the document.

use std::io::Read as _;

use flate2::read::GzDecoder;
use js_cpu_replay::report::report;
use js_cpu_replay::sim::Farm;
use js_cpu_replay::trace::parse;

/// The trace: the jobs that started on the farm's six hosts while no job's CPU was held at its
/// booking, as the `convert` command writes them.
const TRACE: &[u8] = include_bytes!("fixtures/farm.trace.gz");

/// The report of [`TRACE`].
const REPORT: &str = include_str!("fixtures/farm.report.md");

#[test]
fn the_farm_trace_replays_to_the_report_the_design_cites() {
    let mut text = String::new();
    GzDecoder::new(TRACE).read_to_string(&mut text).unwrap();
    let jobs = parse(&text).unwrap();
    assert_eq!(jobs.len(), 14_140);
    let replayed = report(&jobs, Farm::OURS);
    assert!(
        replayed == REPORT,
        "the replay of the farm's trace differs from tests/fixtures/farm.report.md:\n{replayed}"
    );
}
