//! Tests of the proof that a deleted account is gone: each item, failing in its own way,
//! after the deletion that is supposed to have removed it. Each failure is made by a hook that
//! runs once the deletion has done its work, so what it plants is what the proof must see.

use super::*;
use crate::mac_accounts::tests::fake::Print;
use crate::mac_accounts::tests::rig::{Rig, me};
use crate::mac_accounts::{Held, Pool};

/// A lent account over a new rig of `base`'s test, whose host runs `hook` after the call
/// `key`; the account is retired, and the failure is returned with the rig and the pool.
fn failing(
    base: &Rig,
    name: &str,
    key: &str,
    hook: impl FnOnce(&Rig) -> Box<dyn FnMut() + Send>,
) -> (Rig, Pool, Held) {
    let rig = base.next(name, 4, 0);
    let pool = rig.open();
    let account = pool.lend("job-1").unwrap().account;
    let hook = hook(&rig);
    rig.fake.on(key, hook);
    let held = match pool.retire(account) {
        Ok(retired) => panic!("{name}: the account was shown gone: {retired:?}"),
        Err(held) => held,
    };
    // Gone is written only when the proof holds.
    assert!(
        !rig.ledger().contains("gone kbf-acct-1"),
        "{name}: {}",
        rig.ledger()
    );
    (rig, pool, held)
}

/// What a proof failure said: its item and its text.
fn said(held: &Held, item: Gone) -> String {
    assert_eq!(held.step, Step::Proof(item), "{held}");
    assert_eq!(
        held.to_string(),
        format!(
            "account kbf-acct-1 (uid {}) is held at proof-{item}: {}",
            me(),
            held.why
        )
    );
    held.why.clone()
}

/// Asserts the record item: a record by the name that remains, a record by another name that
/// has the uid, and a lookup that fails, each fail the proof at `record`; and an error in
/// looking is never read as gone.
#[test]
fn a_record_that_remains_or_cannot_be_looked_up_fails_the_proof() {
    let base = Rig::new("proof-record", 1, 0);
    let uid = me();
    let (rig, _pool, held) = failing(&base, "remains", "delete_account", |rig| {
        let (fake, homes) = (rig.fake.clone(), rig.settings.homes.clone());
        Box::new(move || fake.add_user("kbf-acct-1", uid, 20, &homes))
    });
    assert_eq!(
        said(&held, Gone::Record),
        "a record named kbf-acct-1 remains"
    );
    assert_eq!(rig.lines().last().unwrap(), "held kbf-acct-1 proof-record");

    let (_rig, _pool, held) = failing(&base, "uid-taken", "delete_account", |rig| {
        let (fake, homes) = (rig.fake.clone(), rig.settings.homes.clone());
        Box::new(move || fake.add_user("intruder", uid, 20, &homes))
    });
    assert_eq!(said(&held, Gone::Record), format!("a record has uid {uid}"));

    let (_rig, _pool, held) = failing(&base, "lookup-fails", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.fail("account kbf-acct-1"))
    });
    assert_eq!(
        said(&held, Gone::Record),
        "looking up kbf-acct-1: account kbf-acct-1 failed"
    );

    let (_rig, _pool, held) = failing(&base, "uid-lookup-fails", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.fail(&format!("uid_taken {uid}")))
    });
    assert_eq!(
        said(&held, Gone::Record),
        format!("looking up uid {uid}: uid_taken {uid} failed")
    );

    let (_rig, _pool, held) = failing(&base, "directory-fails", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.fail("in_directory kbf-acct-1"))
    });
    assert_eq!(
        said(&held, Gone::Record),
        "looking up kbf-acct-1: in_directory kbf-acct-1 failed"
    );

    let (_rig, _pool, held) = failing(&base, "directory-uid-fails", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.fail(&format!("uid_in_directory {uid}")))
    });
    assert_eq!(
        said(&held, Gone::Record),
        format!("looking up uid {uid}: uid_in_directory {uid} failed")
    );
}

/// Asserts that a record the directory deleted, which the lookup cache (`getpw*`) still
/// answers for during the next two lookups (counted, not timed), does not hold the account:
/// the directory decides, the retirement records `gone`, the proof spent both of those
/// lookups (by name and by uid) and the log says the cache answered. Catches a proof that
/// reads a cached answer as a record that remains.
#[test]
fn a_record_only_the_lookup_cache_still_answers_for_is_gone() {
    let rig = Rig::new("proof-cache", 2, 0);
    rig.fake.state().getpw_lingers = 2;
    let pool = rig.open();
    let account = pool.lend("job-1").unwrap().account;
    let retired = pool.retire(account).unwrap();
    assert!(retired.gone.record);
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-1");
    assert_eq!(rig.fake.state().cache_answers, 2);
    assert!(
        rig.fake.state().cached.is_empty(),
        "both lookups were spent"
    );
    let log = rig.log.text();
    assert!(
        log.contains(
            "the directory has no record of the account; a lookup still answers from its cache"
        ),
        "{log}"
    );
}

/// Asserts the process item: a process that is there after the deletion, and a process
/// table that cannot be read, each fail the proof at `processes`.
#[test]
fn a_process_after_the_deletion_fails_the_proof() {
    let base = Rig::new("proof-processes", 1, 0);
    let uid = me();
    let (_rig, _pool, held) = failing(&base, "late-process", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.add_process(uid, 99, "late"))
    });
    assert_eq!(
        said(&held, Gone::Processes),
        format!("1 processes of uid {uid} remain after the account was deleted: 99 (late)")
    );

    let (_rig, _pool, held) = failing(&base, "listing-fails", "delete_account", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.fail(&format!("processes {uid}")))
    });
    assert_eq!(
        said(&held, Gone::Processes),
        format!("listing the processes of uid {uid}: processes {uid} failed")
    );
}

/// Asserts that once the record is deleted the proof asks launchd nothing, so a user domain
/// that would print does not hold the account and is never made again by a print: the
/// retirement is `gone`, and no `launchctl print` or `bootout` follows the deletion.
#[test]
fn the_proof_asks_launchd_nothing_after_the_deletion() {
    let rig = Rig::new("proof-no-launchd", 1, 0);
    let pool = rig.open();
    let account = pool.lend("job-1").unwrap().account;
    let uid = account.uid;
    let fake = rig.fake.clone();
    rig.fake.on("delete_account", move || {
        fake.state()
            .print
            .insert(format!("user/{uid}"), Print::Stays);
    });
    rig.fake.take_calls();
    pool.retire(account).unwrap();
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-1");
    let calls = rig.fake.take_calls();
    let deleted = calls
        .iter()
        .position(|call| call == "delete_account kbf-acct-1")
        .unwrap();
    assert!(
        calls[deleted..]
            .iter()
            .all(|call| !call.starts_with("launchctl_print") && !call.starts_with("bootout")),
        "{calls:?}"
    );
}

/// Asserts the home item: a home that exists, and a home path that cannot be looked at (an
/// error other than "no such file or directory" is not gone), each fail the proof at `home`.
#[test]
fn a_home_that_exists_or_cannot_be_looked_at_fails_the_proof() {
    let base = Rig::new("proof-home", 1, 0);
    let (rig, _pool, held) = failing(&base, "home-exists", "account kbf-acct-1", |rig| {
        let home = rig.home(1);
        Box::new(move || std::fs::create_dir(&home).unwrap())
    });
    assert_eq!(
        said(&held, Gone::Home),
        format!("the home {} exists", rig.home(1).display())
    );

    let (rig, _pool, held) = failing(&base, "home-unlookable", "account kbf-acct-1", |rig| {
        let homes = rig.settings.homes.clone();
        Box::new(move || {
            std::fs::remove_dir_all(&homes).unwrap();
            std::fs::write(&homes, "").unwrap();
        })
    });
    let text = said(&held, Gone::Home);
    assert_eq!(
        text,
        format!(
            "looking at {}: Not a directory (os error 20)",
            rig.home(1).display()
        )
    );
}

/// Asserts the folders item: something left below a top-level directory of a per-user folder,
/// or a non-directory at its top level, fails the proof at `folders` and names it; many are
/// named eight and counted; folders that cannot be searched fail it too; and a folder that is
/// emptied and kept, as the system leaves it, does not.
#[test]
fn a_per_user_folder_that_is_left_or_cannot_be_searched_fails_the_proof() {
    let base = Rig::new("proof-folders", 1, 0);
    let uid = me();
    let (rig, _pool, held) = failing(&base, "one-left", "account kbf-acct-1", |rig| {
        let hash = rig.folder("ab", "hash");
        Box::new(move || std::fs::write(hash.join("T/leftover"), "x").unwrap())
    });
    assert_eq!(
        said(&held, Gone::Folders),
        format!(
            "left in uid {uid}'s per-user folders: {}",
            rig.dir.join("folders/ab/hash/T/leftover").display()
        )
    );

    let (rig, _pool, held) = failing(&base, "top-level-file", "account kbf-acct-1", |rig| {
        let hash = rig.folder("ab", "hash");
        Box::new(move || std::fs::write(hash.join("stray"), "x").unwrap())
    });
    assert_eq!(
        said(&held, Gone::Folders),
        format!(
            "left in uid {uid}'s per-user folders: {}",
            rig.dir.join("folders/ab/hash/stray").display()
        )
    );

    let (_rig, _pool, held) = failing(&base, "ten-left", "account kbf-acct-1", |rig| {
        let top = rig.folder("ab", "hash").join("C");
        Box::new(move || {
            for n in 0..10 {
                std::fs::write(top.join(format!("f{n}")), "x").unwrap();
            }
        })
    });
    let text = said(&held, Gone::Folders);
    assert!(
        text.starts_with(&format!("left in uid {uid}'s per-user folders: ")),
        "{text}"
    );
    assert!(text.ends_with(", and 2 more"), "{text}");
    assert_eq!(text.matches("/C/f").count(), 8, "{text}");

    let (rig, _pool, held) = failing(&base, "not-searchable", "account kbf-acct-1", |rig| {
        let folders = rig.dir.join("folders");
        Box::new(move || {
            std::fs::remove_dir_all(&folders).unwrap();
            std::fs::write(&folders, "").unwrap();
        })
    });
    let text = said(&held, Gone::Folders);
    assert!(
        text.starts_with(&format!(
            "searching for uid {uid}'s per-user folders: {}: ",
            rig.dir.join("folders").display()
        )),
        "{text}"
    );
}

/// Asserts the places item: an entry the uid owns in a shared place, one named for the
/// account (its crontab), many named eight and counted, and a place that cannot be read fail
/// the proof at `places`.
#[test]
fn a_shared_place_with_something_left_or_unreadable_fails_the_proof() {
    let base = Rig::new("proof-places", 1, 0);
    let uid = me();
    let (rig, _pool, held) = failing(&base, "left", "account kbf-acct-1", |rig| {
        let (left, tab) = (
            rig.dir.join("Shared/left"),
            rig.dir.join("at/tabs/kbf-acct-1"),
        );
        Box::new(move || {
            std::fs::write(&left, "x").unwrap();
            std::fs::write(&tab, "x").unwrap();
        })
    });
    let text = said(&held, Gone::Places);
    assert!(
        text.starts_with(&format!("left of uid {uid}'s or kbf-acct-1's: ")),
        "{text}"
    );
    assert!(
        text.contains(&rig.dir.join("at/tabs/kbf-acct-1").display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&rig.dir.join("Shared/left").display().to_string()),
        "{text}"
    );

    let (_rig, _pool, held) = failing(&base, "ten-left", "account kbf-acct-1", |rig| {
        let tmp = rig.dir.join("tmp");
        Box::new(move || {
            for n in 0..10 {
                std::fs::write(tmp.join(format!("f{n}")), "x").unwrap();
            }
        })
    });
    let text = said(&held, Gone::Places);
    assert!(text.ends_with(", and 2 more"), "{text}");

    let (rig, _pool, held) = failing(&base, "unreadable", "account kbf-acct-1", |rig| {
        let shared = rig.dir.join("Shared");
        Box::new(move || {
            std::fs::remove_dir_all(&shared).unwrap();
            std::fs::write(&shared, "").unwrap();
        })
    });
    let text = said(&held, Gone::Places);
    assert!(
        text.starts_with(&format!(
            "looking in the shared places: {}: ",
            rig.dir.join("Shared").display()
        )),
        "{text}"
    );
}

/// Asserts the IPC item: an object that appears after the removal step, and a listing that
/// fails, fail the proof at `ipc`: the listing is made again after the deletion, whether or
/// not an earlier one found any.
#[test]
fn an_ipc_object_after_the_deletion_or_a_listing_that_fails_fails_the_proof() {
    let base = Rig::new("proof-ipc", 1, 0);
    let uid = me();
    let (_rig, _pool, held) = failing(&base, "object", "account kbf-acct-1", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.add_ipc('m', 100, &uid.to_string(), &uid.to_string()))
    });
    assert_eq!(
        said(&held, Gone::Ipc),
        format!("IPC objects of uid {uid} remain after the account was deleted: -m 100")
    );

    let (_rig, _pool, held) = failing(&base, "listing-fails", "account kbf-acct-1", |rig| {
        let fake = rig.fake.clone();
        Box::new(move || fake.state().ipcs_exits_nonzero = true)
    });
    assert_eq!(
        said(&held, Gone::Ipc),
        "ipcs failed: ipcs: kernel is not answering"
    );
}
