//! Tests of making an account: its shape, the order of what is recorded and made, how a uid
//! is chosen, and what a failure part-way leaves.

use std::sync::{Arc, Mutex};

use super::*;
use crate::mac_accounts::ledger::Record;
use crate::mac_accounts::tests::fake::Print;
use crate::mac_accounts::tests::rig::{Rig, me, my_gid};
use crate::mac_accounts::{MAX_NAME, PREFIX, generated_uid};

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Why the pool can find no free uid: the reasons of the refusal of a loan.
fn reasons(pool: &Pool) -> Vec<String> {
    match pool.lend("probe") {
        Err(AccountError::NoFreeUid { reasons, .. }) => reasons,
        other => panic!("a uid was free: {:?}", other.map(|lent| lent.account)),
    }
}

/// Asserts the shape of the account the pool makes: a name of the pool's own prefix and at
/// most 31 bytes, the uid of the range, the pool's group, a home `<homes>/<name>` that
/// exists; and that what is recorded comes before what is made: `made` is in the ledger
/// before the home exists, and `ready` is not there until the record is made.
#[test]
fn an_account_is_recorded_before_it_is_made_and_has_the_shape_of_the_design() {
    let rig = Rig::new("shape", 3, 1);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ledger = rig.dir.join("state/accounts");
    for op in ["make_home", "create_account"] {
        let (seen, ledger) = (Arc::clone(&seen), ledger.clone());
        rig.fake.on(op, move || {
            seen.lock().unwrap().push((op, read(&ledger)));
        });
    }
    let _pool = rig.open();
    let made = format!("made kbf-acct-1 {}\n", me());
    assert_eq!(
        *seen.lock().unwrap(),
        [("make_home", made.clone()), ("create_account", made)]
    );
    let calls = rig.fake.take_calls();
    let home = calls
        .iter()
        .position(|c| c == "make_home kbf-acct-1")
        .unwrap();
    let record = calls
        .iter()
        .position(|c| c == "create_account kbf-acct-1")
        .unwrap();
    assert!(
        home < record,
        "the home is made before the record: {calls:?}"
    );

    let users = rig.fake.state().users.clone();
    let user = &users["kbf-acct-1"];
    assert!(user.name.starts_with(PREFIX) && user.name.len() <= MAX_NAME);
    assert_eq!((user.uid, user.gid), (me(), my_gid()));
    assert_eq!(user.home, rig.home(1));
    assert!(rig.home(1).is_dir());
}

/// Asserts that a uid comes after the last one given and wraps, rather than the lowest free:
/// a uid is used again as late as the range allows, and not until the account that had it is
/// gone.
#[test]
fn the_next_uid_comes_after_the_last_one_given_and_wraps() {
    let rig = Rig::new("uid-order", 4, 0);
    let pool = rig.open();
    let uids: Vec<u32> = (1..=2)
        .map(|n| pool.lend(&format!("job-{n}")).unwrap().account.uid)
        .collect();
    assert_eq!(uids, [me(), me() + 1]);
    // The first account's uid is free again; the next uid is still the one after the last.
    pool.retire(pool.account(1, me())).unwrap();
    assert_eq!(pool.lend("job-3").unwrap().account.uid, me() + 2);
    assert_eq!(pool.lend("job-4").unwrap().account.uid, me() + 3);
    // Wrapped: the first uid, free again, and not the second, which an account holds.
    assert_eq!(pool.lend("job-5").unwrap().account.uid, me());
    let (b, c, d, a) = (me() + 1, me() + 2, me() + 3, me());
    assert_eq!(
        pool.lend("job-6").unwrap_err().to_string(),
        format!(
            "no free uid in {a}-{d}: {b}: held by kbf-acct-2; {c}: held by kbf-acct-3; \
             {d}: held by kbf-acct-4; {a}: held by kbf-acct-5"
        )
    );
}

/// Asserts that an account is made with the number after any the ledger has ever seen, so a
/// name is never used again, across restarts and after the account was deleted.
#[test]
fn a_name_is_never_used_again_across_restarts() {
    let rig = Rig::new("names", 6, 1);
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap().account;
    pool.retire(first).unwrap();
    drop(pool);
    let pool = rig.open();
    let second = pool.lend("job-2").unwrap().account;
    drop(pool);
    let pool = rig.open();
    let names: Vec<String> = rig
        .lines()
        .iter()
        .filter_map(|line| {
            line.strip_prefix("made ")?
                .split(' ')
                .next()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(names, ["kbf-acct-1", "kbf-acct-2", "kbf-acct-3"]);
    assert_eq!(second.name, "kbf-acct-2");
    assert_eq!(pool.ready_count(), 1);
}

/// Asserts that a uid is passed over when a record has it or a process runs as it that the
/// kill rounds do not end, each for its own reason (the process named by its pid), and that
/// the first free uid after the busy ones is taken.
#[test]
fn a_uid_with_a_record_or_a_process_is_passed_over() {
    let rig = Rig::new("busy-host", 1, 0);
    rig.fake.add_user("someone", me(), 20, &rig.settings.homes);
    let pool = rig.open();
    assert_eq!(reasons(&pool), [format!("{}: a user record has it", me())]);

    let rig = rig.next("busy-process", 1, 0);
    rig.fake.add_process(me(), 500, "daemon");
    rig.fake.state().immortal.insert(me());
    let pool = rig.open();
    assert_eq!(
        reasons(&pool),
        [format!(
            "{u}: 1 processes of uid {u} remain after 10 rounds: 500 (daemon)",
            u = me()
        )]
    );

    let rig = rig.next("busy-then-free", 3, 0);
    rig.fake.add_user("someone", me(), 20, &rig.settings.homes);
    rig.fake.add_process(me() + 1, 500, "daemon");
    rig.fake.state().immortal.insert(me() + 1);
    let pool = rig.open();
    assert_eq!(pool.lend("job-1").unwrap().account.uid, me() + 2);
}

/// Asserts, with the test's own uid as the busy one, that something left in a per-user folder
/// below its top-level directories, a non-directory at its top level, an entry in a shared
/// place, and an entry of the places of schedules each make the uid busy and name what is
/// owned by it; and that the folder the system made and keeps (its top-level directories,
/// empty) makes none busy.
#[test]
fn a_uid_that_owns_files_is_busy() {
    let rig = Rig::new("owns-files", 1, 0);
    let pool = rig.open();
    let uid = me();
    let folder = rig.folder("ab", "hash");
    for leftover in ["T/left", "stray"] {
        let path = folder.join(leftover);
        std::fs::write(&path, "x").unwrap();
        assert_eq!(
            reasons(&pool),
            [format!(
                "{uid}: {} is left in its per-user folder",
                path.display()
            )]
        );
        std::fs::remove_file(path).unwrap();
    }
    for (place, entry) in [("Shared", "left"), ("tmp", "left"), ("at/jobs", "job")] {
        let path = rig.dir.join(place).join(entry);
        std::fs::write(&path, "x").unwrap();
        assert_eq!(
            reasons(&pool),
            [format!("{uid}: {} is owned by it", path.display())]
        );
        std::fs::remove_file(path).unwrap();
    }
    let plist = rig.dir.join(format!("launchd/disabled.{uid}.plist"));
    std::fs::write(&plist, "x").unwrap();
    assert_eq!(
        reasons(&pool),
        [format!("{uid}: {} is owned by it", plist.display())]
    );
    std::fs::remove_file(&plist).unwrap();
    // Nothing is left but the folder as the system keeps it: the uid is free, and an account
    // is made.
    assert!(folder.join("T").is_dir());
    assert_eq!(pool.lend("job-1").unwrap().account.uid, uid);
}

/// Asserts that a slot whose deleted record the lookup cache (`getpw*`) still answers for, for
/// a counted number of lookups after the retirement, is free: the refill after the
/// retirement makes the next account on the same uid and a lend gets it, and the log says the
/// cache answered. Catches a free check that reads a cached answer as a record, which fails
/// the next job of a one-slot pool right after a retirement.
#[test]
fn a_slot_only_the_lookup_cache_still_names_is_free_again() {
    let rig = Rig::new("free-cache", 1, 1);
    rig.fake.state().getpw_lingers = 6;
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap().account;
    let uid = first.uid;
    pool.retire(first).unwrap();
    assert_eq!(pool.ready_count(), 1, "the refill made the next account");
    let next = pool.lend("job-2").unwrap();
    assert_eq!((next.account.uid, next.made_now), (uid, false));
    let log = rig.log.text();
    assert!(
        log.contains(
            "the directory has no record of the uid; a lookup still answers from its cache"
        ),
        "{log}"
    );
}

/// Asserts that an error in looking at a uid is not "free": the uid is passed over and the
/// refusal names why, for each way of looking.
#[test]
fn an_error_in_looking_at_a_uid_is_not_free() {
    let rig = Rig::new("look-errors", 1, 0);
    let pool = rig.open();
    let uid = me();
    let why = |pool: &Pool| reasons(pool).join("; ");
    rig.fake.fail(&format!("uid_taken {uid}"));
    assert_eq!(
        why(&pool),
        format!("{uid}: looking up its record: uid_taken {uid} failed")
    );
    rig.fake.heal(&format!("uid_taken {uid}"));
    rig.fake.fail(&format!("uid_in_directory {uid}"));
    assert_eq!(
        why(&pool),
        format!("{uid}: looking up its record: uid_in_directory {uid} failed")
    );
    rig.fake.heal(&format!("uid_in_directory {uid}"));
    rig.fake.fail(&format!("processes {uid}"));
    assert_eq!(
        why(&pool),
        format!("{uid}: listing its processes: processes {uid} failed")
    );
    rig.fake.heal(&format!("processes {uid}"));

    let folders = rig.dir.join("folders");
    std::fs::remove_dir(&folders).unwrap();
    std::fs::write(&folders, "").unwrap();
    let text = why(&pool);
    assert!(
        text.starts_with(&format!(
            "{uid}: searching the per-user folders: {}: ",
            folders.display()
        )),
        "{text}"
    );
    std::fs::remove_file(&folders).unwrap();
    std::fs::create_dir(&folders).unwrap();

    let shared = rig.dir.join("Shared");
    std::fs::remove_dir(&shared).unwrap();
    std::fs::write(&shared, "").unwrap();
    let text = why(&pool);
    assert!(
        text.starts_with(&format!(
            "{uid}: looking in the shared places: {}: ",
            shared.display()
        )),
        "{text}"
    );
    std::fs::remove_file(&shared).unwrap();
    std::fs::create_dir(&shared).unwrap();
    assert_eq!(pool.lend("job-1").unwrap().account.uid, uid);
}

/// Asserts that a refusal for want of a free uid names up to eight uids and says how many
/// more.
#[test]
fn a_refusal_for_want_of_a_free_uid_names_eight_and_counts_the_rest() {
    let rig = Rig::new("none-free", 12, 0);
    let pool = rig.open();
    for n in 0..12 {
        rig.fake
            .add_user(&format!("user-{n}"), me() + n, 20, &rig.settings.homes);
    }
    let Err(AccountError::NoFreeUid { range, reasons }) = pool.lend("job-1") else {
        panic!("a uid was free");
    };
    assert_eq!(range, format!("{}-{}", me(), me() + 11));
    assert_eq!(reasons.len(), 9);
    assert_eq!(reasons[0], format!("{}: a user record has it", me()));
    assert_eq!(reasons[8], "and 4 more");
    let text = pool.lend("job-1").unwrap_err().to_string();
    assert!(
        text.starts_with(&format!("no free uid in {}-{}: ", me(), me() + 11)),
        "{text}"
    );
    assert!(text.ends_with("; and 4 more"), "{text}");
}

/// Asserts what a failure while making leaves: at the home or at the record, what was made
/// is deleted and shown gone, the ledger says `gone`, the error names the step and says so,
/// and the next account is made under the next number.
#[test]
fn a_failure_while_making_deletes_what_was_made() {
    for (failing, step, why) in [
        ("make_home kbf-acct-1", Step::Home, "making"),
        (
            "create_account kbf-acct-1",
            Step::Record,
            "creating the record",
        ),
    ] {
        let rig = Rig::new("make-fails", 3, 1);
        rig.fake.fail(failing);
        let error = rig.refused();
        let AccountError::Make {
            account,
            step: at,
            why: text,
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!((account.as_str(), *at), ("kbf-acct-1", step), "{failing}");
        assert!(text.contains(why), "{text}");
        assert!(
            text.ends_with("; what was made was deleted and shown gone"),
            "{text}"
        );
        assert_eq!(
            rig.lines(),
            [
                format!("made kbf-acct-1 {}", me()),
                "gone kbf-acct-1".to_owned()
            ]
        );
        assert!(rig.fake.state().users.is_empty());
        assert!(!rig.home(1).exists(), "the half-made home is removed");

        rig.fake.heal(failing);
        let pool = rig.open();
        assert_eq!(pool.ready_count(), 1);
        assert!(
            rig.lines()
                .contains(&format!("made kbf-acct-2 {}", me() + 1)),
            "the number and the uid are not reused"
        );
    }
}

/// Asserts that a home that is already there refuses the account (a new account takes
/// nothing of what another left) and is removed with the failed account.
#[test]
fn a_home_that_exists_refuses_the_account() {
    let rig = Rig::new("home-exists", 3, 1);
    std::fs::create_dir(rig.home(1)).unwrap();
    std::fs::write(rig.home(1).join("leftover"), "x").unwrap();
    let AccountError::Make { step, why, .. } = rig.refused() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Home);
    assert!(
        why.contains("File exists"),
        "the refusal does not say what the test expects"
    );
    assert!(!rig.home(1).exists());
    assert_eq!(rig.open().ready_count(), 1);
}

/// Asserts that when what was made cannot be deleted, the account is held: the ledger says
/// `held` at the step, the error says it did not complete, the uid is passed over from then
/// on, and a retirement tries it again.
#[test]
fn a_failed_making_that_cannot_be_deleted_is_held() {
    let rig = Rig::new("make-held", 2, 1);
    rig.fake.fail("create_account kbf-acct-1");
    rig.fake.fail("delete_account kbf-acct-1");
    let AccountError::Make { account, step, why } = rig.refused() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Record));
    assert!(
        why.ends_with("deleting what was made did not complete, at record: deleting the record kbf-acct-1: delete_account kbf-acct-1 failed"),
        "the refusal does not say what the test expects"
    );
    assert_eq!(
        rig.lines(),
        [
            format!("made kbf-acct-1 {}", me()),
            "held kbf-acct-1 record".to_owned()
        ]
    );

    rig.fake.heal("create_account kbf-acct-1");
    rig.fake.heal("delete_account kbf-acct-1");
    let pool = rig.open();
    // The open's sweep deleted it, and the uid is free again.
    assert!(pool.held().is_empty());
    assert!(rig.lines().contains(&"gone kbf-acct-1".to_owned()));
}

/// Asserts that a ledger that cannot take the `made` record stops the making before
/// anything exists, and one that fails after the record is made deletes it, though the
/// deletion cannot be recorded either: the error says where.
#[test]
fn a_ledger_that_fails_while_making_is_named() {
    let rig = Rig::new("make-ledger", 3, 0);
    let pool = Arc::new(rig.open());
    let path = rig.dir.join("state/accounts");
    pool.ledger().fail_writes(&path);
    let error = pool.lend("job-1").unwrap_err();
    let AccountError::Make { account, step, why } = &error else {
        panic!("{error:?}");
    };
    assert_eq!((account.as_str(), *step), ("kbf-acct-1", Step::Ledger));
    assert!(
        why.starts_with("recording made: "),
        "the error does not say what the test expects"
    );
    assert!(
        rig.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("make_home"))
    );

    // A second pool over the same files, whose ledger fails after the record is made.
    let rig = rig.next("make-ledger-late", 3, 0);
    let pool = Arc::new(rig.open());
    let broken = Arc::clone(&pool);
    let path = rig.dir.join("state/accounts");
    rig.fake
        .on("create_account", move || broken.ledger().fail_writes(&path));
    let AccountError::Make { account, step, why } = pool.lend("job-1").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Ledger));
    assert!(
        why.starts_with("recording lent: "),
        "the refusal does not say what the test expects"
    );
    assert!(
        why.contains("deleting what was made did not complete, at ledger: recording gone: "),
        "the refusal does not say what the test expects"
    );
    // The account was deleted all the same: only the ledger does not know.
    assert!(rig.fake.state().users.is_empty());
    assert_eq!(rig.lines(), [format!("made kbf-acct-1 {}", me())]);
}

/// Asserts that an account made for the pool (a refill) whose `ready` cannot be recorded is
/// deleted, and the error says the failed record was `ready`.
#[test]
fn a_refill_whose_ready_cannot_be_recorded_is_named_and_deleted() {
    let rig = Rig::new("refill-ledger", 3, 0);
    let pool = Arc::new(rig.open());
    let broken = Arc::clone(&pool);
    let path = rig.dir.join("state/accounts");
    rig.fake
        .on("create_account", move || broken.ledger().fail_writes(&path));
    let AccountError::Make { account, step, why } = pool.make(None).unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Ledger));
    assert!(
        why.starts_with("recording ready: "),
        "the refusal does not say what the test expects"
    );
    assert!(rig.fake.state().users.is_empty());
}

/// Asserts that launchd plays no part in the choice of a uid: a domain that would print
/// does not make a uid busy (only the records, processes and files do), the uid is chosen,
/// and the choice asks launchd nothing (a question about a uid with no account would make
/// its user domain).
#[test]
fn a_domain_that_prints_does_not_make_a_uid_busy() {
    let rig = Rig::new("domain-not-busy", 1, 0);
    let pool = rig.open();
    rig.fake
        .state()
        .print
        .insert(format!("gui/{}", me()), Print::Stays);
    rig.fake.take_calls();
    assert_eq!(pool.lend("job-1").unwrap().account.uid, me());
    let calls = rig.fake.take_calls();
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("launchctl_print") && !call.starts_with("bootout")),
        "{calls:?}"
    );
}

/// Asserts that the record is made with the slot's `GeneratedUID`: the account is lent with
/// it, the record the host holds has it, and the next account of the same slot has the same
/// one under another name, while another slot has another.
#[test]
fn a_record_is_made_with_its_slots_generated_uid() {
    let rig = Rig::new("guid-made", 2, 0);
    let pool = rig.open();
    let first = pool.lend("job-1").unwrap().account;
    assert_eq!(first.generated_uid, generated_uid(me()));
    assert_eq!(
        rig.fake.state().users["kbf-acct-1"].generated_uid,
        "4B424641-4343-5400-8000-".to_owned() + &format!("{:012}", me())
    );
    let second = pool.lend("job-2").unwrap().account;
    assert_eq!(second.generated_uid, generated_uid(me() + 1));
    assert_ne!(first.generated_uid, second.generated_uid);

    // The same slot again: the same identifier, another name.
    pool.retire(first).unwrap();
    let rig = rig.next("guid-same-slot", 1, 0);
    let pool = rig.open();
    let one = pool.lend("job-1").unwrap().account;
    pool.retire(one.clone()).unwrap();
    let again = pool.lend("job-2").unwrap().account;
    assert_eq!((again.uid, again.name.as_str()), (one.uid, "kbf-acct-2"));
    assert_eq!(again.generated_uid, one.generated_uid);
    assert_eq!(
        rig.fake.state().users["kbf-acct-2"].generated_uid,
        generated_uid(me())
    );
}

/// Asserts that a record that comes out with another `GeneratedUID` than the slot's fails the
/// making at `record`, naming both, and what was made is deleted and shown gone; and that a
/// record whose `GeneratedUID` cannot be read, or that is not there to be read, fails it too.
#[test]
fn a_record_that_does_not_have_the_slots_generated_uid_fails_the_making() {
    let other = "00000000-0000-4000-8000-000000000001";
    let rig = Rig::new("guid-wrong", 3, 1);
    rig.fake.state().made_with_guid = Some(other.to_owned());
    let AccountError::Make { account, step, why } = rig.refused() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Record));
    assert!(
        why.starts_with(&format!(
            "the record's GeneratedUID is {other}, not the slot's {}; ",
            generated_uid(me())
        )),
        "the refusal does not say what the test expects"
    );
    assert!(
        why.ends_with("what was made was deleted and shown gone"),
        "the refusal does not say what the test expects"
    );
    assert_eq!(
        rig.lines(),
        [
            format!("made kbf-acct-1 {}", me()),
            "gone kbf-acct-1".to_owned()
        ]
    );
    assert!(rig.fake.state().users.is_empty());
    assert!(!rig.home(1).exists());

    let rig = rig.next("guid-unreadable", 3, 1);
    rig.fake.fail("record_guid kbf-acct-1");
    let AccountError::Make { step, why, .. } = rig.refused() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Record);
    assert!(
        why.starts_with("reading the GeneratedUID of the record: record_guid kbf-acct-1 failed; "),
        "the refusal does not say what the test expects"
    );
    assert!(rig.fake.state().users.is_empty());

    let rig = rig.next("guid-vanished", 3, 1);
    let fake = rig.fake.clone();
    rig.fake
        .on("create_account", move || fake.state().users.clear());
    let AccountError::Make { step, why, .. } = rig.refused() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Record);
    assert!(
        why.starts_with("the record is not there after it was made; "),
        "the refusal does not say what the test expects"
    );
}

/// Asserts that a slot whose `GeneratedUID` another record has is not free, whatever that
/// record's name or uid, and that a failed look for it is a reason too: the slot's folder is
/// the same one for the same pair, so an account made beside such a record would share it.
#[test]
fn a_slot_whose_generated_uid_another_record_has_is_not_free() {
    let rig = Rig::new("guid-taken", 1, 0);
    let pool = rig.open();
    let uid = me();
    rig.fake
        .add_user("someone", uid + 5000, 20, &rig.settings.homes);
    rig.fake
        .state()
        .users
        .get_mut("someone")
        .unwrap()
        .generated_uid = generated_uid(uid);
    assert_eq!(
        reasons(&pool),
        [format!("{uid}: a user record has its GeneratedUID")]
    );
    assert!(
        rig.fake
            .take_calls()
            .contains(&format!("guid_taken {}", generated_uid(uid)))
    );
    rig.fake.state().users.remove("someone");

    rig.fake.fail(&format!("guid_taken {}", generated_uid(uid)));
    assert_eq!(
        reasons(&pool),
        [format!(
            "{uid}: looking up its GeneratedUID: guid_taken {} failed",
            generated_uid(uid)
        )]
    );
    rig.fake.heal(&format!("guid_taken {}", generated_uid(uid)));
    assert_eq!(pool.lend("job-1").unwrap().account.uid, uid);
}

/// Asserts that the slot's per-user folder is emptied again once `made` is recorded, before
/// the home and the record: something that appeared after the slot was judged free (a hook
/// runs just before `made` is taken) is gone when the account is lent.
#[test]
fn the_slots_folder_is_emptied_again_before_the_account_is_made() {
    let rig = Rig::new("folder-again", 1, 0);
    let pool = rig.open();
    let folder = rig.folder("ab", "hash");
    let left = folder.join("T/left");
    let leftover = left.clone();
    *pool.before_record.lock().unwrap() = Some(Box::new(move |record| {
        if matches!(record, Record::Made(..)) {
            std::fs::write(&leftover, "x").unwrap();
        }
    }));
    let lent = pool.lend("job-1").unwrap();
    assert_eq!(lent.account.uid, me());
    assert!(!left.exists(), "the leftover was removed");
    assert!(folder.join("T").is_dir(), "the folder is kept");
    let calls = rig.fake.take_calls();
    assert!(
        calls.contains(&"make_home kbf-acct-1".to_owned()),
        "{calls:?}"
    );
}

/// Asserts that a folder that cannot be emptied once `made` is recorded stops the making at
/// `folder`, before the home and the record are made; what was made (nothing) cannot be
/// shown gone either, so the account is held at `folder`, and the error says so.
#[test]
fn a_folder_that_cannot_be_emptied_before_the_making_stops_it_at_folder() {
    let rig = Rig::new("folder-stuck", 1, 0);
    let pool = rig.open();
    rig.folder("ab", "hash");
    let folders = rig.dir.join("folders");
    *pool.before_record.lock().unwrap() = Some(Box::new(move |record| {
        if matches!(record, Record::Made(..)) {
            std::fs::remove_dir_all(&folders).unwrap();
            std::fs::write(&folders, "").unwrap();
        }
    }));
    let AccountError::Make { account, step, why } = pool.lend("job-1").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Folder));
    assert!(
        why.starts_with("emptying its per-user folders: "),
        "the refusal does not say what the test expects"
    );
    assert!(
        why.contains("deleting what was made did not complete, at folder: "),
        "the refusal does not say what the test expects"
    );
    assert_eq!(
        rig.lines(),
        [
            format!("made kbf-acct-1 {}", me()),
            "held kbf-acct-1 folder".to_owned()
        ]
    );
    assert!(
        rig.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("make_home") && !call.starts_with("create_account"))
    );
}

/// Asserts that a process of a free slot (no account of the ledger, no record), such as the
/// `distnoted` macOS starts by itself for a uid, is ended by the kill rounds and the slot is
/// then taken; the fake starts it again after the first kill, and the second round ends it.
/// Catches a check that refuses the slot for it.
#[test]
fn a_stray_process_of_a_free_slot_is_ended_and_the_slot_taken() {
    let rig = Rig::new("stray-free", 1, 0);
    let pool = rig.open();
    let u = me();
    rig.fake.add_process(u, 5071, "distnoted");
    let (fake, mut kills) = (Arc::clone(&rig.fake), 0);
    rig.fake.on(&format!("kill_all {u}"), move || {
        kills += 1;
        if kills == 1 {
            fake.add_process(u, 5072, "distnoted");
        }
    });
    let lent = pool.lend("job-1").unwrap();
    assert_eq!(lent.account.uid, u);
    assert!(rig.fake.state().processes.is_empty());
    assert_eq!(
        rig.lines(),
        [
            format!("made kbf-acct-1 {u}"),
            "lent kbf-acct-1 job-1".to_owned()
        ]
    );
    let calls = rig.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == format!("kill_all {u}"))
            .count(),
        2
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("launchctl_print") && !call.starts_with("bootout")),
        "no account has the uid, so launchd is not asked about it: {calls:?}"
    );
    let log = rig.log.text();
    assert!(
        log.contains("ended the processes of a free slot") && log.contains("killed=[5071, 5072]"),
        "{log}"
    );
}

/// Asserts that the free check never kills the processes of a slot an account of the ledger
/// holds, while it does end those of a slot nobody holds: the held slot is refused as held and
/// its job's process lives on; the other slot's process, which survives every round, is named
/// by its pid.
#[test]
fn a_process_of_a_held_slot_is_never_killed_by_the_free_check() {
    let rig = Rig::new("stray-held", 2, 0);
    let pool = rig.open();
    let (held, free) = (me(), me() + 1);
    assert_eq!(pool.lend("job-1").unwrap().account.uid, held);
    rig.fake.add_process(held, 40, "build");
    rig.fake.add_process(free, 41, "distnoted");
    rig.fake.state().immortal.insert(free);
    rig.fake.take_calls();
    assert_eq!(
        reasons(&pool),
        [
            format!("{free}: 1 processes of uid {free} remain after 10 rounds: 41 (distnoted)"),
            format!("{held}: held by kbf-acct-1"),
        ]
    );
    let calls = rig.fake.take_calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == format!("kill_all {free}"))
            .count(),
        10
    );
    assert!(
        calls.iter().all(|call| *call != format!("kill_all {held}")
            && *call != format!("bootout gui/{held}")
            && *call != format!("bootout user/{held}")),
        "{calls:?}"
    );
    assert_eq!(rig.fake.state().processes[&held].len(), 1);
}

/// Asserts that a making that fails after its record was made, while a process macOS started
/// for the new record runs as its uid, ends that process before it deletes what was made, and
/// is shown gone: `gone`, not `held`. Catches a clean-up that deletes without killing first.
#[test]
fn a_failed_making_ends_a_stray_process_and_is_shown_gone() {
    let rig = Rig::new("unmake-stray", 1, 0);
    let pool = rig.open();
    let u = me();
    let fake = Arc::clone(&rig.fake);
    rig.fake.on("create_account", move || {
        fake.add_process(u, 5071, "distnoted");
    });
    let wrong = "00000000-0000-0000-0000-000000000001";
    rig.fake.state().made_with_guid = Some(wrong.to_owned());
    let AccountError::Make { account, step, why } = pool.lend("job-1").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!((account.as_str(), step), ("kbf-acct-1", Step::Record));
    assert_eq!(
        why,
        format!(
            "the record's GeneratedUID is {wrong}, not the slot's {}; \
             what was made was deleted and shown gone",
            generated_uid(u)
        )
    );
    assert_eq!(
        rig.lines(),
        [format!("made kbf-acct-1 {u}"), "gone kbf-acct-1".to_owned()]
    );
    assert!(rig.fake.state().processes.is_empty());
    assert!(rig.fake.state().users.is_empty());
}

/// Asserts that a failed making never deletes what another ledger on the host made under the
/// same name: with a record `kbf-acct-1` of a uid outside the range (the other ledger's) and
/// its home there, the making fails at the home, the record and the home stay, and the account
/// is held at the record.
#[test]
fn a_failed_making_keeps_a_record_and_a_home_of_the_name_that_are_anothers() {
    let rig = Rig::new("make-theirs", 3, 0);
    let theirs = me() + 10;
    rig.fake
        .add_user("kbf-acct-1", theirs, my_gid(), &rig.settings.homes);
    std::fs::create_dir(rig.home(1)).unwrap();
    std::fs::write(rig.home(1).join("theirs"), "x").unwrap();
    let pool = rig.open();
    let AccountError::Make { step, why, .. } = pool.lend("job").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Home);
    assert!(
        why.contains(&format!(
            "the record kbf-acct-1 has uid {theirs}, not {}: another's, not deleted",
            me()
        )),
        "the refusal names the other's record"
    );
    assert!(
        rig.fake.state().users.contains_key("kbf-acct-1"),
        "the other's record stays"
    );
    assert!(
        rig.home(1).join("theirs").exists(),
        "the other's home stays"
    );
    assert!(rig.lines().contains(&"held kbf-acct-1 record".to_owned()));
}

/// Asserts the other two ways a failed making leaves a name alone. A home of the name that
/// another uid owns (here the test's own uid, while the account has the next one) stays, and
/// the account is held at the home, naming the owner. A record that cannot be looked up holds
/// the account at the record, and nothing is deleted on a failed look.
#[test]
fn a_failed_making_holds_on_another_uids_home_or_on_a_record_it_cannot_look_up() {
    let rig = Rig::new("make-theirs-home", 2, 0);
    let pool = rig.open();
    assert_eq!(pool.lend("job-1").unwrap().account.uid, me());
    std::fs::create_dir(rig.home(2)).unwrap();
    std::fs::write(rig.home(2).join("theirs"), "x").unwrap();
    let AccountError::Make { step, why, .. } = pool.lend("job-2").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Home);
    let owned = format!(
        "deleting what was made did not complete, at home: {} is owned by uid {}, not {}: \
         another's, not removed",
        rig.home(2).display(),
        me(),
        me() + 1
    );
    assert!(why.ends_with(&owned), "the refusal names the other's home");
    assert!(rig.home(2).join("theirs").exists());
    assert!(rig.lines().contains(&"held kbf-acct-2 home".to_owned()));

    let rig = rig.next("make-unlookable", 1, 0);
    let pool = rig.open();
    rig.fake.fail("create_account kbf-acct-1");
    rig.fake.fail("account kbf-acct-1");
    let AccountError::Make { step, why, .. } = pool.lend("job-1").unwrap_err() else {
        panic!("not a making error");
    };
    assert_eq!(step, Step::Record);
    assert!(
        why.ends_with(
            "deleting what was made did not complete, at record: looking up the record \
             kbf-acct-1: account kbf-acct-1 failed"
        ),
        "the refusal names the failed look"
    );
    assert!(rig.lines().contains(&"held kbf-acct-1 record".to_owned()));
    assert!(
        rig.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("delete_account")),
        "nothing is deleted on a failed look"
    );
}
