//! The pure parts of `js-launch-measure` (tests/bin/launch_measure.rs): the statistics and the
//! table (tests/launch_measure/table.rs), the flags (tests/launch_measure/flags.rs) and the
//! append to the summary file (tests/launch_measure/out.rs). Fixed inputs in whole
//! milliseconds or microseconds; no clock, no sleep, no container.

#[path = "launch_measure/flags.rs"]
mod flags;
#[path = "launch_measure/out.rs"]
mod out;
#[path = "launch_measure/table.rs"]
mod table;

use std::path::PathBuf;

use flags::{FlagError, parse};
use js_testkit::dir::TestDir;
use table::{
    Facts, Method, Refusal, Row, Sample, Samples, Stat, Variant, broken, difference_line, median,
    p90, report, row, table as render,
};

/// `n` whole milliseconds, in microseconds.
const fn ms(n: u64) -> u64 {
    n * 1000
}

/// A measured sample of `micros`.
fn ok(micros: u64) -> Sample {
    Sample {
        micros,
        failed: None,
    }
}

/// A failed sample of `micros`, with the error `why`.
fn failed(micros: u64, why: &str) -> Sample {
    Sample {
        micros,
        failed: Some(why.to_owned()),
    }
}

/// The samples of `variant`.
fn samples(variant: Variant, samples: Vec<Sample>) -> Samples {
    Samples { variant, samples }
}

/// The row of `variant` over the measured samples `micros`.
fn row_of(variant: Variant, micros: &[u64]) -> Row {
    row(&samples(variant, micros.iter().copied().map(ok).collect())).expect("a row")
}

fn facts() -> Facts {
    Facts {
        image: "ubuntu-24.04".to_owned(),
        kernel: "6.8.0-1000-test".to_owned(),
        crun: "crun version 1.0".to_owned(),
        cpus: 4,
    }
}

const METHOD: Method = Method { runs: 4, warmup: 1 };

/// Asserts that the median of an even count is the mean of the middle two, from samples in any
/// order: 10, 20, 30 and 40 ms give 25.0, shuffled too; an odd count gives its middle value.
/// Catches a median that takes the upper (or lower) middle value, or reads the input unsorted.
#[test]
fn median_of_an_even_count_is_the_mean_of_the_middle_two() {
    let in_order = [ms(10), ms(20), ms(30), ms(40)];
    let shuffled = [ms(30), ms(10), ms(40), ms(20)];
    assert_eq!(
        median(&in_order).map(|m| m.to_string()).as_deref(),
        Some("25.0")
    );
    assert_eq!(
        median(&shuffled).map(|m| m.to_string()).as_deref(),
        Some("25.0")
    );
    assert_eq!(
        median(&[ms(7), ms(1), ms(3)]),
        Some(Stat::from_micros(ms(3)))
    );
    assert_eq!(median(&[]), None);
}

/// Asserts that p90 is the nearest rank, ceil(0.9 n), of the sorted samples: 1..=10 gives 9,
/// 1..=100 gives 90 (both given in reverse order), and one sample gives itself. Catches an
/// off-by-one in the rank and a read of the unsorted input.
#[test]
fn p90_uses_nearest_rank_on_the_sorted_samples() {
    let ten: Vec<u64> = (1..=10).rev().map(ms).collect();
    let hundred: Vec<u64> = (1..=100).rev().map(ms).collect();
    assert_eq!(p90(&ten), Some(Stat::from_micros(ms(9))));
    assert_eq!(p90(&hundred), Some(Stat::from_micros(ms(90))));
    assert_eq!(p90(&[ms(5)]), Some(Stat::from_micros(ms(5))));
    assert_eq!(p90(&[]), None);
}

/// Asserts the table exactly: the header, the separator and one line per variant in the
/// order given, with `runs`, `failed`, `min`, `median`, `p90` and `max` in that order (1 to
/// 10 ms give four different values, so no two columns can trade places unseen). The
/// rounding rule is half away from zero at the tenth of a millisecond: 12.250 ms renders
/// `12.3`, 15.050 ms `15.1` and 20.049 ms `20.0`. Catches a dropped, reordered or misrounded
/// column (rounding half to even would give `12.2`).
#[test]
fn table_has_one_row_per_variant_with_runs_failed_min_median_p90_max() {
    let none_samples: Vec<u64> = (1..=10).rev().map(ms).collect();
    let none = row_of(Variant::None, &none_samples);
    let wired = row_of(Variant::Wired, &[20_049, 12_250, 16_000, 15_050, 13_000]);
    assert_eq!(
        render(&[none, wired]),
        "| variant | runs | failed | min | median | p90 | max |\n\
         |---|---:|---:|---:|---:|---:|---:|\n\
         | none | 10 | 0 | 1.0 | 5.5 | 9.0 | 10.0 |\n\
         | wired | 5 | 0 | 12.3 | 15.1 | 20.0 | 20.0 |\n"
    );
}

/// Asserts the rounding of a difference: half away from zero on both sides of zero, and a
/// difference that rounds to zero prints `0.0`, never `-0.0`. Catches a rounding that is
/// right only for positive values.
#[test]
fn a_negative_difference_rounds_half_away_from_zero_and_never_prints_minus_zero() {
    let at = Stat::from_micros;
    assert_eq!(at(1_000).minus(at(7_550)).to_string(), "-6.6");
    assert_eq!(at(7_550).minus(at(1_000)).to_string(), "6.6");
    assert_eq!(at(1_000).minus(at(1_049)).to_string(), "0.0");
    assert_eq!(at(0).to_string(), "0.0");
}

/// Asserts the difference line: none's median 20.0 and wired's 26.5 give
/// `wired - none (median): 6.5 ms`, whatever order the rows come in; with the medians the
/// other way round it is `-6.5 ms`, sign included; without both variants there is no line.
/// Catches swapped operands.
#[test]
fn the_difference_line_is_wired_minus_none_of_the_medians() {
    let none = row_of(Variant::None, &[ms(20)]);
    let wired = row_of(Variant::Wired, &[ms(26), ms(27)]);
    let line = "wired - none (median): 6.5 ms";
    assert_eq!(difference_line(&[none, wired]).as_deref(), Some(line));
    assert_eq!(difference_line(&[wired, none]).as_deref(), Some(line));
    let none = row_of(Variant::None, &[ms(26), ms(27)]);
    let wired = row_of(Variant::Wired, &[ms(20)]);
    assert_eq!(
        difference_line(&[none, wired]).as_deref(),
        Some("wired - none (median): -6.5 ms")
    );
    assert_eq!(difference_line(&[none]), None);
}

/// Asserts that a failed sample is counted and kept out of every statistic: of five samples
/// one failed after 1000 ms, so the row has runs 4, failed 1 and the statistics of the other
/// four (with it, the median would be 30.0 and the max 1000.0), and the report lists the
/// failed sample, numbered and with its error text, under the table. Catches a failed sample
/// averaged in, or one dropped without a trace.
#[test]
fn failed_samples_are_counted_and_left_out_of_the_statistics() {
    let none = samples(
        Variant::None,
        vec![
            ok(ms(10)),
            ok(ms(20)),
            failed(ms(1000), "start: crun said no"),
            ok(ms(30)),
            ok(ms(40)),
        ],
    );
    let counted = row(&none).expect("one of five failed is a measurement");
    assert_eq!(
        render(&[counted]),
        format!(
            "{}{}",
            table::HEADER.map(|line| format!("{line}\n")).concat(),
            "| none | 4 | 1 | 10.0 | 25.0 | 40.0 | 40.0 |\n"
        )
    );
    let wired = samples(Variant::Wired, vec![ok(ms(30)); 4]);
    let text = report(&[none, wired], METHOD, &facts()).expect("a report");
    let row_at = text.find("| none | 4 | 1 |").expect("the row");
    let listed = text
        .find("\n- none sample 3: start: crun said no\n")
        .expect("the failed sample is listed");
    assert!(row_at < listed, "listed under the table:\n{text}");
    assert!(text.contains("wired - none (median): 5.0 ms\n"), "{text}");
}

/// Asserts the boundary of a broken measurement, more than a quarter failed of all the
/// variant's samples: 3 of 8 failed is broken (an error, no row), 2 of 8 is a row of 6 runs.
/// Catches `>=` at the boundary, and a denominator of the measured samples only (6, against
/// which 2 failed would be more than a quarter).
#[test]
fn a_variant_with_more_than_a_quarter_failed_is_a_broken_measurement() {
    assert!(broken(3, 8));
    assert!(!broken(2, 8));
    let of = |failures: usize| {
        let mut taken = vec![ok(ms(10)); 8 - failures];
        taken.extend((0..failures).map(|_| failed(ms(1), "no report")));
        samples(Variant::Wired, taken)
    };
    let refused = row(&of(3)).expect_err("3 of 8 failed is broken");
    assert_eq!(
        refused,
        Refusal::Broken {
            variant: Variant::Wired,
            failed: 3,
            total: 8,
        }
    );
    assert_eq!(
        refused.to_string(),
        "variant wired: 3 of 8 samples failed, more than a quarter: a broken measurement, no \
         table"
    );
    let kept = row(&of(2)).expect("2 of 8 failed is a measurement");
    assert_eq!((kept.runs, kept.failed), (6, 2));
    let none = samples(Variant::None, vec![ok(ms(10)); 8]);
    assert_eq!(report(&[none, of(3)], METHOD, &facts()), Err(refused));
}

/// Asserts that a variant with no measured sample is an error naming it, never a row of
/// zeros: every sample failed, or none taken at all (`--runs 0`); the report is refused too.
/// Catches an empty set rendered as `0.0`.
#[test]
fn zero_measured_samples_is_an_error_not_a_table_of_zeros() {
    let all_failed = samples(Variant::Wired, vec![failed(ms(3), "no report"); 2]);
    let refused = row(&all_failed).expect_err("nothing measured");
    assert_eq!(
        refused.to_string(),
        "variant wired: no measured sample (2 failed): nothing to show"
    );
    let none_taken = samples(Variant::None, Vec::new());
    let refused = row(&none_taken).expect_err("nothing taken");
    assert_eq!(
        refused.to_string(),
        "variant none: no measured sample (0 failed): nothing to show"
    );
    let wired = samples(Variant::Wired, vec![ok(ms(10))]);
    assert_eq!(
        report(&[none_taken, wired], METHOD, &facts()),
        Err(Refusal::NoSample {
            variant: Variant::None,
            failed: 0,
        })
    );
}

/// Asserts the whole report around the table: the method with the run and warm-up counts,
/// the rule of the numbers, and the facts of the runner after it. Catches a report that
/// drops the method or a fact.
#[test]
fn the_report_states_the_method_and_the_runner_facts() {
    let none = samples(Variant::None, vec![ok(ms(10)); 4]);
    let wired = samples(Variant::Wired, vec![ok(ms(12)); 4]);
    let text = report(&[none, wired], METHOD, &facts()).expect("a report");
    assert!(text.starts_with("### crun launch time\n\n"), "{text}");
    assert!(
        text.contains(" 4 measured samples per variant after 1 discarded,"),
        "{text}"
    );
    assert!(
        text.contains("Milliseconds, one decimal place, rounded half away from zero"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "wired - none (median): 2.0 ms\n\nRunner:\n\n- image: ubuntu-24.04\n- kernel: \
             6.8.0-1000-test\n- crun: crun version 1.0\n- CPUs: 4\n"
        ),
        "{text}"
    );
}

/// Asserts that the summary is appended: a file that already holds text keeps it and gains
/// the new text after it, twice over; a missing file is made. Catches an open with truncate.
#[test]
fn summary_output_is_appended_to_the_file_not_truncated() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "launch-table-append");
    let path = dir.join("summary.md");
    std::fs::write(&path, "### earlier step\n").expect("write");
    out::append(&path, "| table |\n").expect("append");
    out::append(&path, "again\n").expect("append");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "### earlier step\n| table |\nagain\n"
    );
    let fresh = dir.join("fresh.md");
    out::append(&fresh, "first\n").expect("append to a missing file");
    assert_eq!(std::fs::read_to_string(&fresh).expect("read"), "first\n");
}

/// The required flags, and then `extra`.
fn args(extra: &[&str]) -> Vec<String> {
    let required = [
        "--image-name",
        "ubuntu-24.04",
        "--crun",
        "/usr/bin/crun",
        "--busybox",
        "/usr/bin/busybox",
        "--job-user",
        "1001:1002",
        "--work-dir",
        "/srv/measure",
    ];
    required
        .iter()
        .chain(extra)
        .map(|arg| (*arg).to_owned())
        .collect()
}

/// Asserts the flags: the defaults are 50 runs and 5 warm-up samples with no summary file;
/// each flag given is read into its own field; `--runs abc` and an unknown flag are errors
/// that name the flag, never a fall back to the default. Catches a bad value read as the
/// default, and a flag read into another's field.
#[test]
fn flags_parse_runs_warmup_summary_file_and_image_name_and_reject_unknown_ones() {
    let defaults = parse(&args(&[])).expect("the required flags");
    assert_eq!((defaults.runs, defaults.warmup), (50, 5));
    assert_eq!(
        (defaults.runs, defaults.warmup),
        (flags::RUNS, flags::WARMUP)
    );
    assert_eq!(defaults.summary_file, None);
    assert_eq!(defaults.image_name, "ubuntu-24.04");
    assert_eq!(defaults.crun, PathBuf::from("/usr/bin/crun"));
    assert_eq!(defaults.busybox, PathBuf::from("/usr/bin/busybox"));
    assert_eq!(defaults.job_user, (1001, 1002));
    assert_eq!(defaults.work_dir, PathBuf::from("/srv/measure"));
    let given = parse(&args(&[
        "--runs",
        "7",
        "--warmup",
        "0",
        "--summary-file",
        "out/summary.md",
    ]))
    .expect("every flag");
    assert_eq!((given.runs, given.warmup), (7, 0));
    assert_eq!(given.summary_file, Some(PathBuf::from("out/summary.md")));

    let bad = parse(&args(&["--runs", "abc"])).expect_err("not a number");
    assert_eq!(
        bad.to_string(),
        "--runs \"abc\": it takes a whole number from 0 to 4294967295"
    );
    let bad = parse(&args(&["--warmup", "-1"])).expect_err("not a whole number");
    assert_eq!(
        bad.to_string(),
        "--warmup \"-1\": it takes a whole number from 0 to 4294967295"
    );
    let unknown = parse(&args(&["--fast", "1"])).expect_err("unknown");
    assert_eq!(unknown, FlagError::Unknown("--fast".to_owned()));
    assert_eq!(
        unknown.to_string(),
        "--fast: not a flag of js-launch-measure"
    );
}

/// Asserts the other refusals, each naming its flag: a required flag left out, a flag given
/// twice, a flag with no value, a root or malformed `--job-user`, and a relative program path.
/// Catches a refusal that names the wrong flag, and a root job user let through.
#[test]
fn flags_refuse_a_missing_flag_a_repeated_one_a_root_job_user_and_a_relative_path() {
    let without_image: Vec<String> = args(&[]).into_iter().skip(2).collect();
    assert_eq!(
        parse(&without_image).map(|_| ()),
        Err(FlagError::Missing("--image-name"))
    );
    assert_eq!(
        parse(&args(&["--runs", "3", "--runs", "4"])).map(|_| ()),
        Err(FlagError::Twice("--runs"))
    );
    assert_eq!(
        parse(&args(&["--summary-file"])).map(|_| ()),
        Err(FlagError::NoValue("--summary-file"))
    );
    let mut root = args(&[]);
    root[7] = "0:0".to_owned();
    assert_eq!(
        parse(&root).map(|_| ()).map_err(|why| why.to_string()),
        Err("--job-user \"0:0\": it takes <uid>:<gid> of a user that is not root".to_owned())
    );
    root[7] = "1001".to_owned();
    assert_eq!(
        parse(&root).map(|_| ()).map_err(|why| why.to_string()),
        Err("--job-user \"1001\": it takes <uid>:<gid> of a user that is not root".to_owned())
    );
    let mut relative = args(&[]);
    relative[3] = "crun".to_owned();
    assert_eq!(
        parse(&relative).map(|_| ()).map_err(|why| why.to_string()),
        Err("--crun \"crun\": it takes an absolute path".to_owned())
    );
}
