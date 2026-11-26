//! A restart of the agent at the runtime level. A run whose supervisor is still the process the
//! journal names, or whose supervisor left its exit record, is taken back by the next process:
//! the pool's open keeps its account, the listing holds it, and it is polled, stopped and
//! removed (its outputs collected, its account retired) as a run of this process. Every other
//! run of the journal (a supervisor gone with no record, a record of an earlier release that
//! names no supervisor) is listed ended (`Gone`, with its labels, its age and the output it
//! wrote) and its account is retired by the open, and the lifecycle reports the job failed and
//! removes it, which ends its lease; a lease with no run record (a start that died before its
//! job started) is dropped by `reclaim`; a run whose record could not be written still runs;
//! and a journal that cannot be read fails the listing.
//!
//! The fake supervisor here is the job itself and writes no record: a test writes the exit
//! record where the supervisor would (`exit.json` in the run's record directory).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use js_agent::lifecycle::{Exit, Found, RunHandle, Runtime, Status, label};
use js_agent::mac_supervisor::{Ended, ExitRecord, JobRusage};
use js_agent::native::Journal;
use js_agent_wire::{OutputKind, OutputSpec};

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
    let second = started(&mut rig, "r2", "/usr/bin/true", &[]);
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
    let id = started(&mut rig, "r1", "/usr/bin/true", &[]);
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
    let id = started(&mut rig, "r1", "/usr/bin/true", &[]);
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

/// Asserts that a run's start time is the wall clock read when its job is ready: two runs
/// started at two clock values record those values in the journal, and after a restart a run
/// has been running for the clock then minus its recorded start, in exact milliseconds.
///
/// Catches: a start time taken from another clock read, cached across runs, or left at zero,
/// which would make a restarted run's age wrong.
#[test]
fn a_runs_start_time_is_the_clock_at_job_ready() {
    let clock = Arc::new(AtomicU64::new(1_234_567));
    let read = clock.clone();
    let rig = Rig::in_dir("adopt-started", 3, |_, _| {});
    let mut rig = Rig {
        rt: rig
            .rt
            .with_clock(Arc::new(move || read.load(Ordering::Relaxed))),
        ..rig
    };
    let first = started(&mut rig, "r1", "/usr/bin/true", &[]);
    clock.store(2_000_000, Ordering::Relaxed);
    let second = started(&mut rig, "r2", "/usr/bin/true", &[]);
    let records = Journal::new(&rig.dir.path("native-leases")).runs().unwrap();
    assert_eq!(records[&first.0].started_ms, 1_234_567);
    assert_eq!(records[&second.0].started_ms, 2_000_000);

    let mut next = rig.reopened().with_clock(Arc::new(|| 2_060_000));
    let ages: BTreeMap<RunHandle, u64> = next
        .list_labelled()
        .unwrap()
        .into_iter()
        .map(|found| (found.id, found.running_ms))
        .collect();
    assert_eq!(ages, BTreeMap::from([(first, 825_433), (second, 60_000)]));
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// What the supervisor of a job that exited `code` writes, with what the job used.
fn exit_record(code: i32) -> ExitRecord {
    ExitRecord {
        ended: Ended::Exited(code),
        job_pid: None,
        asid: None,
        usage: Some(JobRusage {
            cpu_usec: 7_000,
            max_rss_bytes: 1 << 20,
        }),
    }
}

/// Writes `record` as the exit record of the `n`th launch of `rig`, as its supervisor would.
fn write_exit(rig: &Rig, n: usize, record: &ExitRecord) {
    let dir = rig.launcher.launched()[n].record_dir.clone();
    fs::write(dir.join("exit.json"), serde_json::to_vec(record).unwrap()).unwrap();
}

/// The account (its `USER`) of the `n`th launch of `rig`.
fn account(rig: &Rig, n: usize) -> String {
    rig.launcher.launched()[n].var("USER").unwrap().to_owned()
}

/// Asserts that a restart keeps the users of the runs it takes back and resets the rest: of
/// three runs, one whose supervisor still runs and one whose supervisor ended after writing its
/// exit record keep their accounts through the next process's open, and the one whose
/// supervisor ended with no record loses its account there. The listing holds all three with
/// their labels; the live run polls running until its supervisor writes its record, and then
/// its exit, with what the job used; the recorded run polls its exit at once; the third is
/// gone. Each removal retires its run's account and ends its lease.
///
/// Catches: a restart that ends a run its supervisor still runs, one that loses the end a
/// supervisor recorded while no agent ran, and one that keeps the account of a run nobody can
/// tell the end of.
#[test]
fn a_restart_keeps_the_users_of_runs_it_takes_back_and_resets_the_rest() {
    let mut rig = Rig::in_dir("adopt-keep", 3, |_, _| {});
    let live = started_unsettled(&mut rig, "r1", "/bin/sleep", &["600"]);
    let recorded = started(&mut rig, "r2", "/usr/bin/true", &[]);
    write_exit(&rig, 1, &exit_record(0));
    let lost = started(&mut rig, "r3", "/usr/bin/true", &[]);
    let names = [account(&rig, 0), account(&rig, 1), account(&rig, 2)];

    let (mut next, pool) = rig.reopened_with_pool();
    let records = rig.accounts.records();
    assert!(records.contains(&names[0]), "the live run's user is kept");
    assert!(
        records.contains(&names[1]),
        "the recorded run's user is kept"
    );
    assert!(!records.contains(&names[2]), "the lost run's user is reset");
    let found = next.list_labelled().unwrap();
    let ids: Vec<&RunHandle> = found.iter().map(|found| &found.id).collect();
    assert_eq!(ids, [&live, &recorded, &lost]);
    assert_eq!(found[0].labels, labels("r1"));

    assert_eq!(next.poll(&live).unwrap(), Status::Running);
    assert_eq!(next.poll(&recorded).unwrap(), exited(0));
    assert_eq!(next.poll(&lost).unwrap(), Status::Gone);
    write_exit(&rig, 0, &exit_record(3));
    assert_eq!(next.poll(&live).unwrap(), exited(3));
    assert_eq!(
        next.poll(&live).unwrap(),
        exited(3),
        "answered the same again"
    );
    let used = next.usage(&live).unwrap();
    assert_eq!(
        (used.cpu_usage_usec, used.peak_memory_bytes),
        (7_000, 1 << 20)
    );

    for id in [&live, &recorded, &lost] {
        next.remove(id).unwrap();
    }
    let records = rig.accounts.records();
    for name in &names {
        assert!(!records.contains(name), "{name} retired");
    }
    assert!(pool.held().is_empty());
    assert!(
        Journal::new(&rig.dir.path("native-leases"))
            .live()
            .unwrap()
            .is_empty()
    );
}

/// Starts `program args` as the run `run_id`, with its labels, and leaves it running.
fn started_unsettled(rig: &mut Rig, run_id: &str, program: &str, args: &[&str]) -> RunHandle {
    let mut spec = rig.spec(run_id, native(program, args));
    spec.labels = labels(run_id);
    rig.rt.create_and_start(&spec).unwrap()
}

/// Asserts that a run taken back keeps what its job wrote: a job that wrote its declared output
/// and ended while no agent ran (its supervisor's record says 0) has the output collected by
/// the next process's removal, before its account is retired.
///
/// Catches: a restart that reports a run's exit and loses its outputs.
#[test]
fn a_run_taken_back_has_its_outputs_collected() {
    let mut rig = Rig::new("adopt-outputs");
    let mut body = native("/bin/sh", &["-c", "echo made > out.txt"]);
    body.outputs.push(OutputSpec {
        path: "out.txt".to_owned(),
        kind: OutputKind::File,
    });
    let spec = rig.spec("r1", body);
    let id = rig.rt.create_and_start(&spec).unwrap();
    rig.settle(&id);
    write_exit(&rig, 0, &exit_record(0));
    let mut next = rig.reopened();
    assert_eq!(next.list_labelled().unwrap().len(), 1);
    assert_eq!(next.poll(&id).unwrap(), exited(0));
    next.remove(&id).unwrap();
    let copied = spec.scratch.join("outputs").join("out.txt");
    assert_eq!(fs::read_to_string(copied).unwrap(), "made\n");
}

/// Asserts that a run whose record an earlier release wrote, which names no supervisor, is not
/// taken back even while its job runs: its account is retired by the next process's open and
/// it is listed gone. The record is rewritten here as an earlier release wrote it.
///
/// Catches: a run taken back with no identity to tell its supervisor by, whose end nobody could
/// then read.
#[test]
fn a_run_recorded_by_an_earlier_release_is_not_taken_back() {
    let mut rig = Rig::new("adopt-earlier");
    let id = started_unsettled(&mut rig, "r1", "/bin/sleep", &["600"]);
    let name = account(&rig, 0);
    let path = rig.dir.path("native-leases");
    let text = fs::read_to_string(&path).unwrap();
    let earlier: String = text
        .lines()
        .map(|line| match line.split_once(" {") {
            Some((head, json)) => {
                let mut record: serde_json::Value =
                    serde_json::from_str(&format!("{{{json}")).unwrap();
                assert!(record["supervisor"].is_object(), "{line}");
                record.as_object_mut().unwrap().remove("supervisor");
                format!("{head} {record}\n")
            }
            None => format!("{line}\n"),
        })
        .collect();
    fs::write(&path, earlier).unwrap();
    let mut next = rig.reopened();
    assert!(
        !rig.accounts.records().contains(&name),
        "retired by the open"
    );
    assert_eq!(next.list_labelled().unwrap().len(), 1);
    assert_eq!(next.poll(&id).unwrap(), Status::Gone);
    next.remove(&id).unwrap();
}

/// Asserts that a run taken back holds its account's uid at the next process's start: with one
/// slot, held by the run, no account is ready, the pool still opens, and the self-test is not
/// run (no supervisor is started for it); once the run is removed, an account is ready again.
///
/// Catches: an agent that cannot start while the runs it took back fill its slots, and so
/// cannot report them.
#[test]
fn a_full_pool_of_runs_taken_back_skips_the_self_test() {
    let mut rig = Rig::new("adopt-full");
    let id = started_unsettled(&mut rig, "r1", "/bin/sleep", &["600"]);
    let (mut next, pool) = rig.reopened_with_pool();
    assert_eq!(pool.ready_count(), 0);
    next.self_test().unwrap();
    assert_eq!(rig.launcher.launched().len(), 1, "no self-test run");
    assert_eq!(next.list_labelled().unwrap().len(), 1);
    next.remove(&id).unwrap();
    assert!(pool.held().is_empty());
    assert_eq!(pool.ready_count(), 1, "the uid is free again");
}
