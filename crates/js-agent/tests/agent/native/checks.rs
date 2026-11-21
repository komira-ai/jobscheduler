//! The post-job check of a native job through the lifecycle: the helper's leak scan
//! after each run, against the scan of the idle host. The helper is the fake, which
//! answers each `scan` as the test scripts it.

use js_mac_session::helper::NO_BASELINE;
use js_mac_session::proto::Reply;

use super::lifecycle::{Native, job};
use crate::lifecycle::{cleanup, names};

/// A login item of the pool uid 601, as the helper's scan prints one.
const LOGIN_ITEM: &str = "login-items \"loginitems.601.plist\" file 601:20 600 sha256:ab";

fn scanned(added: &[&str]) -> Reply {
    Reply::Scanned {
        clean: added.is_empty(),
        added: added.iter().map(|item| (*item).to_owned()).collect(),
        removed: Vec::new(),
        errors: Vec::new(),
    }
}

fn no_baseline() -> Reply {
    Reply::Refused {
        reason: NO_BASELINE.to_owned(),
    }
}

/// Catches: a native job that left a login item, which the reset of its pool user does
/// not remove, ending with its slot freed and the next job started on the host (the
/// check that passed every run), so that the next run of the same uid inherits it.
#[test]
fn a_login_item_a_native_job_left_taints_the_host_and_the_next_job_is_refused() {
    let mut h = Native::new("checks-residue");
    h.fake
        .scan_answers(vec![scanned(&[]), scanned(&[LOGIN_ITEM])]);
    assert_eq!(h.start(job("r1", "/bin/echo")), ["started r1"]);
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.taint_detail().is_some());
    assert_eq!(
        names(&events),
        [
            "result r1",
            "container removed r1",
            "scratch removed r1",
            "tainted r1 residue",
            "finding r1 scan-residue",
        ]
    );
    assert_eq!(
        h.life.taint_detail(),
        Some(format!(
            "the leak scan found what a run left: + {LOGIN_ITEM}"
        ))
    );
    assert_eq!(
        h.fake.scans(),
        2,
        "one when the host was idle, one after the run"
    );
    assert_eq!(h.start(job("r2", "/bin/echo")), ["refused r2 host-tainted"]);
}

/// Catches: a node whose leak-scan baseline was never recorded (every node until one is)
/// tainted by its first job, or the missing baseline noted after every job.
#[test]
fn with_no_baseline_jobs_run_and_the_missing_baseline_is_noted_once() {
    let mut h = Native::new("checks-no-baseline");
    h.fake.scan_answers(vec![no_baseline(); 4]);
    let mut want = vec!["result r1".to_owned()];
    want.extend(cleanup("r1"));
    want.insert(3, "finding r1 scan-no-baseline".to_owned());
    assert_eq!(h.start(job("r1", "/bin/echo")), ["started r1"]);
    h.fake.exit("7.0", 0);
    assert_eq!(names(&h.until(|life| life.busy_slots().is_empty())), want);
    let mut want = vec!["result r2".to_owned()];
    want.extend(cleanup("r2"));
    assert_eq!(h.start(job("r2", "/bin/echo")), ["started r2"]);
    h.fake.exit("7.1", 0);
    assert_eq!(names(&h.until(|life| life.busy_slots().is_empty())), want);
    assert_eq!(h.fake.scans(), 4);
}

/// Catches: a scan the helper could not answer passed as clean: the host is tainted as
/// unchecked, with the helper's reason.
#[test]
fn a_scan_the_helper_refuses_leaves_the_host_unchecked() {
    let mut h = Native::new("checks-refused");
    h.fake.scan_answers(vec![
        scanned(&[]),
        Reply::Refused {
            reason: "the disk is full".to_owned(),
        },
    ]);
    h.start(job("r1", "/bin/echo"));
    h.fake.exit("7.0", 0);
    let events = h.until(|life| life.taint_detail().is_some());
    assert_eq!(
        names(&events)[3..],
        ["tainted r1 unchecked", "finding r1 scan-failed"]
    );
    assert_eq!(
        h.life.taint_detail(),
        Some(
            "the check could not run: the leak scan could not run: kbf-mac-session refused: \
             the disk is full"
                .to_owned()
        )
    );
}
