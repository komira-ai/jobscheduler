//! The contract pieces of a hook that no service changes: exit codes, the last-line report,
//! the step words, the path spelling rule, the runner default and the timing bounds.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use js_update_hook::{
    CUTOVER_MARGIN_SECS, Class, CommandRunner, MAX_REASON_LEN, Outcome, POLL_SECS,
    PREPARE_MARGIN_SECS, ROLLBACK_MARGIN_SECS, ReasonToken, Report, START_WAIT_SECS, Step, Streams,
    UnknownStep, VERIFY_MARGIN_SECS, is_plain, is_reason_token,
};

/// A closed set of reasons for the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Busy,
    Broken,
}

impl ReasonToken for Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::Busy => "host_busy",
            Why::Broken => "unit_broken",
        }
    }
}

/// A writer whose every write fails.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("disk full"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A runner that implements only the required methods.
struct Minimal;

impl CommandRunner for Minimal {
    fn run(&self, _program: &str, _args: &[&str]) -> Result<String, String> {
        Ok(String::new())
    }
    fn run_in(
        &self,
        _program: &str,
        _args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        Ok(String::new())
    }
}

/// The four outcomes exit with 0, 1, 75 and 76.
#[test]
fn outcomes_exit_with_zero_one_seventy_five_and_seventy_six() {
    assert_eq!(Outcome::Success.code(), 0);
    assert_eq!(Outcome::Failure.code(), 1);
    assert_eq!(Outcome::Deferred.code(), 75);
    assert_eq!(Outcome::Halt.code(), 76);
}

/// An outcome converts into the process exit code of its number.
#[test]
fn an_outcome_converts_into_the_exit_code_of_its_number() {
    for outcome in [
        Outcome::Success,
        Outcome::Failure,
        Outcome::Deferred,
        Outcome::Halt,
    ] {
        let code: ExitCode = outcome.into();
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", ExitCode::from(outcome.code()))
        );
    }
}

/// A report that says nothing is a release failure that is reversible with nothing pending.
#[test]
fn the_default_report_is_a_reversible_release_with_nothing_set() {
    let report = Report::<Why>::default();
    assert_eq!(report.class, Class::Release);
    assert_eq!(
        report.to_line(),
        r#"{"class":"release","promotion_pending":false,"reversible":true}"#
    );
}

/// `failure` sets a reason and a text and keeps the other defaults.
#[test]
fn a_failure_report_carries_reason_and_detail_and_the_defaults() {
    let report = Report::failure(Why::Broken, String::from("unit did not start"));
    assert_eq!(report.reason, Some(Why::Broken));
    assert_eq!(report.detail.as_deref(), Some("unit did not start"));
    assert_eq!(report.class, Class::Release);
    assert!(report.reversible);
    assert!(!report.promotion_pending);
    assert_eq!(report.retry_after_secs, None);
    assert_eq!(
        report.to_line(),
        r#"{"class":"release","detail":"unit did not start","promotion_pending":false,"reason":"unit_broken","reversible":true}"#
    );
}

/// Every optional field shows up in the line when it is set.
#[test]
fn a_report_with_every_field_set_prints_every_field() {
    let report = Report {
        class: Class::Environment,
        reason: Some(Why::Busy),
        retry_after_secs: Some(300),
        reversible: false,
        promotion_pending: true,
        detail: Some("two leases".to_owned()),
    };
    assert_eq!(
        report.to_line(),
        r#"{"class":"environment","detail":"two leases","promotion_pending":true,"reason":"host_busy","retry_after_secs":300,"reversible":false}"#
    );
}

/// Each optional field is left out alone when it is the only one unset.
#[test]
fn each_optional_field_is_left_out_when_unset() {
    let full = Report {
        class: Class::Release,
        reason: Some(Why::Busy),
        retry_after_secs: Some(5),
        reversible: true,
        promotion_pending: false,
        detail: Some("d".to_owned()),
    };
    let no_reason = Report {
        reason: None,
        ..full.clone()
    };
    assert_eq!(
        no_reason.to_line(),
        r#"{"class":"release","detail":"d","promotion_pending":false,"retry_after_secs":5,"reversible":true}"#
    );
    let no_retry = Report {
        retry_after_secs: None,
        ..full.clone()
    };
    assert_eq!(
        no_retry.to_line(),
        r#"{"class":"release","detail":"d","promotion_pending":false,"reason":"host_busy","reversible":true}"#
    );
    let no_detail = Report {
        detail: None,
        ..full
    };
    assert_eq!(
        no_detail.to_line(),
        r#"{"class":"release","promotion_pending":false,"reason":"host_busy","retry_after_secs":5,"reversible":true}"#
    );
}

/// The detail text is escaped so the line stays one JSON object on one line.
#[test]
fn a_detail_with_quotes_and_newlines_stays_on_one_line() {
    let report = Report::failure(Why::Busy, "say \"hi\"\nnow");
    let line = report.to_line();
    assert!(!line.contains('\n'));
    assert!(line.contains(r#""detail":"say \"hi\"\nnow""#));
}

/// `write_to` writes the line and one newline.
#[test]
fn write_to_writes_the_line_and_a_newline() {
    let report = Report::failure(Why::Busy, "x");
    let mut out = Vec::new();
    report.write_to(&mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{}\n", report.to_line())
    );
}

/// A writer that fails makes `write_to` return that error.
#[test]
fn write_to_returns_the_error_of_a_failing_writer() {
    let err = Report::<Why>::default()
        .write_to(&mut Refusing)
        .unwrap_err();
    assert_eq!(err.to_string(), "disk full");
}

/// The reason tokens of the test enum are all valid tokens.
#[test]
fn the_test_reasons_are_valid_tokens() {
    assert!(is_reason_token(Why::Busy.as_str()));
    assert!(is_reason_token(Why::Broken.as_str()));
}

/// A reason token is one to 64 bytes of lowercase letters, digits and underscore.
#[test]
fn reason_tokens_are_one_to_sixty_four_lowercase_digit_or_underscore_bytes() {
    assert_eq!(MAX_REASON_LEN, 64);
    assert!(!is_reason_token(""));
    assert!(is_reason_token("a"));
    assert!(is_reason_token(&"a".repeat(64)));
    assert!(!is_reason_token(&"a".repeat(65)));
    assert!(is_reason_token("a_1"));
    assert!(is_reason_token("0123456789"));
    assert!(is_reason_token("_"));
    assert!(!is_reason_token("Abc"));
    assert!(!is_reason_token("abC"));
    assert!(!is_reason_token("a-b"));
    assert!(!is_reason_token("a b"));
    assert!(!is_reason_token("é"));
}

/// The two classes are spelt as the updater reads them, and release is the default.
#[test]
fn classes_are_spelt_release_and_environment_and_release_is_the_default() {
    assert_eq!(Class::Release.as_str(), "release");
    assert_eq!(Class::Environment.as_str(), "environment");
    assert_eq!(Class::default(), Class::Release);
}

/// The steps are listed in the order a successful update runs them.
#[test]
fn all_steps_are_listed_in_contract_order() {
    assert_eq!(
        Step::ALL,
        [
            Step::Prepare,
            Step::Cutover,
            Step::Verify,
            Step::Rollback,
            Step::Cleanup
        ]
    );
}

/// Each step has its command-line word, shown the same by Display.
#[test]
fn each_step_has_its_word_and_displays_it() {
    let words = ["prepare", "cutover", "verify", "rollback", "cleanup"];
    for (step, word) in Step::ALL.into_iter().zip(words) {
        assert_eq!(step.name(), word);
        assert_eq!(step.to_string(), word);
    }
}

/// Every step word parses back to its step.
#[test]
fn every_step_word_parses_back_to_its_step() {
    for step in Step::ALL {
        assert_eq!(step.name().parse::<Step>(), Ok(step));
    }
}

/// A word that is not a step is refused and the message quotes it.
#[test]
fn an_unknown_step_word_is_refused_with_its_text_quoted() {
    let err = "Prepare".parse::<Step>().unwrap_err();
    assert_eq!(err, UnknownStep("Prepare".to_owned()));
    assert_eq!(err.to_string(), r#"unknown step "Prepare""#);
    assert_eq!(
        "".parse::<Step>().unwrap_err().to_string(),
        r#"unknown step """#
    );
}

/// Absolute paths with plain components are plain, including the root.
#[test]
fn absolute_paths_without_oddities_are_plain() {
    assert!(is_plain(Path::new("/")));
    assert!(is_plain(Path::new("/a")));
    assert!(is_plain(Path::new("/var/lib/js/state")));
}

/// Relative paths are not plain.
#[test]
fn relative_paths_are_not_plain() {
    assert!(!is_plain(Path::new("")));
    assert!(!is_plain(Path::new("a/b")));
    assert!(!is_plain(Path::new("./a")));
    assert!(!is_plain(Path::new(".")));
    assert!(!is_plain(Path::new("..")));
}

/// Dot, dot-dot, doubled and trailing slashes make an absolute path not plain.
#[test]
fn dots_doubled_and_trailing_slashes_are_not_plain() {
    assert!(!is_plain(Path::new("/a/../b")));
    assert!(!is_plain(Path::new("/..")));
    assert!(!is_plain(Path::new("/a/./b")));
    assert!(!is_plain(Path::new("/.")));
    assert!(!is_plain(Path::new("//")));
    assert!(!is_plain(Path::new("//a")));
    assert!(!is_plain(Path::new("/a//b")));
    assert!(!is_plain(Path::new("/a/")));
}

/// A runner that does not override `run_streams` refuses, naming the program.
#[test]
fn the_default_run_streams_reports_that_it_reads_no_standard_error() {
    let err = Minimal
        .run_streams(
            "systemctl",
            &["show"],
            &[("K", "V")],
            10,
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert_eq!(err, "systemctl: this runner reads no standard error");
    assert_eq!(Minimal.run("x", &[]), Ok(String::new()));
    assert_eq!(Minimal.run_in("x", &[], &[]), Ok(String::new()));
}

/// Streams start empty and nothing dropped.
#[test]
fn default_streams_are_empty_and_nothing_was_dropped() {
    let streams = Streams::default();
    assert!(streams.stdout.is_empty());
    assert!(streams.stderr.is_empty());
    assert!(!streams.dropped);
}

/// Every wait bound is positive.
#[test]
fn every_timing_bound_is_positive() {
    let bounds = [
        POLL_SECS,
        PREPARE_MARGIN_SECS,
        START_WAIT_SECS,
        CUTOVER_MARGIN_SECS,
        VERIFY_MARGIN_SECS,
        ROLLBACK_MARGIN_SECS,
    ];
    assert!(bounds.iter().all(|&secs| secs > 0));
}
