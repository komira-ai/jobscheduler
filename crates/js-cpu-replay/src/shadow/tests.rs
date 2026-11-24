use super::*;

/// A check line as the server's log writes it, with `rest` after the class.
fn line(class: &str, rest: &str) -> String {
    format!(
        "2026-11-24T17:00:00.000000Z  INFO js_sched_placer::sizes_profile: {MESSAGE} job=j1 \
         class={class} {rest}"
    )
}

/// A dimension's fields: status, runs, shaped, mean, level, peak, predicted peak, curve
/// error, flat error.
fn fields_of(
    prefix: &str,
    unit: &str,
    (status, shaped): (&str, &str),
    [mean, level, peak, predicted, curve, flat]: [u64; 6],
) -> String {
    format!(
        "{prefix}=\"{status}\" {prefix}_runs=5 {prefix}_shaped={shaped} \
         {prefix}_mean_{unit}={mean} {prefix}_level_{unit}={level} {prefix}_peak_{unit}={peak} \
         {prefix}_predicted_peak_{unit}={predicted} {prefix}_curve_error={curve} \
         {prefix}_flat_error={flat} {prefix}_fit_permille=400"
    )
}

/// The log of three classes: `a b|c` with two checked runs (the second's memory has no
/// series), `b` with one partial run, `c` with two failed runs, and lines of other kinds.
fn log() -> String {
    let first = format!(
        "{} {}",
        fields_of(
            "cpu",
            "millicores",
            ("learned", "false"),
            [1000, 1200, 3000, 2500, 4000, 10000]
        ),
        fields_of(
            "memory",
            "mib",
            ("learned", "true"),
            [100, 90, 200, 210, 50, 100]
        ),
    );
    let second = format!(
        "{} memory=\"no-series\"",
        fields_of(
            "cpu",
            "millicores",
            ("not-learned", "true"),
            [2000, 1500, 4000, 4400, 6000, 5000]
        ),
    );
    [
        "2026-11-24T17:00:00.000000Z  INFO js_lease_server::job_pass: the job pass ended"
            .to_owned(),
        line("a b|c", &first),
        line("b", "cpu=\"partial\" memory=\"partial\""),
        line("c", "cpu=\"not-a-success\" memory=\"not-a-success\""),
        line("a b|c", &second),
        line("c", "cpu=\"not-a-success\" memory=\"not-a-success\""),
    ]
    .join("\n")
}

#[test]
fn check_lines_read_with_every_field_and_other_lines_are_skipped() {
    let checks = parse(&log()).unwrap();
    assert_eq!(checks.len(), 5);
    assert_eq!(
        checks[0],
        Check {
            class: "a b|c".to_owned(),
            cpu: Dim {
                status: "learned".to_owned(),
                shaped: Some(false),
                mean: Some(1000),
                level: Some(1200),
                peak: Some(3000),
                predicted_peak: Some(2500),
                curve_error: Some(4000),
                flat_error: Some(10000),
            },
            memory: Dim {
                status: "learned".to_owned(),
                shaped: Some(true),
                mean: Some(100),
                level: Some(90),
                peak: Some(200),
                predicted_peak: Some(210),
                curve_error: Some(50),
                flat_error: Some(100),
            },
        }
    );
    assert_eq!(
        checks[1],
        Check {
            class: "b".to_owned(),
            cpu: Dim {
                status: "partial".to_owned(),
                ..Dim::default()
            },
            memory: Dim {
                status: "partial".to_owned(),
                ..Dim::default()
            },
        }
    );
    assert_eq!(
        checks[3].memory,
        Dim {
            status: "no-series".to_owned(),
            ..Dim::default()
        }
    );
}

#[test]
fn the_table_adds_up_each_class_of_enough_lines() {
    let checks = parse(&log()).unwrap();
    assert_eq!(
        table(&checks, 2, false),
        "5 check lines, 3 classes, 2 with at least 2 lines\n\
         CPU: learned 1, not-a-success 2, not-learned 1, partial 1\n\
         memory: learned 1, no-series 1, not-a-success 2, partial 1\n\
         \n\
         | class | lines | CPU scored | CPU mean error % | CPU peak error % | CPU curve/flat % \
         | CPU shaped | CPU error removed (core-s) | memory scored | memory mean error % \
         | memory peak error % | memory curve/flat % | memory shaped |\n\
         |---|--:|--:|--:|--:|--:|---|--:|--:|--:|--:|--:|---|\n\
         | a b\\|c | 2 | 2 | 23.3 | 12.8 | 66.6 | yes | 5.0 | 1 | 10.0 | 5.0 | 50.0 | yes |\n\
         | c | 2 | 0 | - | - | - | - | 0.0 | 0 | - | - | - | - |\n\
         \n\
         Classes in the table: 2; shaped by CPU: 1 (50.0%), by memory: 1 (50.0%).\n\
         CPU error the curve removes against the flat line: 5.0 core-s over the table's classes \
         (33.3% of the flat line's 15.0 core-s), 5.0 core-s over its CPU-shaped classes.\n"
    );
}

#[test]
fn hidden_classes_are_numbered_most_lines_first() {
    let mut log = log();
    log.push('\n');
    log.push_str(&line("c", "cpu=\"not-a-success\" memory=\"not-a-success\""));
    let table = table(&parse(&log).unwrap(), 1, true);
    let rows: Vec<&str> = table
        .lines()
        .filter(|row| row.starts_with("| class "))
        .collect();
    assert_eq!(
        rows,
        [
            "| class | lines | CPU scored | CPU mean error % | CPU peak error % | CPU curve/flat % \
             | CPU shaped | CPU error removed (core-s) | memory scored | memory mean error % \
             | memory peak error % | memory curve/flat % | memory shaped |",
            "| class 1 | 3 | 0 | - | - | - | - | 0.0 | 0 | - | - | - | - |",
            "| class 2 | 2 | 2 | 23.3 | 12.8 | 66.6 | yes | 5.0 | 1 | 10.0 | 5.0 | 50.0 | yes |",
            "| class 3 | 1 | 0 | - | - | - | - | 0.0 | 0 | - | - | - | - |",
        ]
    );
    assert!(!table.contains("a b"), "{table}");
}

#[test]
fn a_curve_worse_than_the_flat_line_removes_less_than_nothing() {
    let rest = format!(
        "{} memory=\"no-series\"",
        fields_of(
            "cpu",
            "millicores",
            ("learned", "false"),
            [1000, 1000, 1000, 1000, 2550, 1000]
        ),
    );
    let table = table(&parse(&line("x", &rest)).unwrap(), 1, false);
    assert!(
        table.contains("| x | 1 | 1 | 0.0 | 0.0 | 255.0 | no | -1.5 | 0 | - | - | - | - |\n"),
        "{table}"
    );
    assert!(
        table.ends_with(
            "Classes in the table: 1; shaped by CPU: 0 (0.0%), by memory: 0 (0.0%).\n\
             CPU error the curve removes against the flat line: -1.5 core-s over the table's \
             classes (-155.0% of the flat line's 1.0 core-s), 0.0 core-s over its CPU-shaped \
             classes.\n"
        ),
        "{table}"
    );
}

#[test]
fn a_log_without_check_lines_gives_an_empty_table() {
    let checks = parse("the server started\n").unwrap();
    assert_eq!(checks, []);
    assert_eq!(
        table(&checks, 16, false).lines().last(),
        Some(
            "CPU error the curve removes against the flat line: 0.0 core-s over the table's \
             classes (-% of the flat line's 0.0 core-s), 0.0 core-s over its CPU-shaped classes."
        )
    );
    assert!(
        table(&checks, 16, false)
            .starts_with("0 check lines, 0 classes, 0 with at least 16 lines\nCPU: \nmemory: \n")
    );
}

#[test]
fn a_check_line_without_its_class_or_status_is_refused_by_line() {
    let cases = [
        format!("first\n{MESSAGE} job=j1 cpu=\"learned\""),
        format!("first\n{MESSAGE} job=j1 class=a"),
    ];
    for log in cases {
        let error = parse(&log).unwrap_err();
        assert_eq!(error, ShadowError::NoClass { line: 2 });
        assert_eq!(error.to_string(), "line 2: no class and CPU status");
    }
}

#[test]
fn a_value_that_is_not_a_number_or_a_truth_is_refused_with_its_field() {
    let cases = [
        (
            "cpu=\"learned\" cpu_mean_millicores=1.5",
            "cpu_mean_millicores",
            "1.5",
        ),
        ("cpu=\"learned\" cpu_shaped=maybe", "cpu_shaped", "maybe"),
        (
            "cpu=\"learned\" memory_flat_error=-1",
            "memory_flat_error",
            "-1",
        ),
    ];
    for (rest, field, value) in cases {
        let error = parse(&line("a", rest)).unwrap_err();
        assert_eq!(
            error,
            ShadowError::Value {
                line: 1,
                field: field.to_owned(),
                value: value.to_owned(),
            }
        );
        assert_eq!(error.to_string(), format!("line 1: {field} is {value:?}"));
    }
}

#[test]
fn a_quoted_value_runs_to_its_quote_or_to_the_end() {
    let read = fields("cpu=\"not learned\" cpu_runs=3 memory=\"cut");
    assert_eq!(
        read,
        BTreeMap::from([("cpu", "not learned"), ("cpu_runs", "3"), ("memory", "cut")])
    );
}
