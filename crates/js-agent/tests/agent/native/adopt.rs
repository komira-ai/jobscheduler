//! A restart of the agent at the runtime level. No run outlives the process that started it:
//! the next process's pool retires every account an earlier run held, so the runtime lists
//! each run the journal has a record of as ended (`Gone`, with its labels, its age and the
//! output it wrote), the lifecycle reports the job failed and removes it, which ends its
//! lease; a lease with no run record (a start that died before its job started) is dropped by
//! `reclaim`; a run whose record could not be written still runs; and a journal that cannot be
//! read fails the listing.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

use js_agent::lifecycle::{Exit, Found, RunHandle, Runtime, Status, label};
use js_agent::native::Journal;

use super::{Rig, native};

fn labels(run_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (label::RUN.to_owned(), run_id.to_owned()),
        (label::LEASE.to_owned(), format!("lease-{run_id}")),
    ])
}

fn handle(lease: &str) -> RunHandle {
    RunHandle(lease.to_owned())
}

/// Starts `program args` as the run `run_id`, with its labels, and settles it.
fn started(rig: &mut Rig, run_id: &str, program: &str, args: &[&str]) -> RunHandle {
    let mut spec = rig.spec(run_id, native(program, args));
    spec.labels = labels(run_id);
    let id = rig.rt.create_and_start(&spec).unwrap();
    rig.settle(&id);
    id
}

/// Asserts the restart of several runs: the next runtime's reclaim leaves the runs' leases
/// alone (they have records), its listing holds each run with the labels its start had and the
/// time it has run on the wall clock, once, the accounts of the earlier runs are retired by
/// the pool's open, each run polls `Gone`, the output its job wrote is still its log, and
/// the removal of each ends its lease.
///
/// Catches: a restart that forgets the runs it left (the lifecycle would never report them),
/// an earlier run whose account is left lent, and a run whose wall time starts again from
/// zero.
#[test]
fn a_restart_lists_every_run_of_the_earlier_process_ended_and_removes_them() {
    let mut rig = Rig::in_dir("adopt-many", 3, |_, _| {});
    let first = started(&mut rig, "r1", "/bin/echo", &["from the earlier process"]);
    let second = started(&mut rig, "r2", "/bin/true", &[]);
    assert_eq!((&first, &second), (&handle("7.0"), &handle("7.1")));
    let earlier: Vec<String> = rig
        .launcher
        .launched()
        .iter()
        .map(|launched| launched.var("USER").unwrap().to_owned())
        .collect();
    let journal = Journal::new(&rig.dir.path("native-leases"));
    let records = journal.runs().unwrap();
    let (one, two) = (records["7.0"].started_ms, records["7.1"].started_ms);
    let now = one.max(two) + 60_000;

    let mut next = rig.reopened().with_clock(Arc::new(move || now));
    for name in &earlier {
        assert!(!rig.accounts.records().contains(name), "{name} retired");
    }
    assert!(
        next.reclaim().is_empty(),
        "runs with records are not dropped"
    );
    let want = [
        Found {
            id: first.clone(),
            labels: labels("r1"),
            running_ms: now - one,
            never_started: false,
        },
        Found {
            id: second.clone(),
            labels: labels("r2"),
            running_ms: now - two,
            never_started: false,
        },
    ];
    assert_eq!(next.list_labelled().unwrap(), want);
    assert_eq!(next.list_labelled().unwrap().len(), 2, "listed once each");
    for id in [&first, &second] {
        assert_eq!(next.poll(id).unwrap(), Status::Gone);
    }
    let tail = next.log_tail(&first).unwrap().unwrap();
    assert_eq!(tail.stdout, "from the earlier process\n");
    for id in [&first, &second] {
        next.remove(id).unwrap();
    }
    assert!(journal.live().unwrap().is_empty());
    assert!(next.list_labelled().unwrap().is_empty());
    assert_eq!(next.poll(&first).unwrap(), Status::Gone);
}

/// Asserts that `reclaim` drops exactly the leases that have no run record (a start that
/// died before its job started, which the pool's open has already retired the account of),
/// names each, and leaves the lease of a run that has a record for the listing.
///
/// Catches: a lease of a start that never ran kept for ever, and the lease of a run the
/// lifecycle has still to report dropped.
#[test]
fn a_reclaim_drops_the_leases_of_starts_that_never_ran_and_keeps_the_runs() {
    let mut rig = Rig::new("adopt-reclaim");
    let id = started(&mut rig, "r1", "/bin/true", &[]);
    let path = rig.dir.path("native-leases");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(b"+ 7.9\n"))
        .unwrap();
    let journal = Journal::new(&path);
    assert_eq!(journal.live().unwrap(), ["7.0", "7.9"]);
    let mut next = rig.reopened();
    assert_eq!(
        next.reclaim(),
        ["dropped the lease 7.9 an earlier process left before its job started"]
    );
    assert_eq!(journal.live().unwrap(), ["7.0"]);
    assert!(next.reclaim().is_empty(), "a second look finds nothing");
    let found = next.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    next.remove(&id).unwrap();
    assert!(journal.live().unwrap().is_empty());
}

/// Asserts that a journal that cannot be read fails the listing (the agent cannot say what
/// it holds, and must not say it holds nothing), and that the listing works again once the
/// journal can be read.
///
/// Catches: an agent that registers with an empty list while runs of an earlier process are
/// still in the journal.
#[test]
fn a_listing_that_cannot_read_the_journal_fails() {
    let mut rig = Rig::new("adopt-lost");
    let id = started(&mut rig, "r1", "/bin/true", &[]);
    let mut next = rig.reopened();
    let path = rig.dir.path("native-leases");
    let kept = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let error = next.list_labelled().unwrap_err();
    assert!(error.0.starts_with("the lease journal: "), "{}", error.0);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, kept).unwrap();
    let found = next.list_labelled().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
}

/// Asserts that a run whose record cannot be written still runs: the record costs only the
/// listing after a restart (the lease, written before, is dropped by the next process's
/// reclaim, and its account by the pool's open). The journal here becomes a directory when
/// the run directory is given to the account, between the lease and the record.
///
/// Catches: a started run failed, and its account left lent, because a record could not be
/// written.
#[test]
fn a_run_whose_record_cannot_be_written_still_runs() {
    let rig = Rig::new("adopt-norecord");
    let journal = rig.dir.path("native-leases");
    let breaks = journal.clone();
    let mut rig = rig.handing_over(Arc::new(
        move |_: &Path, _: u32, _: u32| -> io::Result<()> {
            if breaks.is_file() {
                fs::remove_file(&breaks)?;
                fs::create_dir(&breaks)?;
            }
            Ok(())
        },
    ));
    let lent = rig.accounts.records().remove(0);
    let id = rig.start("r1", "/bin/sleep", &["600"]);
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    assert!(
        Journal::new(&journal).runs().is_err(),
        "the journal is a directory now"
    );
    rig.rt
        .stop(&id, std::time::Duration::from_secs(600))
        .unwrap();
    assert_eq!(
        rig.settle(&id),
        Status::Exited(Exit {
            code: 128 + 15,
            oom_killed: false
        })
    );
    rig.rt.remove(&id).unwrap();
    assert!(!rig.accounts.records().contains(&lent), "retired");
    assert!(rig.pool.held().is_empty());
}
