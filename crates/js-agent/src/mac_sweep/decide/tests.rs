//! Which processes are targets, and how the tally records rounds.

use super::{Found, JobIds, Proc, Swept, Tally, Target, targets};

const JOB: JobIds = JobIds {
    uid: 680,
    asid: Some(7),
    own_pid: 50,
};

/// A process `pid` of effective and real uid `uid`, in session `asid`.
fn p(pid: i32, uid: u32, asid: Option<i32>) -> Proc {
    Proc {
        pid,
        uid,
        ruid: uid,
        asid,
        name: format!("p{pid}"),
    }
}

/// The target `pid`, named as [`p`] names it, found by `why`.
fn t(pid: i32, why: Found) -> Target {
    Target {
        pid,
        name: format!("p{pid}"),
        why,
    }
}

/// The swept process `pid`, named as [`p`] names it.
fn s(pid: i32) -> Swept {
    Swept {
        pid,
        name: format!("p{pid}"),
    }
}

/// The job's session finds any uid in it, the slot's uid (effective or real) finds the
/// rest, and nothing else is a target: another uid in another session or with its session
/// unread. Catches a rule that takes every process with no session, or a session rule
/// that also asks for the slot's uid.
#[test]
fn the_jobs_session_and_the_slots_uid_are_targets_and_nothing_else() {
    let set_uid = Proc {
        ruid: 680,
        ..p(106, 0, Some(9))
    };
    let procs = [
        p(100, 680, Some(7)), // the job's, in its session
        p(101, 0, Some(7)),   // in its session as another user: still the job's
        p(102, 680, Some(9)), // the slot's uid, another session
        p(103, 680, None),    // the slot's uid, session unread
        p(104, 501, Some(9)), // someone else
        p(105, 501, None),    // someone else, session unread
        set_uid,              // the slot's by its real uid
    ];
    assert_eq!(
        targets(&procs, &JOB),
        [
            t(100, Found::Session),
            t(101, Found::Session),
            t(102, Found::UserOnly),
            t(103, Found::UserOnly),
            t(106, Found::UserOnly),
        ]
    );
}

/// pid 0, pid 1 and the sweeper are never targets, in the job's session and of its uid.
/// Catches a missing guard on the kernel, launchd or the agent itself.
#[test]
fn the_kernel_launchd_and_the_sweeper_are_never_targets() {
    let procs = [p(0, 680, Some(7)), p(1, 680, Some(7)), p(50, 680, Some(7))];
    assert!(targets(&procs, &JOB).is_empty());
}

/// With no session of its own, the job is found by the slot's uid alone, and a process
/// with no session read is not taken for the job's. Catches `None == None` read as a
/// match of sessions.
#[test]
fn with_no_session_the_job_is_found_by_the_slots_uid_alone() {
    let job = JobIds { asid: None, ..JOB };
    let procs = [p(100, 680, None), p(101, 501, None), p(102, 501, Some(7))];
    assert_eq!(targets(&procs, &job), [t(100, Found::UserOnly)]);
}

/// The tally names each pid once under its rule however many rounds found it, and is
/// clean only after a round that found nothing. Catches a pid listed once per round, and
/// clean set while processes were still found.
#[test]
fn the_tally_names_each_pid_once_and_is_clean_after_an_empty_round() {
    let mut tally = Tally::default();
    assert!(tally.round(&[t(100, Found::Session), t(102, Found::UserOnly)]));
    assert!(!tally.clean);
    assert!(tally.round(&[t(100, Found::Session), t(103, Found::Session)]));
    assert!(!tally.round(&[]));
    assert_eq!(
        tally,
        Tally {
            asid: None,
            by_session: vec![s(100), s(103)],
            by_user_only: vec![s(102)],
            remaining: Vec::new(),
            rounds: 3,
            clean: true,
            error: None,
        }
    );
}

/// The rules are spelled in kebab case wherever they are written out. Catches a renamed
/// rule in the logs and reports.
#[test]
fn rules_serialise_in_kebab_case() {
    assert_eq!(
        serde_json::to_string(&[Found::Session, Found::UserOnly]).expect("json"),
        r#"["session","user-only"]"#
    );
}

/// A failure after a clean round leaves the tally not clean, with the reason. Catches a
/// failure that a clean earlier round hides.
#[test]
fn a_failure_after_an_empty_round_is_not_clean() {
    let mut tally = Tally::default();
    assert!(!tally.round(&[]));
    tally.failed("no listing".to_owned());
    assert!(!tally.clean);
    assert_eq!(tally.error.as_deref(), Some("no listing"));
}
