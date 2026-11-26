use super::*;

/// A check line of `job` as the server's log writes it, with `rest` after the class.
fn line(job: &str, class: &str, rest: &str) -> String {
    format!(
        "2026-11-24T17:00:00.000000Z  INFO js_sched_placer::sizes_profile: {MESSAGE} job={job} \
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
        line("j1", "a b|c", &first),
        line("j2", "b", "cpu=\"partial\" memory=\"partial\""),
        line("j3", "c", "cpu=\"not-a-success\" memory=\"not-a-success\""),
        line("j4", "a b|c", &second),
        line("j5", "c", "cpu=\"not-a-success\" memory=\"not-a-success\""),
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
            job: "j1".to_owned(),
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
            job: "j2".to_owned(),
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
        "5 check lines (and 0 repeating a job's line, left out), 3 classes, 2 with at least 2 \
         lines\n\
         CPU: learned 1, not-a-success 2, not-learned 1, partial 1\n\
         memory: learned 1, no-series 1, not-a-success 2, partial 1\n\
         \n\
         | class | lines | CPU scored | CPU mean error % | CPU peak error % | CPU curve/flat % \
         | CPU shaped | CPU peak over % | CPU series usable % | CPU curve \
         | CPU error removed (core-s) | memory scored | memory mean error % \
         | memory peak error % | memory curve/flat % | memory shaped |\n\
         |---|--:|--:|--:|--:|--:|---|--:|--:|---|--:|--:|--:|--:|--:|---|\n\
         | a b\\|c | 2 | 2 | 23.3 | 12.8 | 66.6 | yes | 0.0 | 100.0 | no | 5.0 | 1 | 10.0 | 5.0 \
         | 50.0 | yes |\n\
         | c | 2 | 0 | - | - | - | - | - | 0.0 | no | 0.0 | 0 | - | - | - | - |\n\
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
    log.push_str(&line(
        "j6",
        "c",
        "cpu=\"not-a-success\" memory=\"not-a-success\"",
    ));
    let table = table(&parse(&log).unwrap(), 1, true);
    let rows: Vec<&str> = table
        .lines()
        .filter(|row| row.starts_with("| class "))
        .collect();
    assert_eq!(
        rows,
        [
            "| class | lines | CPU scored | CPU mean error % | CPU peak error % | CPU curve/flat % \
             | CPU shaped | CPU peak over % | CPU series usable % | CPU curve \
             | CPU error removed (core-s) | memory scored | memory mean error % \
             | memory peak error % | memory curve/flat % | memory shaped |",
            "| class 1 | 3 | 0 | - | - | - | - | - | 0.0 | no | 0.0 | 0 | - | - | - | - |",
            "| class 2 | 2 | 2 | 23.3 | 12.8 | 66.6 | yes | 0.0 | 100.0 | no | 5.0 | 1 | 10.0 \
             | 5.0 | 50.0 | yes |",
            "| class 3 | 1 | 0 | - | - | - | - | - | 0.0 | no | 0.0 | 0 | - | - | - | - |",
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
    let table = table(&parse(&line("j1", "x", &rest)).unwrap(), 1, false);
    assert!(
        table.contains(
            "| x | 1 | 1 | 0.0 | 0.0 | 255.0 | no | 0.0 | 100.0 | no | -1.5 | 0 | - | - | - | - |\n"
        ),
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
    assert!(table(&checks, 16, false).starts_with(
        "0 check lines (and 0 repeating a job's line, left out), 0 classes, 0 with at least 16 \
             lines\nCPU: \nmemory: \n"
    ));
}

#[test]
fn a_check_line_without_its_job_class_or_status_is_refused_by_line() {
    let cases = [
        format!("first\n{MESSAGE} job=j1 cpu=\"learned\""),
        format!("first\n{MESSAGE} job=j1 class=a"),
        format!("first\n{MESSAGE} class=a cpu=\"learned\""),
    ];
    for log in cases {
        let error = parse(&log).unwrap_err();
        assert_eq!(error, ShadowError::Unnamed { line: 2 });
        assert_eq!(error.to_string(), "line 2: no job, class and CPU status");
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
        let error = parse(&line("j1", "a", rest)).unwrap_err();
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

#[test]
fn a_second_line_of_the_same_job_is_left_out_and_counted() {
    let rest = fields_of(
        "cpu",
        "millicores",
        ("learned", "true"),
        [1000, 1000, 1000, 1000, 0, 1000],
    );
    let mut log = log();
    log.push('\n');
    log.push_str(&line("j1", "a b|c", &rest));
    let table = table(&parse(&log).unwrap(), 2, false);
    assert!(
        table.starts_with(
            "5 check lines (and 1 repeating a job's line, left out), 3 classes, 2 with at \
             least 2 lines\nCPU: learned 1, not-a-success 2, not-learned 1, partial 1\n"
        ),
        "{table}"
    );
    assert!(
        table.contains(
            "| a b\\|c | 2 | 2 | 23.3 | 12.8 | 66.6 | yes | 0.0 | 100.0 | no | 5.0 | 1 | 10.0 | 5.0 \
             | 50.0 | yes |\n"
        ),
        "{table}"
    );
}

/// A check line of class `class` whose CPU status is `status`, shaped as the server said. With
/// `peak` it is scored (a mean and level of one core, a predicted peak of one core, a curve
/// error a tenth of the flat one) and peaked at `peak` millicores; without, it carries no
/// figure.
fn cpu_line(job: usize, class: &str, status: &str, peak: Option<u64>) -> String {
    let figures = peak.map_or(String::new(), |peak| {
        format!(
            " cpu_mean_millicores=1000 cpu_level_millicores=1000 cpu_peak_millicores={peak} \
             cpu_predicted_peak_millicores=1000 cpu_curve_error=100 cpu_flat_error=1000"
        )
    });
    line(
        &format!("j{job}"),
        class,
        &format!("cpu=\"{status}\" cpu_shaped=true{figures} memory=\"no-series\""),
    )
}

/// The table row of class `k` of a log of `lines`, split into its cells.
fn cells(lines: &[String]) -> Vec<String> {
    let table = table(&parse(&lines.join("\n")).unwrap(), 1, false);
    let row = table.lines().find(|row| row.starts_with("| k |")).unwrap();
    row.trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

/// Asserts the peak-over column at the edge of an overrun: a peak 25% above the predicted peak
/// is not an overrun and one millicore more is; a scored line with no predicted peak did not
/// overrun but counts in the share; a line with no scores is not counted at all, whatever its
/// peak. The last cell of the share is read in tenths of a percent.
#[test]
fn a_peak_over_a_quarter_above_the_prediction_is_the_only_overrun() {
    let exactly = cpu_line(0, "k", "learned", Some(1250));
    let one_more = cpu_line(1, "k", "learned", Some(1251));
    assert_eq!(cells(std::slice::from_ref(&exactly))[7], "0.0");
    assert_eq!(cells(std::slice::from_ref(&one_more))[7], "100.0");
    assert_eq!(cells(&[exactly.clone(), one_more.clone()])[7], "50.0");
    let unpredicted = line(
        "j2",
        "k",
        "cpu=\"learned\" cpu_shaped=true cpu_peak_millicores=9000 cpu_curve_error=100 \
         cpu_flat_error=1000 memory=\"no-series\"",
    );
    let unscored = line(
        "j3",
        "k",
        "cpu=\"learned\" cpu_shaped=true cpu_peak_millicores=9000 \
         cpu_predicted_peak_millicores=1000 memory=\"no-series\"",
    );
    assert_eq!(cells(&[exactly, unpredicted.clone()])[7], "0.0");
    assert_eq!(cells(&[unpredicted, unscored.clone()])[7], "0.0");
    assert_eq!(cells(&[unscored])[7], "-");
}

/// Asserts the whole row of a class of 16 scored, usable and shaped lines at the edge of check
/// 3: three overruns keep `CPU curve` at `yes`, four turn it to `no`.
#[test]
fn three_overruns_of_sixteen_keep_the_curve_and_four_take_it() {
    for (overruns, over, peak_error, curve) in [(3, "18.7", "4.4", "yes"), (4, "25.0", "5.9", "no")]
    {
        let lines: Vec<String> = (0..16)
            .map(|job| {
                let peak = if job < overruns { 1251 } else { 1000 };
                cpu_line(job, "k", "learned", Some(peak))
            })
            .collect();
        assert_eq!(
            cells(&lines).join("|"),
            format!("k|16|16|0.0|{peak_error}|10.0|yes|{over}|100.0|{curve}|14.4|0|-|-|-|-"),
            "{overruns} overruns"
        );
    }
}

/// Asserts the edge of check 4 as a count: of 16 lines, 12 usable (a mix of `learned` and
/// `not-learned`) keep `CPU curve` at `yes` and 11 turn it to `no`, the usable share reading
/// 75.0 and 68.7; a class of 12 lines, all usable, is `yes` and one of 11 `no`.
#[test]
fn twelve_usable_series_of_sixteen_keep_the_curve_and_eleven_take_it() {
    let lines = |usable: usize, total: usize| -> Vec<String> {
        (0..total)
            .map(|job| match job {
                job if job >= usable => cpu_line(job, "k", "disagrees", None),
                job if job % 2 == 0 => cpu_line(job, "k", "learned", Some(1000)),
                job => cpu_line(job, "k", "not-learned", Some(1000)),
            })
            .collect()
    };
    for (usable, total, share, curve) in [
        (12, 16, "75.0", "yes"),
        (11, 16, "68.7", "no"),
        (12, 12, "100.0", "yes"),
        (11, 11, "100.0", "no"),
    ] {
        let cells = cells(&lines(usable, total));
        assert_eq!(
            (cells[8].as_str(), cells[9].as_str()),
            (share, curve),
            "{usable} of {total}"
        );
    }
}

/// Asserts the windows are the last 16 lines in log order: a 17th line drops the oldest from
/// both. Four overruns, the first line's among them, read four of 16 (`no`) and, with a 17th
/// line that did not overrun, three of the last 16 (`yes`). Twelve usable lines then four that
/// are not read 12 of 16; a 17th usable line pushes the oldest usable one out and still reads
/// 12 (`yes`), a 17th that is not reads 11 (`no`).
#[test]
fn a_seventeenth_line_drops_the_oldest_from_both_windows() {
    let peaks = |list: &[u64]| -> Vec<String> {
        list.iter()
            .enumerate()
            .map(|(job, peak)| cpu_line(job, "k", "learned", Some(*peak)))
            .collect()
    };
    let mut sixteen = vec![1251; 4];
    sixteen.extend([1000; 12]);
    assert_eq!(cells(&peaks(&sixteen))[7..10], ["25.0", "100.0", "no"]);
    sixteen.push(1000);
    assert_eq!(cells(&peaks(&sixteen))[7..10], ["18.7", "100.0", "yes"]);
    // Twelve usable lines, then four that are not: the 17th pushes the first usable one out.
    let usable = |last: &str| -> Vec<String> {
        (0..16)
            .map(|job| {
                if job < 12 {
                    cpu_line(job, "k", "learned", Some(1000))
                } else {
                    cpu_line(job, "k", "no-series", None)
                }
            })
            .chain([cpu_line(16, "k", last, None)])
            .collect()
    };
    assert_eq!(cells(&usable("learned"))[8..10], ["75.0", "yes"]);
    assert_eq!(cells(&usable("partial"))[8..10], ["68.7", "no"]);
}

/// Asserts a class books its curve only while the server's shaped reading holds at the last
/// line that gives one: a last line that says `false` turns `CPU curve` to `no` whatever the
/// overruns and series, and a last line that gives no reading keeps what the line before said.
#[test]
fn a_curve_needs_the_servers_shaped_reading_at_the_last_line_that_gives_one() {
    let mut lines: Vec<String> = (0..12)
        .map(|job| cpu_line(job, "k", "learned", Some(1000)))
        .collect();
    assert_eq!(cells(&lines)[9], "yes");
    lines.push(line("j12", "k", "cpu=\"learned\" memory=\"no-series\""));
    assert_eq!(cells(&lines)[9], "yes");
    lines.push(line(
        "j13",
        "k",
        "cpu=\"learned\" cpu_shaped=false memory=\"no-series\"",
    ));
    assert_eq!(cells(&lines)[9], "no");
}

/// Asserts a log of the release before, whose lines give no predicted peak and no errors, reads
/// without an error and prints `-` for the overruns and `no` for the curve, with the usable
/// share from the statuses it does give.
#[test]
fn a_log_of_the_release_before_prints_a_dash_and_no() {
    let old = line(
        "j0",
        "k",
        "cpu=\"learned\" cpu_runs=3 cpu_shaped=false cpu_mean_millicores=1500 \
         cpu_level_millicores=1437 cpu_peak_millicores=1937 memory=\"no-series\"",
    );
    assert_eq!(cells(&[old])[7..10], ["-", "100.0", "no"]);
}

/// Asserts that a class holding the text ` cpu=` and ` class=` (a size class may hold any
/// printable ASCII) is read whole, and the CPU status after it still read. Catches a class cut
/// at its own ` cpu=`, whose check would be filed under another class with its fields misread.
#[test]
fn a_class_holding_cpu_and_class_words_is_read_whole() {
    let class = "cell//pkg:name build x cpu=1 class=y";
    let checks = parse(&line("j1", class, "cpu=\"not-a-success\"")).unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(
        (checks[0].job.as_str(), checks[0].class.as_str()),
        ("j1", class)
    );
    assert_eq!(checks[0].cpu.status, "not-a-success");
}
