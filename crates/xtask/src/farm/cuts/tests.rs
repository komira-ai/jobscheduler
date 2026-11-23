use super::*;

/// A build of `//:ci` whose one failure was a cut, as buck2 printed it on the farm (an excerpt of a
/// failed run of farm.yml, digests and the build id zeroed): the cut once as it happened and once
/// in the list of failed actions at the end.
const BUILD_CUT: &str = include_str!("build_cut.log");
/// A build whose many failures were all cuts (an excerpt: three reports and two of the list).
const BUILD_CUTS: &str = include_str!("build_cuts.log");
/// A build whose cuts came with buck2's own warnings that it retries its calls, which say `timed
/// out` (an excerpt of a recorded run: two reports, five warnings, a third report and the list).
const BUILD_CUT_WITH_WARNINGS: &str = include_str!("build_cut_with_warnings.log");
/// A build whose cuts sat beside a real failure: two cut reports of one recorded run and the
/// failed test of another (no recorded run held both), with the list of failures of the cut run.
const BUILD_CUT_AND_FAILED_TEST: &str = include_str!("build_cut_and_failed_test.log");
/// A build whose failures were the table check and a test of the coverage crate.
const TEST_FAILED: &str = include_str!("test_failed.log");

/// One cut action's report, as buck2 prints it.
const CUT: &str = "\
[t] Action failed: jobscheduler//crates/a:a_lint (rustc a_lint)
[t] Internal error (stage: remote_call_error): Remote Execution Error on Execute with digest 0:1 for ReSession S
[t] Error: (Error was returned on the stream by RE: RE channel error: code: 'Unknown error', message: \"h2 protocol error: error reading a body from connection\", source: hyper::Error(Body, Error { kind: Io(Custom { kind: BrokenPipe, error: \"stream closed because of a broken pipe\" }) }))
[t] Remote action digest: '0:1'
[t] Stdout: <empty>
[t] Stderr: <empty>
";

/// The end of a failed build.
const FAILED: &str = "BUILD FAILED\nFailed to build 'jobscheduler//crates/a:a_lint'\n";

#[test]
fn a_recorded_build_whose_every_failure_is_a_cut_is_cut_counting_each_report() {
    // Catches: a real cut on the farm not read as one (the stamp or the reason's words not
    // matched), or a report counted once when buck2 lists it twice.
    assert_eq!(only_cuts(BUILD_CUT), Some(2));
    assert_eq!(only_cuts(BUILD_CUTS), Some(5));
    assert_eq!(only_cuts(BUILD_CUT_WITH_WARNINGS), Some(4));
}

#[test]
fn a_recorded_build_with_a_failed_action_beside_its_cuts_is_final() {
    // Catches: the cuts of a build excusing the action that failed on its own beside them (a
    // test that returned non-zero exit code 101), or a cancelled download standing alone read
    // as a cut.
    assert_eq!(only_cuts(BUILD_CUT_AND_FAILED_TEST), None);
    let alone = "[t] Error: (Failed to make BatchReadBlobs request: code: 'The operation was \
                 cancelled', message: \"operation was canceled\")\n";
    assert_eq!(only_cuts(&format!("{CUT}{alone}{FAILED}")), None);
}

#[test]
fn buck2s_own_warnings_are_passed_over_and_its_errors_are_final() {
    // Catches: buck2's warning that it retries a call (`keep-alive timed out`) taken for a real
    // failure, so a build cut on the farm is never run again; buck2's own ERROR line taken for a
    // warning; and a line that only looks like one (no stamp, no level, a stamp without `Z`)
    // passed over.
    let warn = "[t] \u{1b}[2m2026-11-22T00:18:21.391037Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \
                \u{1b}[2mremote_execution::client\u{1b}[0m: Transient error (attempt 1/5), retrying in \
                114ms: http2 error: keep-alive timed out: operation timed out\n";
    assert_eq!(only_cuts(&format!("{CUT}{warn}{FAILED}")), Some(1));
    let plain = "2026-11-22T00:18:21.391037Z  INFO buck2: Error panicked timed out\n";
    assert_eq!(only_cuts(&format!("{plain}{CUT}{FAILED}")), Some(1));
    let error = "[t] \u{1b}[2m2026-11-22T00:18:21.391037Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m buck2: \
                 lost the daemon\n";
    assert_eq!(only_cuts(&format!("{CUT}{error}{FAILED}")), None);
    for line in [
        "WARN timed out",
        "2026-11-22T00:18:21 WARN timed out",
        "2026-11-22T00:18:21Z NOTE timed out",
        "\u{1b}[2mx\u{1b}[0m WARN timed out",
        "2026-11-22T00:18:21Z timed out",
        "Z WARN timed out",
    ] {
        assert_eq!(
            only_cuts(&format!("{CUT}{line}\n{FAILED}")),
            None,
            "{line:?}"
        );
    }
    // An escape that is not a colour code is kept, so the line is not buck2's log.
    assert_eq!(log_level("\u{1b}]2026-11-22T00:18:21Z WARN x"), None);
    assert_eq!(log_level("\u{1b}[2x2026-11-22T00:18:21Z WARN x"), None);
    assert_eq!(log_level("\u{1b}[2xm2026-11-22T00:18:21Z WARN x"), None);
    assert_eq!(
        log_level("\u{1b}[1;33m2026-11-22T00:18:21Z\u{1b}[0m WARN x"),
        Some("WARN")
    );
}

#[test]
fn a_recorded_test_failure_is_final() {
    // Catches: a failing test of the change run again, as if the farm had failed it.
    assert_eq!(only_cuts(TEST_FAILED), None);
}

#[test]
fn a_cut_with_no_report_around_it_and_one_printed_again_at_the_end_count() {
    // Catches: a cut while inputs were uploaded (no action report) not read as one, or the
    // command's own error, printed again at the end, read as another failure.
    let upload =
        "Error: (RE channel error: code: 'Unknown error', message: \"h2 protocol error\")\n";
    assert_eq!(only_cuts(&format!("{upload}BUILD FAILED\n")), Some(1));
    let again = "Command failed: \n  Error: (Error was returned on the stream by RE: RE channel \
                 error: code: 'Unknown error')\n";
    assert_eq!(only_cuts(&format!("{CUT}{CUT}{again}")), Some(3));
}

#[test]
fn a_report_whose_reason_is_not_a_cut_is_final() {
    // Catches: an action that failed on its own (a compile error, a test, a timeout, an internal
    // error that names no remote stream) run again because a cut was printed after it.
    for reason in [
        "Remote command returned non-zero exit code 1",
        "Command timed out after 600s",
        "Internal error (stage: local_execute): could not spawn",
        "Internal error (stage: remote_call_error): something else broke",
    ] {
        let report =
            format!("[t] Action failed: jobscheduler//crates/a:a (rustc a)\n[t] {reason}\n");
        assert_eq!(
            only_cuts(&format!("{report}{CUT}{FAILED}")),
            None,
            "{reason}"
        );
    }
}

#[test]
fn a_remote_error_with_no_cut_line_under_it_is_final() {
    // Catches: a remote-execution error of another kind (the line after the reason is not the
    // channel's error) read as a cut, and a report cut off at its end.
    let head = "[t] Action failed: jobscheduler//crates/a:a (rustc a)\n[t] Internal error (stage: \
                remote_call_error): Remote Execution Error on Execute for ReSession S\n";
    let other = "[t] Error: (Error was returned on the stream by RE: code: 'Unavailable')\n";
    assert_eq!(only_cuts(&format!("{head}{other}{FAILED}")), None);
    assert_eq!(only_cuts(head), None);
    assert_eq!(
        only_cuts("[t] Action failed: jobscheduler//crates/a:a (rustc a)\n"),
        None
    );
    // A second report starting before the first's reason came.
    assert_eq!(
        only_cuts(&format!(
            "[t] Action failed: jobscheduler//crates/a:a (rustc a)\n{CUT}"
        )),
        None
    );
}

#[test]
fn any_other_failure_in_the_output_is_final_though_every_report_is_a_cut() {
    // Catches: an analysis error, a compiler error, a panic or a failed command anywhere beside
    // the cuts passed over by the retry.
    for line in [
        "Error: evaluating build file `jobscheduler//crates/a:BUCK`",
        "Error running analysis for `jobscheduler//:ci`",
        "error: expected one of `,` or `)`",
        "error[E0425]: cannot find value `x` in this scope",
        "thread 'main' panicked at src/lib.rs:1:1",
        "Local command returned non-zero exit code 2",
        "Tests timed out",
    ] {
        assert_eq!(
            only_cuts(&format!("{CUT}[t] {line}\n{FAILED}")),
            None,
            "{line}"
        );
    }
}

#[test]
fn the_cut_words_anywhere_but_at_the_start_of_buck2s_error_are_not_a_cut() {
    // Catches: a test or a compiler that only prints the phrase taken for a cut connection.
    for line in [
        "note: RE channel error: code: 'Unknown error'",
        "[t] Stderr: Error was returned: RE channel error: code: 'Unknown error'",
        "  > RE channel error: code: 'Unknown error'",
    ] {
        assert_eq!(only_cuts(&format!("{line}\n{FAILED}")), None, "{line}");
    }
}

#[test]
fn a_failed_build_with_no_cut_is_not_cut() {
    // Catches: an empty log, or a failure buck2 printed nothing about, read as a cut.
    assert_eq!(only_cuts(""), None);
    assert_eq!(only_cuts(FAILED), None);
}

#[test]
fn the_stamp_indent_and_line_ends_are_taken_off_before_the_line_is_read() {
    // Catches: a report read with its stamp, its indent or a CRLF line end, and so not as one.
    let crlf = CUT.replace('\n', "\r\n");
    assert_eq!(only_cuts(&crlf), Some(1));
    assert_eq!(
        bare("[2026-11-21T07:48:10.059+00:00]   Error: x"),
        "Error: x"
    );
    assert_eq!(bare("  Error: x\r"), "Error: x");
    assert_eq!(bare("[no stamp end"), "[no stamp end");
}
