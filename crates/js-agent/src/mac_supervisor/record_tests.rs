//! The records: their JSON, how half a record reads, the statuses, and the one write on a held
//! file.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use js_testkit::dir::TestDir;

use super::{
    EXIT, Ended, ExitRecord, READY, Ready, Records, Step, from_status, parse_exit, parse_ready,
    status_of, whole,
};
use crate::mac_supervisor::status;

/// Every step, with the name it is written as.
const STEPS: [(Step, &str); 7] = [
    (Step::Spec, "spec"),
    (Step::Records, "records"),
    (Step::AuditSession, "audit-session"),
    (Step::Drop, "drop"),
    (Step::Limits, "limits"),
    (Step::Profile, "profile"),
    (Step::Spawn, "spawn"),
];

/// An exit status that exited with `code`.
fn exited(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

/// An exit status that `signal` ended.
fn killed(signal: i32) -> ExitStatus {
    ExitStatus::from_raw(signal)
}

/// Every way a start can end, as an exit record of a job with pid 4321 in session 77.
fn every_end() -> Vec<ExitRecord> {
    let mut ends = vec![Ended::Exited(0), Ended::Exited(125), Ended::Signalled(9)];
    ends.extend(STEPS.iter().map(|(step, _)| Ended::SetupFailed {
        step: *step,
        why: format!("the {step} failed"),
    }));
    ends.into_iter()
        .map(|ended| ExitRecord {
            ended,
            job_pid: Some(4321),
            asid: Some(77),
        })
        .collect()
}

/// Every record round-trips through its text, the ready record with and without a session;
/// the text names the end and the step in kebab case; and empty text, every prefix of a record
/// short of the whole and `{}` read as not written. Catches the agent treating a half-written
/// record as an end.
#[test]
fn records_round_trip_and_partial_text_reads_as_not_written() {
    for record in every_end() {
        let text = serde_json::to_string(&record).unwrap();
        assert_eq!(parse_exit(&text), Some(record.clone()), "{text}");
        for cut in 0..text.len() {
            assert_eq!(parse_exit(&text[..cut]), None, "{}", &text[..cut]);
        }
    }
    for ready in [
        Ready {
            job_pid: 4321,
            asid: Some(77),
        },
        Ready {
            job_pid: 4321,
            asid: None,
        },
    ] {
        let text = serde_json::to_string(&ready).unwrap();
        assert_eq!(parse_ready(&text), Some(ready));
        for cut in 0..text.len() {
            assert_eq!(parse_ready(&text[..cut]), None, "{}", &text[..cut]);
        }
    }
    assert_eq!(parse_ready("{}"), None);
    assert_eq!(parse_exit("{}"), None);
    let failed = ExitRecord {
        ended: Ended::SetupFailed {
            step: Step::AuditSession,
            why: "no".to_owned(),
        },
        job_pid: None,
        asid: None,
    };
    assert_eq!(
        serde_json::to_string(&failed).unwrap(),
        r#"{"ended":{"how":"setup-failed","value":{"step":"audit-session","why":"no"}},"job_pid":null,"asid":null}"#
    );
}

/// Each step is printed as it is written in a record. Catches a stderr line that names a step
/// differently from the record the agent reads.
#[test]
fn every_step_is_printed_as_it_is_written() {
    for (step, name) in STEPS {
        assert_eq!(step.to_string(), name);
        assert_eq!(serde_json::to_string(&step).unwrap(), format!("\"{name}\""));
    }
}

/// A job's exit 125 and a failed step are told apart by the record, and both exit the
/// supervisor 125; a job's 7 is 7, a job killed by signal 9 is 137. Catches reading the status
/// instead of the record.
#[test]
fn the_job_status_and_setup_failure_are_told_apart() {
    let of = |ended| {
        status_of(&ExitRecord {
            ended,
            job_pid: None,
            asid: None,
        })
    };
    assert_eq!(of(Ended::Exited(125)), 125);
    assert_eq!(
        of(Ended::SetupFailed {
            step: Step::Profile,
            why: "refused".to_owned()
        }),
        status::SETUP_FAILED
    );
    assert_eq!(of(Ended::Exited(7)), 7);
    assert_eq!(of(Ended::Exited(0)), 0);
    assert_eq!(of(Ended::Signalled(9)), 137);
    assert_eq!(super::ended_of(exited(125)), Ended::Exited(125));
    assert_eq!(super::ended_of(killed(9)), Ended::Signalled(9));
}

/// With no exit record, the supervisor's own status says what it can: 125 is records that
/// could not be opened, 2 a refused spec, a signal a killed supervisor, any other code the
/// job's. Catches an agent that reads a refused spec or a missing record as a job's exit.
#[test]
fn a_supervisor_with_no_record_is_read_by_its_status() {
    let failed = |step, code| Ended::SetupFailed {
        step,
        why: format!("the supervisor exited {code} and wrote no record"),
    };
    assert_eq!(from_status(exited(125)), failed(Step::Records, 125));
    assert_eq!(from_status(exited(2)), failed(Step::Spec, 2));
    assert_eq!(from_status(exited(7)), Ended::Exited(7));
    assert_eq!(from_status(exited(0)), Ended::Exited(0));
    assert_eq!(from_status(killed(9)), Ended::Signalled(9));
}

/// The records are made empty, mode 0600, and each write lands whole in its own file; a second
/// open in the same directory is refused, because a record file is always new. Catches records
/// another user could read, or an old record taken for this start's.
#[test]
fn records_are_new_private_files_written_once_each() {
    let dir = TestDir::under(std::env::temp_dir(), "mac-supervisor-records");
    let mut records = Records::open(dir.path()).unwrap();
    for name in [READY, EXIT] {
        let file = fs::metadata(dir.join(name)).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600, "{name}");
        assert_eq!(file.len(), 0, "{name}");
    }
    let ready = Ready {
        job_pid: 4321,
        asid: None,
    };
    records.write_ready(&ready).unwrap();
    let record = every_end().remove(0);
    records.write_exit(&record).unwrap();
    let text = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
    assert_eq!(parse_ready(&text(READY)), Some(ready));
    assert_eq!(parse_exit(&text(EXIT)), Some(record));
    assert_eq!(
        Records::open(dir.path()).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
}

/// A write that wrote less than the record is an error that says how much. Catches a short
/// write left as a half record the agent would wait on.
#[test]
fn a_short_write_is_an_error() {
    assert!(whole(40, 40).is_ok());
    let short = whole(12, 40).unwrap_err();
    assert_eq!(short.kind(), std::io::ErrorKind::WriteZero);
    assert_eq!(short.to_string(), "wrote 12 of the record's 40 bytes");
}
