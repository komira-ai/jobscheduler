//! Reading records, grouping, warm-ups, missing values, and which arms are compared.

mod common;

use common::{arm_runs, rec};
use js_analysis::{Error, Options, Report, RunRecord, Verdict, analyze, parse_jsonl};

fn close(got: f64, want: f64) {
    assert!((got - want).abs() <= 1e-9, "got {got}, want {want}");
}

fn report(records: Vec<RunRecord>, options: &Options) -> Report {
    analyze(records, options).unwrap()
}

/// Asserts a record parses from the documented JSON line, a null metric is not available,
/// and the label may be left out.
#[test]
fn a_json_line_is_a_record() {
    let line = r#"{"setting":"0.5","arm":"tierstore","run_index":3,"seed":42,"commit":"abc1234","label":"warmup","metrics":{"queue_wait_s":12.5,"makespan_s":null}}"#;
    let records = parse_jsonl(&format!("\n{line}\n\n")).unwrap();
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!((r.setting.as_str(), r.arm.as_str()), ("0.5", "tierstore"));
    assert_eq!((r.run_index, r.seed, r.label.as_str()), (3, 42, "warmup"));
    assert_eq!(r.metrics["queue_wait_s"], Some(12.5));
    assert_eq!(r.metrics["makespan_s"], None);
    let bare = r#"{"setting":"1","arm":"a","run_index":0,"seed":1,"commit":"c","metrics":{}}"#;
    assert_eq!(parse_jsonl(bare).unwrap()[0].label, "");
}

/// Asserts a record survives a round trip through its JSON line.
#[test]
fn a_record_round_trips_through_json() {
    let r = rec("0.5", "a", 1, "", &[("m", Some(1.5)), ("q", None)]);
    let line = serde_json::to_string(&r).unwrap();
    assert_eq!(parse_jsonl(&line).unwrap(), vec![r]);
}

/// Asserts a bad line is refused with its number, and so is a field the format lacks.
#[test]
fn a_bad_line_is_refused_with_its_number() {
    let good = r#"{"setting":"1","arm":"a","run_index":0,"seed":1,"commit":"c","metrics":{}}"#;
    let error = parse_jsonl(&format!("{good}\nnot json\n")).unwrap_err();
    assert!(matches!(error, Error::Line { line: 2, .. }));
    assert!(error.to_string().starts_with("line 2: "));
    let extra =
        r#"{"setting":"1","arm":"a","run_index":0,"seed":1,"commit":"c","metrics":{},"x":1}"#;
    assert!(matches!(
        parse_jsonl(extra),
        Err(Error::Line { line: 1, .. })
    ));
}

/// Asserts groups come in order of first appearance, keep run order, and hold every record.
#[test]
fn groups_keep_order_and_every_run() {
    let records = vec![
        rec("1", "b", 0, "", &[("m", Some(1.0))]),
        rec("1", "a", 0, "", &[("m", Some(2.0))]),
        rec("2", "b", 0, "", &[("m", Some(3.0))]),
        rec("1", "b", 1, "", &[("m", Some(4.0))]),
        rec("1", "a", 1, "", &[("m", Some(5.0))]),
    ];
    let r = report(records.clone(), &Options::default());
    let keys: Vec<(&str, &str)> = r
        .groups
        .iter()
        .map(|g| (g.setting.as_str(), g.arm.as_str()))
        .collect();
    assert_eq!(keys, [("1", "b"), ("1", "a"), ("2", "b")]);
    assert_eq!(r.groups[0].values("m"), [1.0, 4.0]);
    let listed: usize = r.groups.iter().map(|g| g.runs.len()).sum();
    assert_eq!(listed, records.len());
    assert_eq!(r.groups[0].runs, [records[0].clone(), records[3].clone()]);
}

/// Asserts a warm-up run is listed and counted but not part of any statistic.
#[test]
fn warm_ups_are_labelled_not_removed() {
    let records = vec![
        rec("1", "a", 0, "warmup", &[("m", Some(1000.0))]),
        rec("1", "a", 1, "", &[("m", Some(1.0))]),
        rec("1", "a", 2, "", &[("m", Some(2.0))]),
        rec("1", "a", 3, "note", &[("m", Some(3.0))]),
    ];
    let g = &report(records, &Options::default()).groups[0];
    assert_eq!((g.n_warmup, g.n_runs, g.runs.len()), (1, 3, 4));
    assert_eq!(g.runs[0].label, "warmup");
    let m = &g.metrics[0];
    assert_eq!((m.n, m.n_missing), (3, 0));
    close(m.summary.as_ref().unwrap().mean, 2.0);
    close(m.summary.as_ref().unwrap().max, 3.0);
}

/// Asserts a not-available value is left out of the statistics (not read as zero) and
/// counted, whether the run says null or leaves the metric out.
#[test]
fn missing_values_are_excluded_and_counted() {
    let records = vec![
        rec("1", "a", 0, "", &[("m", Some(2.0))]),
        rec("1", "a", 1, "", &[("m", None)]),
        rec("1", "a", 2, "", &[("m", Some(4.0))]),
        rec("1", "a", 3, "", &[("other", Some(9.0))]),
    ];
    let g = &report(records, &Options::default()).groups[0];
    let m = g.metrics.iter().find(|m| m.metric == "m").unwrap();
    assert_eq!((m.n, m.n_missing), (2, 2));
    close(m.summary.as_ref().unwrap().mean, 3.0);
    let other = g.metrics.iter().find(|m| m.metric == "other").unwrap();
    assert_eq!((other.n, other.n_missing), (1, 3));
}

/// Asserts a metric with no value at all has no summary, and that a metric seen only in a
/// warm-up has none either.
#[test]
fn a_metric_without_values_has_no_summary() {
    let records = vec![
        rec("1", "a", 0, "warmup", &[("w", Some(1.0)), ("q", None)]),
        rec("1", "a", 1, "", &[("q", None)]),
        rec("1", "a", 2, "", &[("q", None)]),
    ];
    let g = &report(records, &Options::default()).groups[0];
    for name in ["q", "w"] {
        let m = g.metrics.iter().find(|m| m.metric == name).unwrap();
        assert!(m.summary.is_none() && m.plans.is_empty() && m.n == 0);
    }
    let q = g.metrics.iter().find(|m| m.metric == "q").unwrap();
    assert_eq!(q.n_missing, 2);
}

/// Asserts two records with the same setting, arm and run index are refused.
#[test]
fn a_repeated_run_index_is_refused() {
    let records = vec![
        rec("1", "a", 0, "", &[]),
        rec("2", "a", 0, "", &[]),
        rec("1", "a", 0, "", &[]),
    ];
    let error = analyze(records, &Options::default()).unwrap_err();
    assert!(matches!(error, Error::DuplicateRun { run_index: 0, .. }));
    assert!(error.to_string().contains("appears twice"));
}

/// Asserts a base arm or std/mean arm that has no run is refused by name.
#[test]
fn an_unknown_arm_is_refused() {
    let records = arm_runs("1", "a", &[1.0, 2.0, 3.0]);
    let baseline = Options {
        baseline: Some("zzz".to_string()),
        ..Options::default()
    };
    let error = analyze(records.clone(), &baseline).unwrap_err();
    assert!(matches!(&error, Error::UnknownArm(name) if name == "zzz"));
    let pair = Options {
        cv_pairs: vec![("a".to_string(), "yyy".to_string())],
        ..Options::default()
    };
    assert!(matches!(analyze(records, &pair), Err(Error::UnknownArm(_))));
}

fn three_arms(setting: &str) -> Vec<RunRecord> {
    let mut records = arm_runs(setting, "base", &[9.0, 10.0, 11.0]);
    records.extend(arm_runs(setting, "a", &[12.0, 13.0, 14.0]));
    records.extend(arm_runs(setting, "b", &[19.0, 20.0, 21.0]));
    records
}

fn pairs(r: &Report) -> Vec<(String, String, String)> {
    r.comparisons
        .iter()
        .map(|c| (c.setting.clone(), c.base.clone(), c.arm.clone()))
        .collect()
}

fn triple(s: &str, b: &str, a: &str) -> (String, String, String) {
    (s.to_string(), b.to_string(), a.to_string())
}

/// Asserts the default base is the first arm of each setting, and that comparisons carry the
/// verdicts of the Welch rule.
#[test]
fn arms_are_compared_with_the_first_arm_by_default() {
    let r = report(three_arms("1"), &Options::default());
    assert_eq!(
        pairs(&r),
        [triple("1", "base", "a"), triple("1", "base", "b")]
    );
    assert!(r.comparisons.iter().all(|c| c.verdict == Verdict::Differs));
    assert!(r.cv_comparisons.is_empty());
}

/// Asserts a named base is used, and that a setting without it has no comparison.
#[test]
fn a_named_base_is_used_where_it_ran() {
    let mut records = three_arms("1");
    records.extend(arm_runs("2", "x", &[1.0, 2.0, 3.0]));
    records.extend(arm_runs("2", "y", &[1.0, 2.0, 3.0]));
    let options = Options {
        baseline: Some("a".to_string()),
        ..Options::default()
    };
    let r = report(records, &options);
    assert_eq!(pairs(&r), [triple("1", "a", "base"), triple("1", "a", "b")]);
}

/// Asserts the ladder compares each arm with the one before it, and a setting with one arm
/// has nothing to compare.
#[test]
fn the_ladder_compares_neighbours() {
    let mut records = three_arms("1");
    records.extend(arm_runs("2", "x", &[1.0, 2.0, 3.0]));
    let options = Options {
        ladder: true,
        ..Options::default()
    };
    let r = report(records, &options);
    assert_eq!(pairs(&r), [triple("1", "base", "a"), triple("1", "a", "b")]);
}

/// Asserts a metric only one arm has is still compared: the comparison cannot be estimated.
#[test]
fn a_metric_one_arm_lacks_is_not_estimable() {
    let mut records = arm_runs("1", "base", &[1.0, 2.0, 3.0]);
    records.push(rec(
        "1",
        "a",
        0,
        "",
        &[("m", Some(1.0)), ("extra", Some(5.0))],
    ));
    let r = report(records, &Options::default());
    let extra = r.comparisons.iter().find(|c| c.metric == "extra").unwrap();
    assert_eq!(extra.verdict, Verdict::NotEstimable);
    assert_eq!((extra.n_base, extra.n_arm), (0, 1));
}

/// Asserts the std/mean comparison is made in every setting where both arms ran.
#[test]
fn spreads_are_compared_where_both_arms_ran() {
    let mut records = three_arms("1");
    records.extend(arm_runs("2", "base", &[1.0, 2.0, 3.0]));
    let options = Options {
        cv_pairs: vec![("base".to_string(), "a".to_string())],
        ..Options::default()
    };
    let r = report(records, &options);
    assert_eq!(r.cv_comparisons.len(), 1);
    let c = &r.cv_comparisons[0];
    assert_eq!(
        (c.setting.as_str(), c.base.as_str(), c.arm.as_str()),
        ("1", "base", "a")
    );
    assert!(c.cv_base.is_some() && c.underpowered);
}

/// Asserts run counts are given for each requested effect when std/mean exists, and only
/// then.
#[test]
fn run_counts_follow_the_requested_effects() {
    let mut records = arm_runs("1", "a", &[9.0, 10.0, 11.0]);
    records.push(rec("1", "a", 5, "", &[("one", Some(4.0))]));
    let options = Options {
        effects: vec![0.1, 0.05],
        ..Options::default()
    };
    let g = &report(records, &options).groups[0];
    let m = g.metrics.iter().find(|m| m.metric == "m").unwrap();
    assert_eq!(m.plans.len(), 2);
    assert_eq!(m.plans[0].effect, 0.1);
    assert!(m.plans[0].min_runs_ci.unwrap() <= m.plans[1].min_runs_ci.unwrap());
    let one = g.metrics.iter().find(|m| m.metric == "one").unwrap();
    assert!(one.plans.is_empty());
}
