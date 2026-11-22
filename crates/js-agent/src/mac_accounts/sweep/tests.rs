//! Tests of what opening the pool does about what a crash left: one test for each point the
//! ledger can be left at, and for the records the ledger does not know.

use super::*;
use crate::mac_accounts::tests::os::FakeOs;
use crate::mac_accounts::tests::rig::{Rig, me, my_gid};
use crate::mac_accounts::{Account, generated_uid};

/// The record and the home of account `n` with `uid`, as the making leaves them.
fn made(rig: &Rig, n: u64, uid: u32) {
    rig.fake
        .add_user(&Rig::name(n), uid, my_gid(), &rig.settings.homes);
    std::fs::create_dir(rig.home(n)).unwrap();
}

fn line(text: &str) -> String {
    text.replace("{me}", &me().to_string())
        .replace("{me+1}", &(me() + 1).to_string())
}

/// Asserts a crash after `made` was written: the account's record and home, made or not,
/// are deleted and shown gone, the ledger says `gone`, and the pool is filled with an account
/// under a new number and the next uid.
#[test]
fn a_made_account_that_was_never_ready_is_deleted() {
    let rig = Rig::new("crash-made", 6, 1);
    rig.write_ledger(&line("made kbf-acct-1 {me}\n"));
    made(&rig, 1, me());
    let pool = rig.open();
    assert_eq!(
        rig.lines(),
        [
            line("made kbf-acct-1 {me}"),
            "gone kbf-acct-1".to_owned(),
            line("made kbf-acct-2 {me+1}"),
            "ready kbf-acct-2".to_owned(),
        ]
    );
    assert_eq!(
        rig.fake.state().users.keys().collect::<Vec<_>>(),
        ["kbf-acct-2"]
    );
    assert!(!rig.home(1).exists());
    assert_eq!(pool.ready_count(), 1);

    // Nothing made at all: the crash came right after the record.
    let rig = rig.next("crash-nothing", 6, 0);
    rig.write_ledger(&line("made kbf-acct-1 {me}\n"));
    rig.open();
    assert_eq!(
        rig.lines(),
        [line("made kbf-acct-1 {me}"), "gone kbf-acct-1".to_owned()]
    );
}

/// Asserts that a ready account whose record and home are as they were made stays ready, with
/// nothing killed, deleted or recorded; and that each way it is not is deleted: its record
/// gone, its home gone or not a directory, its record with another uid.
#[test]
fn a_ready_account_that_is_intact_is_kept_and_one_that_is_not_is_deleted() {
    let rig = Rig::new("crash-ready", 6, 1);
    let ledger = line("made kbf-acct-1 {me}\nready kbf-acct-1\n");
    rig.write_ledger(&ledger);
    made(&rig, 1, me());
    let pool = rig.open();
    assert_eq!(pool.ready_count(), 1);
    assert_eq!(rig.ledger(), ledger);
    assert!(
        rig.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("kill_all"))
    );

    let deleted = line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\ngone kbf-acct-1\nmade kbf-acct-2 {me+1}\nready kbf-acct-2\n",
    );
    for (name, spoil) in [
        (
            "record-vanished",
            Box::new(|rig: &Rig| {
                rig.fake.state().users.clear();
            }) as Box<dyn Fn(&Rig)>,
        ),
        (
            "home-vanished",
            Box::new(|rig: &Rig| std::fs::remove_dir(rig.home(1)).unwrap()),
        ),
        (
            "home-a-file",
            Box::new(|rig: &Rig| {
                std::fs::remove_dir(rig.home(1)).unwrap();
                std::fs::write(rig.home(1), "").unwrap();
            }),
        ),
        (
            "other-uid",
            Box::new(|rig: &Rig| {
                rig.fake.state().users.get_mut("kbf-acct-1").unwrap().uid += 1;
            }),
        ),
    ] {
        let rig = rig.next(name, 6, 1);
        rig.write_ledger(&ledger);
        made(&rig, 1, me());
        spoil(&rig);
        let pool = rig.open();
        assert_eq!(rig.ledger(), deleted, "{name}");
        assert_eq!(pool.ready_count(), 1, "{name}");
        assert!(!rig.home(1).exists(), "{name}");
    }
}

/// Asserts that a ready account whose record cannot be looked up is not taken as intact: it is
/// deleted, and when the proof cannot look either it is held, and the open goes on.
#[test]
fn a_ready_account_that_cannot_be_looked_at_is_not_intact() {
    let rig = Rig::new("crash-ready-lookup", 6, 1);
    rig.write_ledger(&line("made kbf-acct-1 {me}\nready kbf-acct-1\n"));
    made(&rig, 1, me());
    rig.fake.fail("account kbf-acct-1");
    let pool = rig.open();
    assert_eq!(
        pool.held(),
        [Account {
            name: "kbf-acct-1".to_owned(),
            uid: me(),
            gid: my_gid(),
            generated_uid: generated_uid(me()),
        }]
    );
    assert_eq!(rig.lines()[2], "held kbf-acct-1 proof-record");
    assert_eq!(
        pool.ready_count(),
        1,
        "another account was made for the pool"
    );
    let log = rig.log.text();
    assert!(
        log.contains("held at proof-record: looking up kbf-acct-1"),
        "{log}"
    );
}

/// Asserts that an account that was lent (its job died with the process) is ended: its
/// processes killed, its record and home deleted, `gone` recorded.
#[test]
fn a_lent_account_is_deleted_at_open() {
    let rig = Rig::new("crash-lent", 6, 0);
    rig.write_ledger(&line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\nlent kbf-acct-1 job-1\n",
    ));
    made(&rig, 1, me());
    rig.fake.add_process(me(), 40, "node");
    let pool = rig.open();
    assert!(rig.fake.state().processes.is_empty());
    assert!(rig.fake.state().users.is_empty());
    assert!(!rig.home(1).exists());
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-1");
    assert!(pool.held().is_empty());
}

/// Asserts that a held account is tried again at open: deleted when its cause is gone, and
/// held again, recorded again and logged when it is not, without failing the open; and that
/// its uid is not chosen meanwhile.
#[test]
fn a_held_account_is_tried_again_at_open() {
    let rig = Rig::new("crash-held", 6, 1);
    let ledger = line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\nlent kbf-acct-1 job-1\nheld kbf-acct-1 kill\n",
    );
    rig.write_ledger(&ledger);
    made(&rig, 1, me());
    rig.fake.add_process(me(), 40, "daemon");
    rig.fake.state().immortal.insert(me());
    let pool = rig.open();
    assert_eq!(pool.held().len(), 1);
    assert_eq!(rig.lines()[4], "held kbf-acct-1 kill");
    assert_eq!(
        rig.lines()[5],
        line("made kbf-acct-2 {me+1}"),
        "the held uid is passed over"
    );
    let log = rig.log.text();
    assert!(log.contains("held at kill: 1 processes of uid"), "{log}");
    drop(pool);

    rig.fake.state().immortal.clear();
    let pool = rig.open();
    assert!(pool.held().is_empty());
    assert!(rig.lines().contains(&"gone kbf-acct-1".to_owned()));
}

/// Asserts that a record the ledger knows as gone, which is there again (a lost deletion, or
/// one made by hand), and a record it does not know at all, are deleted by name at open, with
/// nothing recorded for them: a record in the pool's range and prefix is the pool's to end.
#[test]
fn a_record_the_ledger_does_not_hold_is_deleted_by_name() {
    let rig = Rig::new("crash-unknown", 6, 1);
    rig.write_ledger(&line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\ngone kbf-acct-1\n",
    ));
    made(&rig, 1, me());
    made(&rig, 9, me() + 3);
    rig.fake
        .add_user("kbf-acct-x", me() + 4, my_gid(), &rig.settings.homes);
    let pool = rig.open();
    assert_eq!(
        rig.fake.state().users.keys().collect::<Vec<_>>(),
        ["kbf-acct-2"]
    );
    assert!(!rig.home(1).exists() && !rig.home(9).exists());
    assert_eq!(
        rig.lines(),
        [
            line("made kbf-acct-1 {me}"),
            "ready kbf-acct-1".to_owned(),
            "gone kbf-acct-1".to_owned(),
            line("made kbf-acct-2 {me+1}"),
            "ready kbf-acct-2".to_owned(),
        ]
    );
    assert_eq!(pool.ready_count(), 1);
    let log = rig.log.text();
    assert!(
        log.contains("deleted an account the ledger did not know"),
        "{log}"
    );
}

/// Asserts that a record outside the range, one whose name is not the pool's though the host
/// listed it, and a known account are not touched.
#[test]
fn a_record_outside_the_range_or_the_prefix_is_not_touched() {
    let rig = Rig::new("crash-outside", 6, 0);
    rig.fake
        .add_user("kbf-acct-5", 5000, my_gid(), &rig.settings.homes);
    rig.fake
        .add_user("bob", me(), my_gid(), &rig.settings.homes);
    rig.fake.state().list_ignores_prefix = true;
    rig.open();
    assert_eq!(
        rig.fake.state().users.keys().collect::<Vec<_>>(),
        ["bob", "kbf-acct-5"]
    );
    assert!(
        rig.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("delete_account"))
    );
}

/// Asserts that a record the ledger does not know and that cannot be deleted fails the open,
/// naming the account and the step, with nothing recorded and nothing made: an account the
/// ledger does not hold cannot be held, so the open says so and tries again at the next.
#[test]
fn an_unknown_record_that_cannot_be_deleted_fails_the_open_by_name() {
    let rig = Rig::new("crash-unknown-stuck", 6, 1);
    made(&rig, 9, me() + 3);
    rig.fake.fail("delete_account kbf-acct-9");
    let why = rig.refused();
    let AccountError::Make {
        account,
        step,
        why: text,
    } = &why
    else {
        panic!("{why:?}");
    };
    assert_eq!((account.as_str(), *step), ("kbf-acct-9", Step::Record));
    assert_eq!(
        why.to_string(),
        "making account kbf-acct-9: record: deleting the record kbf-acct-9: delete_account kbf-acct-9 failed"
    );
    assert_eq!(
        text,
        "deleting the record kbf-acct-9: delete_account kbf-acct-9 failed"
    );
    assert_eq!(rig.ledger(), "");
    assert!(rig.fake.state().users.contains_key("kbf-acct-9"));
    rig.fake.heal("delete_account kbf-acct-9");
    rig.open();
    assert!(!rig.fake.state().users.contains_key("kbf-acct-9"));
}

/// Asserts that a directory that cannot be listed fails the open, naming what was asked.
#[test]
fn a_directory_that_cannot_be_listed_fails_the_open() {
    let rig = Rig::new("crash-listing", 6, 0);
    rig.fake.fail("list_accounts");
    let why = rig.refused();
    assert!(matches!(why, AccountError::Host { .. }), "{why:?}");
    assert_eq!(
        why.to_string(),
        "listing the accounts of the pool: list_accounts kbf-acct- failed"
    );
}

/// Asserts that a ledger that cannot record the outcome of the sweep fails it, naming the
/// ledger and not the account: the account was deleted, and the next open deletes it again
/// and records it.
#[test]
fn a_ledger_that_cannot_record_the_sweep_fails_it() {
    let rig = Rig::new("crash-ledger", 6, 0);
    let pool = rig.open();
    pool.lend("job-1").unwrap();
    pool.ledger().fail_writes(&rig.dir.join("state/accounts"));
    let why = pool.sweep().unwrap_err();
    assert!(matches!(why, AccountError::Ledger { .. }), "{why:?}");
    assert!(why.to_string().contains("recording gone: "), "{why}");
    assert!(rig.fake.state().users.is_empty(), "the account was deleted");
}

/// Asserts that a per-user folder a crash left non-empty after its account was deleted (the
/// ledger says `gone`, or knows nothing of the uid) is emptied at open, and kept; and that
/// the folders of a uid a live account of the ledger holds are not touched by that pass.
#[test]
fn a_folder_a_crash_left_non_empty_is_emptied_at_open() {
    let rig = Rig::new("crash-folder", 6, 0);
    rig.write_ledger(&line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\ngone kbf-acct-1\n",
    ));
    let hash = rig.folder("ab", "hash");
    std::fs::create_dir_all(hash.join("T/inner")).unwrap();
    std::fs::write(hash.join("T/inner/f"), "x").unwrap();
    std::fs::write(hash.join("stray"), "x").unwrap();
    rig.open();
    assert_eq!(std::fs::read_dir(&hash).unwrap().count(), 4);
    for top in ["0", "C", "T", "X"] {
        assert_eq!(
            std::fs::read_dir(hash.join(top)).unwrap().count(),
            0,
            "{top}"
        );
    }

    // A uid no ledger line names is emptied too.
    let rig = rig.next("crash-folder-unknown", 6, 0);
    let hash = rig.folder("ab", "hash");
    std::fs::write(hash.join("C/f"), "x").unwrap();
    rig.open();
    assert_eq!(std::fs::read_dir(hash.join("C")).unwrap().count(), 0);

    // A held account's uid is its own to retry: the pass leaves its folder alone (the account's
    // deletion itself stops at the kill, before the folders).
    let rig = rig.next("crash-folder-held", 6, 0);
    rig.write_ledger(&line(
        "made kbf-acct-1 {me}\nready kbf-acct-1\nlent kbf-acct-1 job-1\nheld kbf-acct-1 kill\n",
    ));
    made(&rig, 1, me());
    rig.fake.add_process(me(), 40, "daemon");
    rig.fake.state().immortal.insert(me());
    let hash = rig.folder("ab", "hash");
    std::fs::write(hash.join("T/f"), "x").unwrap();
    let pool = rig.open();
    assert_eq!(pool.held().len(), 1);
    assert!(hash.join("T/f").is_file());
}

/// Asserts that a folder that cannot be emptied at open is logged with its uid and why, does
/// not fail the open, and leaves its slot not free: a loan is refused naming what is left.
#[test]
fn a_folder_that_cannot_be_emptied_at_open_is_logged_and_its_slot_is_not_free() {
    let rig = Rig::new("crash-folder-stuck", 1, 0);
    let hash = rig.folder("ab", "hash");
    std::fs::write(hash.join("T/frozen"), "x").unwrap();
    let os = FakeOs::new();
    os.flag_for_good("frozen");
    let pool = rig.open_with(&os);
    let log = rig.log.text();
    assert!(
        log.contains("emptying the per-user folders of a free uid")
            && log.contains("T/frozen: the flags cannot be cleared"),
        "{log}"
    );
    let Err(AccountError::NoFreeUid { reasons, .. }) = pool.lend("job-1") else {
        panic!("the slot was free");
    };
    assert_eq!(
        reasons,
        [format!(
            "{}: {} is left in its per-user folder",
            me(),
            hash.join("T/frozen").display()
        )]
    );
}

/// Asserts that the pass at open leaves alone the per-user folders of a uid in the range that
/// something outside the pool uses: a record the pool did not make (an earlier release's
/// account a job may be running as), a live process of the uid, or a uid that cannot be asked
/// about. Catches a pass that emptied a working account's temporary files under it.
#[test]
fn a_folder_of_a_uid_something_else_uses_is_not_emptied_at_open() {
    let rig = Rig::new("folder-in-use-record", 1, 0);
    rig.fake
        .add_user("kbf-lease-pool-0", me(), my_gid(), &rig.settings.homes);
    let hash = rig.folder("ab", "hash");
    std::fs::write(hash.join("T/working"), "x").unwrap();
    let _ = rig.try_open();
    assert!(
        hash.join("T/working").is_file(),
        "a record's folder was emptied"
    );

    let rig = rig.next("folder-in-use-process", 1, 0);
    rig.fake.add_process(me(), 41, "build");
    let hash = rig.folder("ab", "hash");
    std::fs::write(hash.join("C/working"), "x").unwrap();
    let _ = rig.try_open();
    assert!(
        hash.join("C/working").is_file(),
        "a running uid's folder was emptied"
    );

    for key in [format!("uid_taken {}", me()), format!("processes {}", me())] {
        let rig = rig.next("folder-in-use-unknown", 1, 0);
        rig.fake.fail(&key);
        let hash = rig.folder("ab", "hash");
        std::fs::write(hash.join("0/working"), "x").unwrap();
        let _ = rig.try_open();
        assert!(
            hash.join("0/working").is_file(),
            "{key}: emptied on a failed look"
        );
    }
}
