//! More tests of `image prime`: the failures it must name, and the frames it must list or
//! refuse. They share the fixtures of `prime_tests`, of which this is a child module.

use super::*;

/// Catches a failed step whose reason is lost: the reason names the signal that ended it,
/// or says only that it failed when the guest gave neither an exit code nor a signal.
#[test]
fn a_failed_step_is_named_by_its_signal_or_by_failing_alone() {
    let work = work_with_base("prime-failed-how");
    let sys = FakeSystem::new();
    let killed = |index: usize, id: &str, signal: Option<i32>| {
        let (name, body) = step(index, id, Status::Failed, None);
        let mut f: StepFrame = serde_json::from_str(&body).expect("frame");
        f.signal = signal;
        (name, serde_json::to_string(&f).expect("json"))
    };
    let frames = vec![
        killed(0, "add-user", Some(9)),
        killed(1, "home", None),
        summary(false, false, 0),
    ];
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    let reason = r.reason.expect("reason");
    assert!(reason.contains("add-user (signal 9)"), "{reason}");
    assert!(reason.contains("home (failed)"), "{reason}");
}

/// Catches a summary that is not ok, with no failed step and no note, producing an empty
/// reason: it says to see the summary.
#[test]
fn a_summary_that_is_not_ok_with_no_step_and_no_note_points_at_the_summary() {
    let work = work_with_base("prime-notok-nonote");
    let sys = FakeSystem::new();
    let (name, body) = summary(false, false, n_steps());
    let mut s: MkUserSummary = serde_json::from_str(&body).expect("summary");
    s.note = None;
    let mut frames = all_ok();
    frames.pop();
    frames.push((name, serde_json::to_string(&s).expect("json")));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
    assert!(
        r.reason.expect("reason").contains("see the summary"),
        "the reason points at the report's summary"
    );
}

/// Catches a frame the host must not read (one over the size cap) being read, or stopping
/// the run: it is listed as unreadable with the reason, and the other frames still count.
#[test]
fn a_frame_over_the_size_cap_is_listed_as_unreadable() {
    let work = work_with_base("prime-bigframe");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    let big = "x".repeat((guest::MAX_RESULT_BYTES + 1) as usize);
    frames.push((step_frame_name(21, "big"), big));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let bad = r.details["unreadable_frames"].as_array().expect("list");
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(
        bad[0]["error"]
            .as_str()
            .expect("why")
            .contains("larger than")
    );
    assert_eq!(r.details["steps_received"], n_steps());
}

/// Catches a guest binary that cannot be copied into the share going unreported: it is an
/// error that names the copy, and nothing is started.
#[test]
fn a_guest_binary_that_cannot_be_copied_is_an_error_and_starts_nothing() {
    let work = work_with_base("prime-nocopy");
    let sys = FakeSystem::new();
    let c = prime_ctx(&sys, &work);
    std::fs::remove_file(c.guest_binary.as_ref().expect("bin")).expect("rm");
    let r = prime(&c, LONG);
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("into the share"));
    assert!(sys.starts.borrow().is_empty());
}

/// Catches a priming mark that cannot be written being reported as taken: without the mark
/// the sink would answer the guest `no`, and the run would wait for nothing.
#[test]
fn a_mark_that_cannot_be_written_is_an_error() {
    let dir = tmp("prime-nomark");
    let share = guest::Share {
        dir: dir.join("share"),
        tagged: dir.join("tagged"),
        received: dir.join("absent"),
    };
    let e = crate::prime::request_prime(&share).expect_err("no such directory");
    assert!(e.starts_with("write "), "{e}");
}

/// Catches a helper that cannot be started leaving the share directories behind (the next
/// run removes them, but a failure should not litter the base), or being reported as a
/// timeout or a success.
#[test]
fn a_helper_that_cannot_start_is_an_error_and_leaves_no_share() {
    let work = work_with_base("prime-nostart");
    let sys = FakeSystem::new();
    *sys.start_error.borrow_mut() = Some("no helper".to_owned());
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("no helper"));
    assert!(!work.join("base/prime-share").exists());
    assert!(!work.join("base/prime-tagged").exists());
    // The priming mark is not left behind by a run that never started.
    assert!(!results(&work).join(crate::sink::PRIME_MARKER).exists());
}
