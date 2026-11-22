//! Which processes the sweep ends, and its tally.

use super::{Found, JobIds, Proc, Tally, targets};

const JOB: JobIds = JobIds {
    uid: 680,
    asid: Some(7),
    own_pid: 50,
};

fn p(pid: i32, uid: u32, asid: Option<i32>) -> Proc {
    Proc { pid, uid, asid }
}

#[test]
fn the_jobs_session_and_the_jobs_user_are_ended_and_nothing_else() {
    let procs = [
        p(100, 680, Some(7)), // the job's, in its session
        p(101, 0, Some(7)),   // in its session as another user: still the job's
        p(102, 680, Some(9)), // the job's user, another session
        p(103, 680, None),    // the job's user, session unread
        p(104, 501, Some(9)), // someone else
        p(105, 501, None),    // someone else, session unread
    ];
    assert_eq!(
        targets(&procs, &JOB),
        [
            (100, Found::Session),
            (101, Found::Session),
            (102, Found::UserOnly),
            (103, Found::UserOnly),
        ]
    );
}

#[test]
fn launchd_the_kernel_and_the_sweeper_are_never_targets() {
    let procs = [p(0, 680, Some(7)), p(1, 680, Some(7)), p(50, 680, Some(7))];
    assert!(targets(&procs, &JOB).is_empty());
}

#[test]
fn with_no_session_of_its_own_the_job_is_found_by_its_user_alone() {
    let job = JobIds { asid: None, ..JOB };
    let procs = [p(100, 680, None), p(101, 501, None), p(102, 501, Some(7))];
    assert_eq!(targets(&procs, &job), [(100, Found::UserOnly)]);
}

#[test]
fn the_tally_counts_each_pid_once_and_ends_clean_on_an_empty_round() {
    let mut tally = Tally::default();
    assert!(tally.round(&[(100, Found::Session), (102, Found::UserOnly)]));
    assert!(!tally.clean);
    assert!(tally.round(&[(100, Found::Session), (103, Found::Session)]));
    assert!(!tally.round(&[]));
    assert_eq!(
        tally,
        Tally {
            by_session: vec![100, 103],
            by_user_only: vec![102],
            rounds: 3,
            clean: true,
            error: None,
        }
    );
}

#[test]
fn rules_serialize_in_kebab_case() {
    assert_eq!(
        serde_json::to_string(&[Found::Session, Found::UserOnly]).expect("json"),
        r#"["session","user-only"]"#
    );
}

#[test]
fn a_sweep_that_cannot_read_the_process_table_is_not_clean() {
    let mut tally = Tally::default();
    assert!(!tally.round(&[]));
    tally.failed("no listing".to_owned());
    assert!(!tally.clean);
    assert_eq!(tally.error.as_deref(), Some("no listing"));
}
