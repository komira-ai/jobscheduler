//! The lease journal and what rests on it: the file's own rules, the run record a started
//! run leaves (and the records of earlier releases, which still read), a lease written
//! before the pool lends its account, the lease of a start that could not be undone dropped
//! by the next process's reclaim, and a journal that cannot be read or rewritten told.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;

use js_agent::lifecycle::Runtime;
use js_agent::native::{Journal, NativeRuntime, RunRecord};
use js_agent_wire::{OutputKind, OutputSpec};

use super::{Failing, Rig, native};
use crate::support::Scratch;

fn live(rig: &Rig) -> Vec<String> {
    Journal::new(&rig.dir.path("native-leases")).live().unwrap()
}

/// A runtime over the same pool state and journal as `rig`'s, as a later process of the agent
/// makes one.
fn later(rig: &Rig) -> NativeRuntime {
    rig.reopened()
}

fn seed(rig: &Rig, text: &str) {
    fs::write(rig.dir.path("native-leases"), text).unwrap();
}

/// Asserts the journal's own rules: a file that is not there is empty, an added lease is
/// live until it is marked done, a line that is neither an addition nor a removal (a torn
/// one) is ignored, a retirement drops exactly the leases it names, and every way the file cannot
/// be read, written or replaced is an error. It catches a journal that forgets a lease or
/// keeps one that was ended.
#[test]
fn the_journal_keeps_what_was_added_and_not_marked_done() {
    let dir = Scratch::new("journal-file");
    let journal = Journal::new(&dir.path("j"));
    assert_eq!(journal.live().unwrap(), Vec::<String>::new());
    journal.add("1.1").unwrap();
    journal.add("1.2").unwrap();
    journal.done("1.1").unwrap();
    assert_eq!(journal.live().unwrap(), ["1.2"]);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(dir.path("j"))
        .unwrap();
    write!(file, "torn\nx 9.9\n+ 1.3").unwrap();
    assert_eq!(journal.live().unwrap(), ["1.2", "1.3"]);
    journal
        .retire(&["1.2".to_owned(), "7.7".to_owned()])
        .unwrap();
    assert_eq!(journal.live().unwrap(), ["1.3"]);

    let nowhere = Journal::new(&dir.path("no-such-dir").join("j"));
    assert!(nowhere.add("1.1").is_err());
    assert!(nowhere.retire(&[]).is_err());
    let unreadable = Journal::new(&dir.path(""));
    assert!(unreadable.live().is_err());
}

/// Asserts that a lease added between the read of the journal and its retirement survives:
/// a worker that is still alive (wedged, not dead) appends `+ lease` while `reclaim` is
/// dropping the leases it read. It catches a retirement that writes back what was read, which
/// forgets a lease nothing else will drop.
#[test]
fn a_lease_added_while_others_are_retired_is_kept() {
    let dir = Scratch::new("journal-race");
    let journal = Journal::new(&dir.path("j"));
    journal.add("1.1").unwrap();
    journal.add("1.2").unwrap();
    let read = journal.live().unwrap();
    journal.add("9.9").unwrap();
    journal.retire(&read).unwrap();
    assert_eq!(journal.live().unwrap(), ["9.9"]);
}

/// Asserts the same with real concurrency: threads add leases while another retires others
/// over and over. Whatever the interleaving, every lease that was added and not named is
/// live at the end (the assertion does not depend on timing; only a lost write fails it).
/// It catches an append and a retirement that do not exclude each other.
#[test]
fn concurrent_adds_are_never_lost_to_a_retirement() {
    const THREADS: usize = 4;
    const EACH: usize = 40;
    let dir = Scratch::new("journal-threads");
    let journal = Journal::new(&dir.path("j"));
    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let journal = journal.clone();
            std::thread::spawn(move || {
                for n in 0..EACH {
                    journal.add(&format!("keep.{thread}.{n}")).unwrap();
                }
            })
        })
        .collect();
    for round in 0..EACH {
        let gone = format!("gone.{round}");
        journal.add(&gone).unwrap();
        journal.retire(&[gone]).unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let live = journal.live().unwrap();
    assert_eq!(live.len(), THREADS * EACH, "{live:?}");
    assert!(live.iter().all(|lease| lease.starts_with("keep.")));
}

/// Asserts that a started run leaves a run record in the journal with what a later process
/// needs: the pid its supervisor reported, the labels, scratch directory and home, the
/// declared outputs, no bundle and no deadline, and the name and uid of the account it was
/// lent; that the lease is live while the run is held, and that removing the run ends the
/// lease and its record.
///
/// Catches: a record that does not name the account (a later process could not tell whose
/// run it lists), or a lease left live after its run was removed.
#[test]
fn a_started_run_is_recorded_with_its_account_and_uid_until_it_is_removed() {
    let mut rig = Rig::new("journal-record");
    let mut body = native("/usr/bin/true", &[]);
    body.outputs.push(OutputSpec {
        path: "out".to_owned(),
        kind: OutputKind::File,
    });
    let mut spec = rig.spec("r1", body);
    spec.labels = BTreeMap::from([("k".to_owned(), "v".to_owned())]);
    let id = rig.rt.create_and_start(&spec).unwrap();
    let launched = rig.launcher.last();
    let name = launched.var("USER").unwrap().to_owned();
    let journal = Journal::new(&rig.dir.path("native-leases"));
    assert_eq!(live(&rig), std::slice::from_ref(&id.0));
    let records = journal.runs().unwrap();
    let record = &records[&id.0];
    assert_ne!(record.pid, 0);
    assert_eq!(record.deadline_ms, None);
    assert_eq!(record.labels, spec.labels);
    assert_eq!(record.scratch, spec.scratch);
    assert_eq!(record.home, rig.home_of(&name));
    assert_eq!(
        record.outputs,
        [OutputSpec {
            path: "out".to_owned(),
            kind: OutputKind::File
        }]
    );
    assert_eq!(record.bundle, None);
    assert_eq!(record.account, Some(name));
    assert_eq!(record.uid, Some(launched.uid));
    rig.settle(&id);
    rig.rt.remove(&id).unwrap();
    assert!(live(&rig).is_empty());
    assert!(journal.runs().unwrap().is_empty());
}

/// Asserts that run records written by earlier releases still read: one with a deadline and
/// no account or uid, one with only the fields every release wrote (no deadline, outputs or
/// bundle), and one with a field this release does not know, beside the new account and
/// uid. The fields an earlier record lacks read as none, and what it has reads as written.
///
/// Catches: a journal an update would leave unreadable, so that the next process lists none
/// of the runs the earlier one left.
#[test]
fn a_run_record_of_an_earlier_release_still_reads() {
    let dir = Scratch::new("journal-earlier");
    let path = dir.path("j");
    fs::write(
        &path,
        concat!(
            "+ 3.0\n",
            "= 3.0 {\"pid\":4242,\"started_ms\":1000,\"deadline_ms\":5000,",
            "\"labels\":{\"k\":\"v\"},\"scratch\":\"/s\",\"home\":\"/h\",",
            "\"outputs\":[{\"path\":\"out\"}],\"bundle\":\"b@sha256:00\"}\n",
            "+ 3.1\n",
            "= 3.1 {\"pid\":7,\"started_ms\":2,\"labels\":{},\"scratch\":\"/t\",\"home\":\"/g\"}\n",
            "+ 3.2\n",
            "= 3.2 {\"pid\":8,\"started_ms\":3,\"labels\":{},\"scratch\":\"/u\",\"home\":\"/i\",",
            "\"account\":\"kbf-acct-9\",\"uid\":509,\"later_field\":[1,2]}\n",
        ),
    )
    .unwrap();
    let runs = Journal::new(&path).runs().unwrap();
    assert_eq!(runs.keys().collect::<Vec<_>>(), ["3.0", "3.1", "3.2"]);
    let first = &runs["3.0"];
    assert_eq!((first.pid, first.started_ms), (4242, 1000));
    assert_eq!(first.deadline_ms, Some(5000));
    assert_eq!(first.labels["k"], "v");
    assert_eq!(first.outputs.len(), 1);
    assert_eq!(first.bundle.as_deref(), Some("b@sha256:00"));
    assert_eq!((first.account.as_deref(), first.uid), (None, None));
    let second = &runs["3.1"];
    assert_eq!((second.pid, second.started_ms), (7, 2));
    assert_eq!(second.deadline_ms, None);
    assert!(second.outputs.is_empty());
    assert_eq!(second.bundle, None);
    assert_eq!((second.account.as_deref(), second.uid), (None, None));
    let third = &runs["3.2"];
    assert_eq!(third.account.as_deref(), Some("kbf-acct-9"));
    assert_eq!(third.uid, Some(509));
}

/// Asserts that a record of this release, with its account and uid, reads back from the
/// journal as written, and survives a retirement of another lease.
///
/// Catches: a field of the record that the journal drops, or a retirement that rewrites a
/// kept record without its account.
#[test]
fn a_run_record_with_its_account_and_uid_reads_back_as_written() {
    let dir = Scratch::new("journal-roundtrip");
    let journal = Journal::new(&dir.path("j"));
    let record = RunRecord {
        pid: 31,
        started_ms: 1234,
        deadline_ms: None,
        labels: BTreeMap::from([("run".to_owned(), "r1".to_owned())]),
        scratch: "/scratch/r1".into(),
        home: "/Users/kbf-acct-4".into(),
        outputs: vec![OutputSpec {
            path: "out".to_owned(),
            kind: OutputKind::Dir,
        }],
        bundle: Some("example.com/b@sha256:00".to_owned()),
        account: Some("kbf-acct-4".to_owned()),
        uid: Some(504),
    };
    journal.add("8.0").unwrap();
    journal.started("8.0", &record).unwrap();
    journal.add("8.1").unwrap();
    assert_eq!(journal.runs().unwrap()["8.0"], record);
    journal.retire(&["8.1".to_owned()]).unwrap();
    assert_eq!(journal.live().unwrap(), ["8.0"]);
    assert_eq!(journal.runs().unwrap()["8.0"], record);
}

/// Asserts that the lease is in the journal before the pool lends an account: a start whose
/// lease cannot be journalled goes no further, so no account is lent (the pool's ready
/// account is the same one, none is held), no supervisor is started and no run is kept.
///
/// Catches: an account lent that no journal names.
#[test]
fn a_journal_that_cannot_be_written_stops_the_start_before_an_account_is_lent() {
    let mut rig = Rig::in_dir("journal-nowrite", 1, |config, dir| {
        config.journal = dir.path("no-such-dir").join("native-leases");
    });
    let ready = rig.accounts.records();
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    let error = rig.rt.create_and_start(&spec).unwrap_err();
    assert!(error.0.starts_with("the lease journal: "), "{}", error.0);
    assert_eq!(
        rig.accounts.records(),
        ready,
        "the ready account is still ready"
    );
    assert!(rig.pool.held().is_empty());
    assert!(rig.launcher.launched().is_empty());
}

/// Asserts that a start that cannot be undone keeps its lease: the supervisor cannot be
/// started and the account's record cannot be deleted, so the lease stays in the journal, and
/// the next process (its pool's open retires the account once the record can be deleted)
/// drops the lease at its reclaim and names it.
///
/// Catches: a lease nothing will ever drop, or an account left lent for good.
#[test]
fn a_lease_that_could_not_be_undone_is_dropped_by_a_later_runtime() {
    let mut rig = Rig::new("journal-undone");
    let lent = rig.accounts.records().remove(0);
    rig.accounts.keep_record(&lent);
    rig.launcher.fail(Failing::Launch);
    let spec = rig.spec("r1", native("/usr/bin/true", &[]));
    rig.rt.create_and_start(&spec).unwrap_err();
    assert_eq!(live(&rig), ["7.0"]);
    rig.accounts.free_record(&lent);
    let lines = later(&rig).reclaim();
    assert_eq!(
        lines,
        ["dropped the lease 7.0 an earlier process left before its job started"]
    );
    assert!(live(&rig).is_empty());
    assert!(
        !rig.accounts.records().contains(&lent),
        "retired by the open"
    );
}

/// Asserts that a journal that cannot be read is told and nothing else is done, and that one
/// that can be read but not rewritten still has its unstarted leases named, with the failure
/// to rewrite told beside them.
///
/// Catches: a reclaim that fails silently.
#[test]
fn a_journal_that_cannot_be_read_or_rewritten_is_told() {
    let rig = Rig::new("journal-broken");
    fs::create_dir(rig.dir.path("native-leases")).unwrap();
    let lines = later(&rig).reclaim();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("native: cannot read the lease journal"),
        "{lines:?}"
    );

    fs::remove_dir(rig.dir.path("native-leases")).unwrap();
    seed(&rig, "+ 4.4\n");
    fs::create_dir(rig.dir.path("native-leases.new")).unwrap();
    let lines = later(&rig).reclaim();
    assert!(
        lines
            .iter()
            .any(|l| l == "dropped the lease 4.4 an earlier process left before its job started"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("cannot rewrite the lease journal")),
        "{lines:?}"
    );
}
