//! The public helpers of the simulator probe that the host side calls: what the unit
//! tests beside the code leave to a caller. They are unit tests and not tests of a
//! `tests/` file because the coverage ratchet counts a function that an integration test
//! calls only in part as missed lines, and these helpers are called in whole by the unit
//! tests beside them.

use crate::sim::max_frames;
use crate::sim_choose::choose_device_type;
use crate::sim_xctest::{parse_summary, remove_no_follow};
use crate::test_dir::TestDir;

/// Catches `max_frames` ignoring the runtimes: the host reads at most this many frames,
/// so a count that does not grow with the plan would cut a run with many runtimes short.
#[test]
fn each_runtime_adds_frames_to_the_most_a_run_sends() {
    assert!(max_frames(2) > max_frames(1));
    assert!(max_frames(1) > max_frames(0));
}

/// Catches a device-type listing that is not JSON being taken for one with no iPhone, or
/// accepted: the probe would then name the wrong reason to the host.
#[test]
fn a_device_type_listing_that_is_not_json_is_refused_as_such() {
    let err = choose_device_type("not json").expect_err("refused");
    assert!(err.starts_with("not JSON: "), "{err}");
}

/// Catches a summary that lacks a count being accepted with zeros, which would report a
/// run as passed when the tool printed something else; each of the three counts is
/// required.
#[test]
fn a_summary_without_a_count_is_refused() {
    for missing in ["passedTests", "failedTests", "skippedTests"] {
        let mut v = serde_json::json!({"passedTests": 1, "failedTests": 0, "skippedTests": 0});
        v.as_object_mut().expect("object").remove(missing);
        let err = parse_summary(&v.to_string(), false).expect_err("refused");
        assert!(err.contains(missing), "{missing}: {err}");
    }
}

/// Catches a removal that reports success for a path it could not examine (here a path
/// below a regular file), which would leave a stale result bundle for the next run to
/// read, and one that fails on a path that is not there.
#[test]
fn remove_no_follow_reports_what_it_cannot_examine_and_ignores_what_is_absent() {
    let dir = TestDir::under(std::env::temp_dir(), "js-guest-remove");
    let file = dir.join("file");
    std::fs::write(&file, b"x").expect("file");
    assert!(remove_no_follow(&file.join("below")).is_err());
    assert!(remove_no_follow(&dir.join("absent")).is_ok());
    assert!(remove_no_follow(&file).is_ok());
    assert!(!file.exists());
}

/// Catches a read error reported as an empty or partial file: a regular file that fails
/// to read must come back as an error naming the path, or the probe would send the host
/// a screenshot of nothing. `/proc/self/mem` is a regular file whose first byte is
/// unmapped, so reading it fails.
#[cfg(target_os = "linux")]
#[test]
fn read_regular_reports_a_file_that_fails_to_read() {
    let err = crate::sim_run::read_regular(std::path::Path::new("/proc/self/mem"), 16)
        .expect_err("fails");
    assert!(err.starts_with("read /proc/self/mem: "), "{err}");
}
