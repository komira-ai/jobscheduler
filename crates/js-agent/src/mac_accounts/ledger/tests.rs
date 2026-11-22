//! Tests of the ledger: what it keeps across a reopen, what it refuses, and the crash it
//! survives.

use js_testkit::dir::TestDir;

use super::*;
use crate::mac_accounts::tests::rig::Log;
use crate::test_tracing::capture;

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-accounts-ledger-{name}"),
    )
}

/// Asserts that an account number has one spelling and a name of the pool's own: a second
/// spelling would let one account be recorded twice under two names.
#[test]
fn a_name_has_one_spelling() {
    assert_eq!(name(7), "kbf-acct-7");
    assert_eq!(parse_name("kbf-acct-7"), Some(7));
    assert_eq!(parse_name("kbf-acct-18446744073709551615"), Some(u64::MAX));
    for bad in [
        "",
        "kbf-acct-",
        "kbf-acct-0",
        "kbf-acct-07",
        "kbf-acct-+7",
        "kbf-acct-7x",
        "kbf-acct-18446744073709551616",
        "kbf-lease-7",
        "root",
    ] {
        assert_eq!(parse_name(bad), None, "{bad:?}");
    }
}

/// Asserts that a job id is one to 63 of `a-z`, `0-9` and `-`, and nothing else: it goes
/// into a line of the ledger, so a space or a newline would forge another record.
#[test]
fn a_job_id_is_one_to_63_of_a_z_0_9_and_a_dash() {
    for good in ["a", "0", "-", "job-7", &"a".repeat(63)] {
        assert!(valid_job(good), "{good:?}");
    }
    for bad in ["", &"a".repeat(64), "Job", "a b", "a\nb", "a_b", "é"] {
        assert!(!valid_job(bad), "{bad:?}");
    }
    assert!(valid_step("proof-record"));
    assert!(valid_step("kill"));
    assert!(valid_step("step-2"));
    assert!(!valid_step(""));
    assert!(!valid_step("Kill"));
    assert!(!valid_step(&"k".repeat(41)));
}

/// Asserts that the records written are read back as they were: the state of each account,
/// its uid, the uid of the last `made`, the next number, and the lines as text; and that a
/// reopen sees the same.
#[test]
fn records_survive_reopening() {
    let dir = scratch("reopen");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.next(), 1);
    assert_eq!(ledger.last_uid(), None);
    for record in [
        Record::Made(1, 700),
        Record::Ready(1),
        Record::Lent(1, "job-1".to_owned()),
        Record::Made(2, 701),
        Record::Ready(2),
        Record::Made(3, 702),
        Record::Held(1, "proof-home".to_owned()),
        Record::Made(4, 703),
        Record::Gone(4),
    ] {
        ledger.append(&record).unwrap();
    }
    drop(ledger);
    let ledger = Ledger::open(&path).unwrap();
    let state = |n| ledger.get(n).map(|entry| (entry.uid, entry.state));
    assert_eq!(state(1), Some((700, State::Held)));
    assert_eq!(state(2), Some((701, State::Ready)));
    assert_eq!(state(3), Some((702, State::Made)));
    assert_eq!(state(4), Some((703, State::Gone)));
    assert_eq!(state(5), None);
    assert_eq!(ledger.next(), 5);
    assert_eq!(ledger.last_uid(), Some(703));
    assert_eq!(ledger.holding(702), Some(3));
    assert_eq!(ledger.holding(703), None, "a gone account holds no uid");
    assert_eq!(ledger.holding(799), None);
    assert_eq!(ledger.first_in(State::Ready).map(|(n, _)| n), Some(2));
    assert_eq!(ledger.first_in(State::Lent), None);
    assert_eq!(
        ledger.live().iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(ledger.in_state(State::Gone).len(), 1);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready kbf-acct-1\nlent kbf-acct-1 job-1\nmade kbf-acct-2 701\n\
         ready kbf-acct-2\nmade kbf-acct-3 702\nheld kbf-acct-1 proof-home\n\
         made kbf-acct-4 703\ngone kbf-acct-4\n"
    );
}

/// Asserts that a record that does not follow from the ones before is refused and not
/// written: a name made twice, a `ready` for an account never made or already ready, a
/// `lent` for one that is not ready (an account is lent once), a `held` or `gone` for one
/// that is gone, a bad job id or step.
#[test]
fn a_record_that_does_not_follow_is_refused_and_not_written() {
    let dir = scratch("transitions");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Made(1, 700)).unwrap();
    let refused =
        |ledger: &mut Ledger, record: Record| ledger.append(&record).unwrap_err().to_string();
    assert_eq!(
        refused(&mut ledger, Record::Made(1, 701)),
        "kbf-acct-1 was made twice"
    );
    assert_eq!(
        refused(&mut ledger, Record::Ready(2)),
        "kbf-acct-2 was never made, so it cannot be ready"
    );
    assert_eq!(
        refused(&mut ledger, Record::Lent(1, "Bad Job".to_owned())),
        "bad job id \"Bad Job\""
    );
    ledger.append(&Record::Ready(1)).unwrap();
    assert_eq!(
        refused(&mut ledger, Record::Ready(1)),
        "kbf-acct-1 is ready, so it cannot be ready"
    );
    assert_eq!(
        refused(&mut ledger, Record::Lent(1, "Bad Job".to_owned())),
        "bad job id \"Bad Job\""
    );
    ledger.append(&Record::Lent(1, "job".to_owned())).unwrap();
    assert_eq!(
        refused(&mut ledger, Record::Lent(1, "other".to_owned())),
        "kbf-acct-1 is lent, so it cannot be lent"
    );
    assert_eq!(
        refused(&mut ledger, Record::Held(1, "No Step".to_owned())),
        "bad step \"No Step\""
    );
    ledger.append(&Record::Held(1, "kill".to_owned())).unwrap();
    ledger.append(&Record::Held(1, "kill".to_owned())).unwrap();
    assert_eq!(
        refused(&mut ledger, Record::Lent(1, "again".to_owned())),
        "kbf-acct-1 is held, so it cannot be lent"
    );
    ledger.append(&Record::Gone(1)).unwrap();
    assert_eq!(
        refused(&mut ledger, Record::Gone(1)),
        "kbf-acct-1 is gone, so it cannot be gone"
    );
    assert_eq!(
        refused(&mut ledger, Record::Held(1, "kill".to_owned())),
        "kbf-acct-1 is gone, so it cannot be held"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready kbf-acct-1\nlent kbf-acct-1 job\nheld kbf-acct-1 kill\n\
         held kbf-acct-1 kill\ngone kbf-acct-1\n"
    );
}

/// Asserts that a torn last line (a crash between the write and its newline) is cut off and
/// said, so it cannot glue onto the next record, and that a complete line that does not
/// parse or does not follow refuses the open, naming its line.
#[test]
fn a_torn_last_line_is_cut_and_a_bad_line_refused() {
    let dir = scratch("torn");
    let path = dir.join("accounts");
    std::fs::write(&path, "made kbf-acct-1 700\nready kbf-").unwrap();
    let log = Log::default();
    let _capture = capture(log.clone(), tracing::Level::WARN);
    let mut ledger = Ledger::open(&path).unwrap();
    let text = log.text();
    assert!(text.contains("WARN"), "{text:?}");
    assert!(
        text.contains("dropping an account ledger record cut short by a crash"),
        "{text:?}"
    );
    assert!(text.contains("fragment=\"ready kbf-\""), "{text:?}");
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
    ledger.append(&Record::Ready(1)).unwrap();
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready kbf-acct-1\n"
    );

    for (bad, line, why) in [
        ("made kbf-acct-1\n", 1, "unknown record \"made kbf-acct-1\""),
        ("made kbf-acct-1 six\n", 1, "bad uid \"six\""),
        ("made kbf-acct-01 700\n", 1, "bad account \"kbf-acct-01\""),
        ("made kbf-lease-1 700\n", 1, "bad account \"kbf-lease-1\""),
        (
            "removed kbf-acct-1\n",
            1,
            "unknown record \"removed kbf-acct-1\"",
        ),
        (
            "ready kbf-acct-1\n",
            1,
            "kbf-acct-1 was never made, so it cannot be ready",
        ),
        (
            "made kbf-acct-1 700\nmade kbf-acct-1 701\n",
            2,
            "kbf-acct-1 was made twice",
        ),
        (
            "made kbf-acct-1 700\nready kbf-acct-1\nlent kbf-acct-1 Job\n",
            3,
            "bad job id \"Job\"",
        ),
        (
            "made kbf-acct-1 700\nheld kbf-acct-1 Kill\n",
            2,
            "bad step \"Kill\"",
        ),
        (
            "made kbf-acct-1 700\ngone kbf-acct-2\n",
            2,
            "kbf-acct-2 was never made, so it cannot be gone",
        ),
    ] {
        std::fs::write(&path, bad).unwrap();
        let error = Ledger::open(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bad:?}");
        assert_eq!(error.to_string(), format!("line {line}: {why}"), "{bad:?}");
    }
    std::fs::write(&path, b"made kbf-acct-1 \xff\n").unwrap();
    assert!(Ledger::open(&path).is_err(), "text that is not UTF-8");
}

/// Asserts that the ledger is not opened through a symbolic link planted at its path.
#[test]
fn a_symlink_at_the_ledger_path_is_refused() {
    let dir = scratch("link");
    std::fs::write(dir.join("elsewhere"), "").unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("accounts")).unwrap();
    assert!(Ledger::open(&dir.join("accounts")).is_err());
}

/// Asserts that a record the disk did not take changes nothing in memory (the pool must not
/// act on a record that is not durable), and that the file is made with mode 0600.
#[test]
fn a_failed_write_changes_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("fail");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    ledger.append(&Record::Made(1, 700)).unwrap();
    ledger.fail_writes(&path);
    assert!(ledger.append(&Record::Ready(1)).is_err());
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\n"
    );
}

/// Asserts that the next uid is chosen after the uid of the last `made` record in the order
/// written (not the highest uid, not the highest number), and that no other record moves
/// it; and that the next number is one more than any ever made, a gone account included.
#[test]
fn the_last_uid_and_the_next_number_follow_the_made_records() {
    let dir = scratch("last");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Made(5, 750)).unwrap();
    ledger.append(&Record::Made(2, 710)).unwrap();
    ledger.append(&Record::Gone(5)).unwrap();
    assert_eq!(ledger.last_uid(), Some(710));
    assert_eq!(ledger.next(), 6);
    drop(ledger);
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!((ledger.last_uid(), ledger.next()), (Some(710), 6));
}

/// Asserts that a write that fails part way leaves the file exactly as it was (the fragment
/// is cut off, not left for the next append to join into a garbage line), that the next
/// append is written whole, and that a reopen reads both records.
#[test]
fn a_failed_write_is_cut_back_and_the_next_append_reads_back() {
    let dir = scratch("cut");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Made(1, 700)).unwrap();
    ledger.inject.write = Some(5);
    assert!(ledger.append(&Record::Ready(1)).is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\n"
    );
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
    ledger.append(&Record::Ready(1)).unwrap();
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready kbf-acct-1\n"
    );
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Ready));
}

/// Asserts that a sync that fails after a complete write is rolled back: the record is not
/// in memory, and not in the file, on the spot or after a reopen.
#[test]
fn a_failed_sync_leaves_no_record_in_memory_or_on_disk() {
    let dir = scratch("sync");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Made(1, 700)).unwrap();
    ledger.inject.sync = true;
    assert!(ledger.append(&Record::Ready(1)).is_err());
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\n"
    );
    drop(ledger);
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
}

/// Asserts that when the cut back fails too the ledger is poisoned: every later append is
/// refused with an error that says so, even one that would be fine, until it is reopened;
/// the reopen cuts the torn tail the failed write left.
#[test]
fn a_failed_cut_back_poisons_the_ledger_until_it_is_reopened() {
    let dir = scratch("poison");
    let path = dir.join("accounts");
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Made(1, 700)).unwrap();
    ledger.inject.write = Some(5);
    ledger.inject.roll_back = true;
    let first = ledger.append(&Record::Ready(1)).unwrap_err().to_string();
    assert!(first.contains("poisoned"), "{first}");
    let later = ledger.append(&Record::Ready(1)).unwrap_err().to_string();
    assert!(later.contains("poisoned"), "{later}");
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Made));
    drop(ledger);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready"
    );
    let mut ledger = Ledger::open(&path).unwrap();
    ledger.append(&Record::Ready(1)).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "made kbf-acct-1 700\nready kbf-acct-1\n"
    );
}

/// Asserts that an account made for a lend may go from `made` straight to `lent`, once.
#[test]
fn a_made_account_may_be_lent_directly_but_only_once() {
    let dir = scratch("made-lent");
    let mut ledger = Ledger::open(&dir.join("accounts")).unwrap();
    ledger.append(&Record::Made(1, 700)).unwrap();
    ledger.append(&Record::Lent(1, "job-1".to_owned())).unwrap();
    assert_eq!(ledger.get(1).map(|entry| entry.state), Some(State::Lent));
    assert!(ledger.append(&Record::Lent(1, "job-2".to_owned())).is_err());
}
