//! The farm's trace replays to the report `docs/design/cpu-packing.md` cites, so every number
//! the document takes from it is the replay's, of the committed trace.
//!
//! When the replay or the trace changes on purpose, write the report again with
//! `cargo run -p js-cpu-replay -- run crates/js-cpu-replay/tests/fixtures/farm.trace.gz`
//! into `tests/fixtures/farm.report.md`, and carry what changed into the document.

use std::io::Read as _;

use flate2::read::GzDecoder;
use js_cpu_replay::report::{report, summary};
use js_cpu_replay::sim::Farm;
use js_cpu_replay::trace::parse;

/// The trace: the jobs that started on the farm's six hosts while no job's CPU was held at its
/// booking, as the `convert` command writes them.
const TRACE: &[u8] = include_bytes!("fixtures/farm.trace.gz");

/// Every job of the export that ran: three days, most of them under a `cpu.max` of their booking.
const EXPORT: &[u8] = include_bytes!("fixtures/export.trace.gz");

/// The report of [`TRACE`].
const REPORT: &str = include_str!("fixtures/farm.report.md");

/// The jobs of a compressed trace.
fn jobs(trace: &[u8]) -> Vec<js_cpu_replay::trace::Job> {
    let mut text = String::new();
    GzDecoder::new(trace).read_to_string(&mut text).unwrap();
    parse(&text).unwrap()
}

#[test]
fn the_whole_export_sums_to_the_figures_the_design_cites() {
    assert_eq!(
        summary(&jobs(EXPORT), Farm::OURS),
        "250739 jobs over 68.9 h on 6 hosts of 88 cores: 5755 core-hours of CPU, a mean of 83.5 \
         cores (15.8% of the farm). Recorded wait p50 24 s, p90 774 s, p99 2145 s. The most CPU a \
         run of 10 s or more averaged: 23.3 cores (a shorter run is taken to average at most \
         24.0).\n"
    );
}

#[test]
fn the_farm_trace_replays_to_the_report_the_design_cites() {
    let jobs = jobs(TRACE);
    assert_eq!(jobs.len(), 14_140);
    let replayed = report(&jobs, Farm::OURS);
    assert!(
        replayed == REPORT,
        "the replay of the farm's trace differs from tests/fixtures/farm.report.md:\n{replayed}"
    );
}
