//! What `js usage` prints of a record's memory series, against the in-memory job server of
//! `fake_jobs`: its text lines and its JSON, beside the record's peak memory and CPU series.

use js_proto::v1::{CpuSeries, GetUsageResponse, MemorySeries, ResourceUsage, UsageRecord};
use prost::Message as _;
use serde_json::{Value, json};

use super::js;
use crate::fake_jobs::{Answer, Fake};
use crate::support::Scratch;

/// A page of two records: the first with a peak memory, a CPU series and a memory series whose
/// points pass 16 bits, the second (a record from an agent that sent no series) with none.
fn page() -> GetUsageResponse {
    GetUsageResponse {
        records: vec![
            UsageRecord {
                job_id: "job-1".to_owned(),
                used: Some(ResourceUsage {
                    peak_memory_bytes: 3 << 30,
                    ..ResourceUsage::default()
                }),
                cpu_series: Some(CpuSeries {
                    granularity_secs: 4,
                    points: vec![16],
                    run_usec: 9_500_000,
                    partial: false,
                }),
                memory_series: Some(MemorySeries {
                    granularity_secs: 4,
                    points: vec![1, 70_000, 3072],
                    run_usec: 9_500_000,
                    partial: true,
                }),
                ..UsageRecord::default()
            },
            UsageRecord {
                job_id: "job-2".to_owned(),
                ..UsageRecord::default()
            },
        ],
        next_page_token: String::new(),
    }
}

/// `js usage`, with `--json` when `json`, against a fake that answers [`page`].
fn usage(name: &str, json: bool) -> (u8, String, String) {
    let scratch = Scratch::new(name);
    let fake = Fake::new();
    fake.answer("GetUsage", vec![Answer::Message(page().encode_to_vec())]);
    let mut words = vec!["usage"];
    words.extend(json.then_some("--json"));
    let got = js(&scratch, fake.dialer(), &words);
    (got.code, got.out, got.err)
}

/// Asserts that `js usage` prints a record's memory series as text, one line per field in the
/// order the schema names them, after the CPU series: its granularity, each point by index
/// (whole, past 16 bits), its run length and its partial flag; and prints nothing of a series
/// for a record without one. The whole output is compared, so a line added, lost, reordered or
/// printed for the record without a series fails. Catches a memory series the command does not
/// show and one shown under the CPU series' name.
#[test]
fn usage_prints_a_records_memory_series_as_text() {
    let (code, out, err) = usage("jobs_usage_memory_text", false);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        out,
        "records[0].job_id: job-1\n\
         records[0].used.peak_memory_bytes: 3221225472\n\
         records[0].cpu_series.granularity_secs: 4\n\
         records[0].cpu_series.points[0]: 16\n\
         records[0].cpu_series.run_usec: 9500000\n\
         records[0].memory_series.granularity_secs: 4\n\
         records[0].memory_series.points[0]: 1\n\
         records[0].memory_series.points[1]: 70000\n\
         records[0].memory_series.points[2]: 3072\n\
         records[0].memory_series.run_usec: 9500000\n\
         records[0].memory_series.partial: true\n\
         records[1].job_id: job-2\n"
    );
}

/// Asserts that `js usage --json` prints a record's memory series as `memorySeries` in proto3
/// JSON (the run length a string, as for every 64-bit number, the points numbers), beside its
/// `cpuSeries`, and no `memorySeries` for a record without one. Catches a series lost from the
/// machine output, a field named otherwise than the gateway names it, and one invented for a
/// record that has none.
#[test]
fn usage_prints_a_records_memory_series_as_json() {
    let (code, out, err) = usage("jobs_usage_memory_json", true);
    assert_eq!((code, err.as_str()), (0, ""));
    let answer: Value = serde_json::from_str(&out).expect("one JSON object");
    assert_eq!(
        answer["records"][0]["memorySeries"],
        json!({
            "granularitySecs": 4,
            "points": [1, 70000, 3072],
            "runUsec": "9500000",
            "partial": true
        })
    );
    assert_eq!(
        answer["records"][0]["cpuSeries"],
        json!({ "granularitySecs": 4, "points": [16], "runUsec": "9500000" })
    );
    assert_eq!(answer["records"][1], json!({ "jobId": "job-2" }));
}
