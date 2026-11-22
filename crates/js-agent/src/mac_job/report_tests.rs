//! The report: its status and its JSON.

use super::{AccountUse, Ended, Leftovers, Outputs, PutUse, Report, TAIL_MAX, TreeUse, tail};
use crate::mac_job::status;
use crate::mac_job::sweep::Tally;

fn report() -> Report {
    Report {
        id: "j".to_owned(),
        user: "u".to_owned(),
        uid: 680,
        asid: Some(7),
        folder: "/r/j".to_owned(),
        profile: "(version 1)".to_owned(),
        ended: Ended::Exited(0),
        run_ms: 5,
        sweep: Tally {
            clean: true,
            ..Tally::default()
        },
        outputs: Outputs::default(),
        leftovers: Leftovers::default(),
        put_dirs: Vec::new(),
        put: None,
        trees: Vec::new(),
        account: None,
        spawn: "ordinary".to_owned(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
    }
}

fn lent(retired: Result<String, String>) -> Option<AccountUse> {
    Some(AccountUse {
        name: "kbf-acct-1".to_owned(),
        made_now: false,
        retired,
    })
}

#[test]
fn a_held_account_is_something_left_and_a_retired_one_is_not() {
    let ok = Report {
        account: lent(Ok("gone".to_owned())),
        ..report()
    };
    assert_eq!(ok.status(), status::OK);
    let held = Report {
        account: lent(Err("held at delete".to_owned())),
        ..report()
    };
    assert_eq!(held.status(), status::LEFT_BEHIND);
    let json = serde_json::to_string(&held.account).expect("json");
    assert_eq!(
        json,
        r#"{"name":"kbf-acct-1","made_now":false,"retired":{"Err":"held at delete"}}"#
    );
}

#[test]
fn a_stream_tail_keeps_the_last_bytes_only() {
    assert_eq!(tail(b"short"), "short");
    let long: Vec<u8> = (0..TAIL_MAX + 10).map(|i| b'a' + (i % 26) as u8).collect();
    let cut = tail(&long);
    assert_eq!(cut.len(), TAIL_MAX);
    assert_eq!(cut.as_bytes(), &long[10..]);
    // A multi-byte character cut at the start reads as one replacement character.
    let mut text = vec![0xC3, 0xA9];
    text.extend(std::iter::repeat_n(b'x', TAIL_MAX - 1));
    assert_eq!(tail(&text), format!("\u{FFFD}{}", "x".repeat(TAIL_MAX - 1)));
}

#[test]
fn a_clean_run_that_exited_zero_is_ok() {
    assert!(Leftovers::default().is_clean());
    assert_eq!(report().status(), status::OK);
}

#[test]
fn a_failed_job_or_a_missing_output_is_a_job_failure() {
    for ended in [Ended::Exited(1), Ended::Signalled(9), Ended::WallLimit] {
        assert_eq!(Report { ended, ..report() }.status(), status::JOB_FAILED);
    }
    let outputs = Outputs {
        copied: Vec::new(),
        missing: vec!["out: gone".to_owned()],
    };
    assert_eq!(
        Report {
            outputs,
            ..report()
        }
        .status(),
        status::JOB_FAILED
    );
}

#[test]
fn anything_left_behind_wins_over_how_the_job_ended() {
    let lefts = [
        Leftovers {
            processes: vec![1],
            ..Leftovers::default()
        },
        Leftovers {
            files: vec!["/tmp/x".to_owned()],
            ..Leftovers::default()
        },
        Leftovers {
            folder: Some("busy".to_owned()),
            ..Leftovers::default()
        },
    ];
    for leftovers in lefts {
        assert!(!leftovers.is_clean());
        let r = Report {
            leftovers,
            ended: Ended::Exited(1),
            ..report()
        };
        assert_eq!(r.status(), status::LEFT_BEHIND);
    }
    let unswept = Report {
        sweep: Tally::default(),
        ..report()
    };
    assert_eq!(unswept.status(), status::LEFT_BEHIND);
}

#[test]
fn the_json_names_how_the_job_ended() {
    let json = |ended: Ended| serde_json::to_string(&ended).expect("json");
    assert_eq!(json(Ended::Exited(3)), r#"{"how":"exited","value":3}"#);
    assert_eq!(
        json(Ended::Signalled(9)),
        r#"{"how":"signalled","value":9}"#
    );
    assert_eq!(json(Ended::WallLimit), r#"{"how":"wall-limit"}"#);
    let text = serde_json::to_string(&report()).expect("json");
    assert!(
        text.starts_with(r#"{"id":"j","user":"u","uid":680,"asid":7,"folder":"/r/j""#),
        "{text}"
    );
}

#[test]
fn a_tree_reports_its_reference_place_path_and_release() {
    let tree = TreeUse {
        reference: "cas/l/ab".to_owned(),
        at: "src".to_owned(),
        path: "/fs/mount-provider/a/b/0".to_owned(),
        mount_ms: 12,
        released: Ok(()),
    };
    assert_eq!(
        serde_json::to_string(&tree).expect("json"),
        r#"{"ref":"cas/l/ab","at":"src","path":"/fs/mount-provider/a/b/0","mount_ms":12,"released":{"Ok":null}}"#
    );
}

#[test]
fn a_put_directory_that_cannot_be_read_is_a_job_failure_and_one_with_entries_is_not() {
    let dir = |error: Option<&str>| PutUse {
        path: "/put/cas/b/r/n".to_owned(),
        at: ".kbf-out".to_owned(),
        left: vec!["pack".to_owned()],
        error: error.map(str::to_owned),
    };
    assert_eq!(
        Report {
            put_dirs: vec![dir(None)],
            ..report()
        }
        .status(),
        status::OK
    );
    let failed = Report {
        put_dirs: vec![dir(Some("gone"))],
        ..report()
    };
    assert_eq!(failed.status(), status::JOB_FAILED);
}

/// Asserts that an agent's commit that failed is a job failure, one that was made is not, and
/// that the key and the digest it reports are written into the report.
#[test]
fn a_failed_commit_of_the_outputs_is_a_job_failure_and_a_made_one_is_reported() {
    let committed = crate::mac_job::put::Committed {
        key: "cas/b/job/0011/pack".to_owned(),
        sha256: "ab".repeat(32),
        bytes: 10_240,
    };
    let made = Report {
        put: Some(Ok(committed)),
        ..report()
    };
    assert_eq!(made.status(), status::OK);
    let text = serde_json::to_string(&made).unwrap();
    assert!(
        text.contains(r#""put":{"Ok":{"key":"cas/b/job/0011/pack","sha256":"abab"#),
        "{text}"
    );
    let failed = Report {
        put: Some(Err("outputs missing: out: gone".to_owned())),
        ..report()
    };
    assert_eq!(failed.status(), status::JOB_FAILED);
}
