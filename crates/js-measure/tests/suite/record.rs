//! The record format: one pinned line the analysis reads, and what a number that cannot
//! be read becomes.

use std::collections::BTreeMap;

use js_measure::{Record, finite};

fn record(label: &str) -> Record {
    let mut metrics = BTreeMap::new();
    metrics.insert("makespan_ms".to_owned(), Some(1500.0));
    metrics.insert("cpu_used_over_booked".to_owned(), None);
    Record {
        setting: "fill".to_owned(),
        arm: "0.8".to_owned(),
        run_index: 3,
        seed: 7,
        commit: "abc123".to_owned(),
        label: label.to_owned(),
        metrics,
    }
}

/// Asserts the exact line a run is written as: the keys the analysis reads, in this
/// order, a number as a number and a missing one as `null`. It catches a renamed key, a
/// missing number written as 0, and an extra key (the reader refuses unknown keys).
#[test]
fn a_record_is_one_pinned_line() {
    let line = serde_json::to_string(&record("")).unwrap();
    assert_eq!(
        line,
        r#"{"setting":"fill","arm":"0.8","run_index":3,"seed":7,"commit":"abc123","label":"","metrics":{"cpu_used_over_booked":null,"makespan_ms":1500.0}}"#
    );
}

/// Asserts that the example line of the analysis's record format reads as a record and
/// writes back as the same line, and that a line without `label` reads as an empty one.
/// It is the same line the analysis crate's own test reads.
#[test]
fn the_analysis_example_line_round_trips() {
    let line = r#"{"setting":"0.5","arm":"tierstore","run_index":3,"seed":42,"commit":"abc1234","label":"","metrics":{"makespan_s":null,"queue_wait_s":12.5}}"#;
    let record: Record = serde_json::from_str(line).unwrap();
    assert_eq!(
        (record.setting.as_str(), record.arm.as_str()),
        ("0.5", "tierstore")
    );
    assert_eq!((record.run_index, record.seed), (3, 42));
    assert_eq!(record.metrics["queue_wait_s"], Some(12.5));
    assert_eq!(record.metrics["makespan_s"], None);
    assert_eq!(serde_json::to_string(&record).unwrap(), line);
    let theirs: js_analysis::RunRecord = serde_json::from_str(line).unwrap();
    assert_eq!(serde_json::to_string(&theirs).unwrap(), line);
    let bare = r#"{"setting":"s","arm":"a","run_index":0,"seed":1,"commit":"c","metrics":{}}"#;
    assert_eq!(serde_json::from_str::<Record>(bare).unwrap().label, "");
}

/// Asserts that a key the format does not have is refused when reading, as the analysis
/// refuses it, so the two cannot drift apart unnoticed.
#[test]
fn an_extra_key_is_refused() {
    let line = r#"{"setting":"s","arm":"a","run_index":0,"seed":1,"commit":"c","label":"","metrics":{},"warmup":null}"#;
    assert!(serde_json::from_str::<Record>(line).is_err());
}

/// Asserts that a warm-up is the label `warmup` and reads back as one.
#[test]
fn a_warmup_label_is_written_and_read_back() {
    let line = serde_json::to_string(&record("warmup")).unwrap();
    assert!(
        line.contains(r#""commit":"abc123","label":"warmup","metrics""#),
        "{line}"
    );
    assert_eq!(
        serde_json::from_str::<Record>(&line).unwrap(),
        record("warmup")
    );
}

/// Asserts that only a finite reading is a number: no reading, NaN and the infinities are
/// `null`. It catches a NaN or an infinity reaching the analysis as a number.
#[test]
fn only_a_finite_reading_is_a_number() {
    assert_eq!(finite(Some(2.5)), Some(2.5));
    assert_eq!(finite(Some(0.0)), Some(0.0));
    for bad in [
        None,
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
    ] {
        assert_eq!(finite(bad), None, "{bad:?}");
    }
}
