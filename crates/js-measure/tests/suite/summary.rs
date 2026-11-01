//! Grouping and the hand-off: what the summary step does with records, without computing
//! a statistic.

use std::collections::BTreeMap;

use js_measure::{Listing, Record, Summarise, group, read_records};

fn rec(arm: &str, run_index: u32, warmup: bool, makespan: Option<f64>) -> Record {
    Record {
        setting: "fill".to_owned(),
        arm: arm.to_owned(),
        run_index,
        seed: 1,
        commit: "c".to_owned(),
        label: if warmup { "warmup" } else { "" }.to_owned(),
        metrics: BTreeMap::from([
            ("makespan_ms".to_owned(), makespan),
            ("jobs".to_owned(), Some(2.0)),
        ]),
    }
}

/// Asserts that records are grouped by setting and arm, the groups sorted, each group in
/// run order, and every record kept (warm-ups included). It catches a run dropped or
/// reordered on the way to the analysis.
#[test]
fn records_are_grouped_by_arm_in_run_order() {
    let records = vec![
        rec("b", 4, false, Some(1.0)),
        rec("a", 5, false, Some(1.0)),
        rec("a", 2, true, None),
        rec("b", 0, false, Some(1.0)),
    ];
    let groups = group(records);
    let shape: Vec<(&str, Vec<u32>)> = groups
        .iter()
        .map(|g| (g.arm.as_str(), g.runs.iter().map(|r| r.run_index).collect()))
        .collect();
    assert_eq!(shape, vec![("a", vec![2, 5]), ("b", vec![0, 4])]);
    assert_eq!(groups[0].setting, "fill");
}

/// Asserts the listing: runs, warm-ups, run indexes and the metrics that are missing in
/// how many runs, nothing computed from a value.
#[test]
fn the_listing_counts_runs_and_missing_numbers() {
    let groups = group(vec![
        rec("a", 5, false, Some(1.0)),
        rec("a", 2, true, None),
        rec("b", 1, false, Some(9.0)),
    ]);
    assert_eq!(
        Listing.summarise(&groups),
        "fill a: 2 runs (1 warm-up); run_index 2 5; not available: makespan_ms=1\n\
         fill b: 1 runs (0 warm-up); run_index 1; not available: none\n"
    );
}

/// Asserts that the records file is read line by line, blank lines skipped, and a bad
/// line is named by its number.
#[test]
fn records_are_read_line_by_line() {
    let line = serde_json::to_string(&rec("a", 0, false, Some(1.0))).unwrap();
    let text = format!("{line}\n\n{line}\n");
    assert_eq!(read_records(&text).unwrap().len(), 2);
    let err = read_records(&format!("{line}\nnot json\n")).unwrap_err();
    assert_eq!(err.line, 2);
    assert!(err.to_string().starts_with("line 2: "), "{err}");
}
