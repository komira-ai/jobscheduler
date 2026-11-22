//! The report: its status and its JSON.

use super::{Ended, Leftovers, Outputs, Report};
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
    }
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
