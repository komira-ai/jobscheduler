//! The identity of an earlier process's supervisor, whether its run can be taken back, and the
//! handle that reads its records, over a process table the tests set.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use js_testkit::dir::TestDir;

use super::{Adopted, GONE, StartOf, SupervisorId, adoptable};
use crate::mac_supervisor::Supervised;
use crate::mac_supervisor::record::{EXIT, Ended, ExitRecord, JobRusage, Ready, Records, Step};

/// The pid of the supervisor of these tests, and the time it started.
const PID: u32 = 4100;
const START: u64 = 1_700_000_000_123_456;

/// What a supervisor wrote when its job exited 3.
fn exited() -> ExitRecord {
    ExitRecord {
        ended: Ended::Exited(3),
        job_pid: Some(4101),
        asid: Some(100_017),
        usage: Some(JobRusage {
            cpu_usec: 9,
            max_rss_bytes: 4096,
        }),
    }
}

/// A process table that has the one process `table` holds, if any: its pid and start time.
fn table(entry: Option<(u32, u64)>) -> StartOf {
    Arc::new(move |pid| entry.filter(|(at, _)| *at == pid).map(|(_, start)| start))
}

/// A record directory, with the two record files made empty as the supervisor makes them.
fn records(name: &str) -> (TestDir, PathBuf, Records) {
    let dir = TestDir::under(
        std::env::temp_dir(),
        &format!("mac-supervisor-adopt-{name}"),
    );
    let path = dir.path().to_path_buf();
    let records = Records::open(&path).unwrap();
    (dir, path, records)
}

fn id() -> SupervisorId {
    SupervisorId {
        pid: PID,
        start: START,
    }
}

/// Asserts that an identity is read from the process table, and names its process only while
/// that process runs: a pid the table does not have has no identity; the same pid with another
/// start time (used again by a later process) or gone from the table is not alive. Catches a
/// supervisor taken for alive by its pid alone.
#[test]
fn an_identity_names_one_process_by_its_pid_and_start() {
    let running = table(Some((PID, START)));
    assert_eq!(SupervisorId::of(PID, &*running), Some(id()));
    assert_eq!(SupervisorId::of(PID + 1, &*running), None);
    assert!(id().alive(&*running));
    assert!(!id().alive(&*table(Some((PID, START + 1)))));
    assert!(!id().alive(&*table(None)));
}

/// Asserts which runs can be taken back: one whose exit record is whole, whether or not its
/// supervisor runs; one whose supervisor runs, with an empty record or with none made yet; and
/// not one whose supervisor is gone with an empty, a partial, or no record. Catches a run
/// adopted whose end nobody can tell, and one ended while its supervisor still runs it.
#[test]
fn a_run_is_adoptable_with_its_exit_record_or_its_supervisor_running() {
    let running = table(Some((PID, START)));
    let gone = table(None);
    let (_dir, path, mut written) = records("adoptable");
    assert!(adoptable(&path, id(), &*running), "running, empty record");
    assert!(!adoptable(&path, id(), &*gone), "gone, empty record");
    written.write_exit(&exited()).unwrap();
    assert!(adoptable(&path, id(), &*gone), "gone, whole record");
    assert!(adoptable(&path, id(), &*running), "running, whole record");

    let (_dir, path, _records) = records("adoptable-partial");
    std::fs::write(path.join(EXIT), br#"{"ended":{"how":"exited""#).unwrap();
    assert!(!adoptable(&path, id(), &*gone), "gone, partial record");

    let nowhere = path.join("not-made");
    assert!(
        adoptable(&nowhere, id(), &*running),
        "running, no record made"
    );
    assert!(!adoptable(&nowhere, id(), &*gone), "gone, no record made");
}

/// Asserts the handle of an adopted supervisor: it names the supervisor's pid and its record
/// directory; its ready record is the one in the directory, or the one kept when that cannot be
/// read; it says nothing while the supervisor runs with no record, the exit record once there
/// is one (each time it is asked), and once the supervisor is gone with none, a failed setup at
/// the spawn step saying so, with the job's pid and session from the ready record. Catches an
/// adopted run that never ends, one ended while it runs, and a supervisor's death read as an
/// exit of the job.
#[test]
fn an_adopted_supervisor_is_read_through_its_records() {
    let kept = Ready {
        job_pid: 4101,
        asid: None,
    };
    let state = Arc::new(Mutex::new(Some((PID, START))));
    let seen = Arc::clone(&state);
    let start_of: StartOf = Arc::new(move |pid| {
        let entry = *seen.lock().unwrap_or_else(PoisonError::into_inner);
        entry.filter(|(at, _)| *at == pid).map(|(_, start)| start)
    });

    let (_dir, path, mut written) = records("handle");
    let mut handle = Adopted::new(id(), path.clone(), kept, Arc::clone(&start_of));
    assert_eq!(handle.pid(), PID);
    assert_eq!(handle.record_dir(), path);
    assert_eq!(
        handle.wait_ready(),
        Ok(kept),
        "no ready record: the one kept"
    );
    assert_eq!(handle.try_exit().unwrap(), None);
    written.write_exit(&exited()).unwrap();
    assert_eq!(handle.try_exit().unwrap(), Some(exited()));
    *state.lock().unwrap() = None;
    assert_eq!(handle.try_exit().unwrap(), Some(exited()), "asked again");

    let (_dir, path, mut written) = records("handle-gone");
    let ready = Ready {
        job_pid: 4102,
        asid: Some(100_018),
    };
    written.write_ready(&ready).unwrap();
    let mut handle = Adopted::new(id(), path, kept, Arc::clone(&start_of));
    assert_eq!(handle.wait_ready(), Ok(ready), "the ready record written");
    let gone = ExitRecord {
        ended: Ended::SetupFailed {
            step: Step::Spawn,
            why: GONE.to_owned(),
        },
        job_pid: Some(4102),
        asid: Some(100_018),
        usage: None,
    };
    assert_eq!(handle.try_exit().unwrap(), Some(gone));
}

/// Asserts that a supervisor that writes its record and exits between the handle's first look
/// at the record and its look at the process is read by its record, not as gone: the process
/// table here writes the record the moment it is asked, and has no such process. Catches an
/// end that a supervisor did record reported as lost.
#[test]
fn a_record_written_as_the_supervisor_exits_is_read() {
    let (_dir, path, written) = records("handle-race");
    let written = Mutex::new(Some(written));
    let start_of: StartOf = Arc::new(move |_| {
        if let Some(mut records) = written.lock().unwrap().take() {
            records.write_exit(&exited()).unwrap();
        }
        None
    });
    let kept = Ready {
        job_pid: 4101,
        asid: None,
    };
    let mut handle = Adopted::new(id(), path, kept, start_of);
    assert_eq!(handle.try_exit().unwrap(), Some(exited()));
}
