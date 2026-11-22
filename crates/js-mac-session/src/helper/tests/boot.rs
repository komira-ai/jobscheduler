//! The boot reset's account step: the crash points of a lease's account (recorded before it
//! is made, made, deleted before `deleted` is written), the accounts no lease of the ledger
//! names (a lost ledger, an account made by hand, the pool a release before this one kept),
//! what the grant decides, and what makes the boot reset fail.

use std::sync::{Arc, Mutex};

use super::*;
use crate::autologin::{Intent, write_intent};
use crate::lease::user_name;

/// A helper over a fresh state directory with six uids from the test's own, its boot reset
/// not run yet.
fn booting(name: &str) -> Rig {
    rig_with(name, UidRange::new(me(), me() + 5).unwrap(), None)
}

/// The same state directory again, as after a restart (a boot), with the accounts the host
/// had, and the boot reset not run yet.
fn rebooted(rig: &Rig, name: &str) -> Rig {
    let again = rig_with(name, rig.helper.settings.range, Some(rig.dir.clone()));
    again.host.state().users = rig.host.state().users.clone();
    again
}

/// Puts the account `name`, with `uid`, in the fake host's directory.
fn account(rig: &Rig, name: &str, uid: u32) {
    let user = NewUser {
        name: name.to_owned(),
        uid,
        gid: my_gid(),
        home: rig.dir.join("Users").join(name),
    };
    rig.host.state().users.insert(name.to_owned(), user);
}

/// What the ledger file holds.
fn ledger_text(rig: &Rig) -> String {
    std::fs::read_to_string(rig.dir.join("ledger")).unwrap()
}

fn names(rig: &Rig) -> Vec<String> {
    rig.host.state().users.keys().cloned().collect()
}

/// Catches: a lease recorded before its account was made that the boot reset leaves live
/// (its uid would be held for good) or fails on (there is no record to delete), or `deleted`
/// written without the account's deletion having been asked for and shown (the crash point
/// between `created` and the account).
#[test]
fn a_lease_with_no_account_is_deleted_as_one_with_none_left() {
    let rig = booting("boot-created");
    rig.helper
        .lock()
        .record_created(LeaseId::new(80, 1), me())
        .unwrap();
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(80, 1)]);
    assert_eq!(reset.held, Vec::<LeaseId>::new());
    assert_eq!(
        ledger_text(&rig),
        format!("created 80.1 {}\ndeleted 80.1\n", me())
    );
    assert!(log(&rig).contains(&"delete_user kbf-lease-80-1".to_owned()));
    assert!(rig.helper.lock().held_uids().is_empty());
}

/// Catches: an account and its home that a restart finds after a crash mid-job left behind
/// (the next lease would inherit them), or a deletion that is not recorded.
#[test]
fn an_account_made_for_a_lease_is_deleted_with_its_home_after_a_crash() {
    let first = booting("boot-account");
    first.helper.boot_reset(&[]).unwrap();
    assert_eq!(first.helper.user_create("81.1"), Ok(me()));
    let home = first.dir.join("Users/kbf-lease-81-1");
    assert!(home.is_dir());
    std::fs::write(home.join("work"), "x").unwrap();
    // The helper dies; the account and the home are there at the next start.
    let rig = rebooted(&first, "boot-account");
    drop(first);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(81, 1)]);
    assert_eq!(names(&rig), Vec::<String>::new());
    assert!(!home.exists());
    assert_eq!(
        ledger_text(&rig),
        format!("created 81.1 {}\ndeleted 81.1\n", me())
    );
}

/// Catches: a lease whose record was deleted before `deleted` was written left live (its uid
/// held for good), refused for the record that is not there, or recorded deleted without the
/// proof: the delete is repeated (a missing record is no error), the proof holds, and
/// `deleted` is written.
#[test]
fn a_record_deleted_before_deleted_was_written_is_finished_by_the_boot_reset() {
    let first = booting("boot-record-gone");
    first.helper.boot_reset(&[]).unwrap();
    assert_eq!(first.helper.user_create("82.1"), Ok(me()));
    // The record was deleted; the home was removed with it; `deleted` was not written.
    first.host.state().users.clear();
    std::fs::remove_dir_all(first.dir.join("Users/kbf-lease-82-1")).unwrap();
    let rig = rebooted(&first, "boot-record-gone");
    drop(first);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(82, 1)]);
    assert_eq!(
        ledger_text(&rig),
        format!("created 82.1 {}\ndeleted 82.1\n", me())
    );
}

/// Catches: an account named for a lease the ledger has not seen (a lost ledger) left in
/// the range (its uid would never be free), deleted without being recorded (the lease id
/// could be used again), or recorded and not ended.
#[test]
fn an_account_named_for_a_lease_the_ledger_does_not_know_is_recorded_and_deleted() {
    let rig = booting("boot-unknown");
    account(&rig, "kbf-lease-83-1", me() + 1);
    account(&rig, "kbf-lease-83-2", me() + 2);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(83, 1), LeaseId::new(83, 2)]);
    assert_eq!(names(&rig), Vec::<String>::new());
    assert_eq!(
        ledger_text(&rig),
        format!(
            "created 83.1 {}\ncreated 83.2 {}\ndeleted 83.1\ndeleted 83.2\n",
            me() + 1,
            me() + 2
        )
    );
    // The lease ids are used: no new lease can have them.
    assert!(
        rig.helper
            .user_create("83.1")
            .unwrap_err()
            .contains("was used before")
    );
}

/// Catches: an account of another name, or of a uid outside the range, deleted (the helper
/// acts on `kbf-lease-*` accounts of its own range and on nothing else), and an account that
/// is not named for a lease taken for one.
#[test]
fn an_account_outside_the_range_or_the_prefix_is_not_the_helpers_to_delete() {
    let rig = booting("boot-outside");
    account(&rig, "kbf-lease-84-1", me() + 50);
    account(&rig, "kbf-lease-pool-7", me() + 51);
    account(&rig, "kbf-vm", me() + 1);
    account(&rig, "staff-user", 501);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, Vec::<LeaseId>::new());
    assert_eq!(names(&rig).len(), 4);
    assert_eq!(ledger_text(&rig), "");
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("delete_user")),
        "{calls:#?}"
    );
}

/// Catches: an account named for a lease that the ledger has as deleted, one named for a
/// live lease with another uid, and one whose name is no lease's deleted as a lease would be
/// (the ledger is not changed for them: they have no lease id to record), or left in the
/// range; and the ledger's own lease, which has no account of its own uid, ended all the same.
#[test]
fn any_other_account_with_the_prefix_is_deleted_by_its_name_and_recorded_nowhere() {
    let rig = booting("boot-by-name");
    {
        let mut ledger = rig.helper.lock();
        ledger.record_created(LeaseId::new(85, 1), me()).unwrap();
        ledger.record_deleted(LeaseId::new(85, 1)).unwrap();
        ledger
            .record_created(LeaseId::new(85, 3), me() + 2)
            .unwrap();
    }
    // A name the ledger has as deleted; one that is a live lease's, with another uid; and
    // two that are no lease's.
    account(&rig, "kbf-lease-85-1", me() + 1);
    account(&rig, "kbf-lease-85-3", me() + 3);
    account(&rig, "kbf-lease-foo", me());
    account(&rig, "kbf-lease-3-4-5", me() + 4);
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(85, 3)]);
    assert_eq!(names(&rig), Vec::<String>::new());
    assert_eq!(
        ledger_text(&rig),
        format!(
            "created 85.1 {}\ndeleted 85.1\ncreated 85.3 {}\ndeleted 85.3\n",
            me(),
            me() + 2
        )
    );
}

/// Catches: the pool's accounts (`kbf-lease-pool-<n>`, which a release before this one made
/// and never deleted) left in the range, deleted before the pool file is removed (a
/// rollback would find slots whose accounts are gone), or deleted as a lease's (there is no
/// lease id to record); and a start with no pool file at all that fails.
#[test]
fn the_pool_s_accounts_are_deleted_by_name_after_its_file_is_removed() {
    let rig = booting("boot-pool");
    std::fs::write(rig.dir.join("pool"), "made 0 600\nfree 0\n").unwrap();
    account(&rig, "kbf-lease-pool-0", me());
    account(&rig, "kbf-lease-pool-1", me() + 1);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (pool_file, found) = (rig.dir.join("pool"), seen.clone());
    rig.host.state().after_delete = Some(Box::new(move |_| {
        found.lock().unwrap().push(pool_file.exists());
    }));
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(
        reset,
        Reset {
            kept: None,
            deleted: Vec::new(),
            held: Vec::new()
        }
    );
    assert_eq!(*seen.lock().unwrap(), [false, false]);
    assert_eq!(names(&rig), Vec::<String>::new());
    assert_eq!(ledger_text(&rig), "");
    assert!(!rig.dir.join("pool").exists());

    // No pool file: the accounts are deleted all the same.
    let rig = booting("boot-pool-no-file");
    account(&rig, "kbf-lease-pool-0", me());
    rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(names(&rig), Vec::<String>::new());
}

/// Catches: anything of the account step run without the grant (an account deleted on a Mac
/// whose helper cannot delete it, the pool file removed with its accounts left, a lease
/// recorded), whether the privacy directory refuses the listing or the listing fails some
/// other way: only the grant lets it begin, and the directory is not even listed.
#[test]
fn without_the_grant_none_of_it_runs() {
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
        let rig = booting("boot-no-grant");
        rig.host.state().privacy_dir = Some(kind);
        std::fs::write(rig.dir.join("pool"), "made 0 600\nfree 0\n").unwrap();
        account(&rig, "kbf-lease-pool-0", me());
        account(&rig, "kbf-lease-84-9", me() + 1);
        let reset = rig.helper.boot_reset(&[]).unwrap();
        assert_eq!(reset.deleted, Vec::<LeaseId>::new());
        assert_eq!(names(&rig).len(), 2, "{kind:?}");
        assert!(rig.dir.join("pool").exists());
        assert_eq!(ledger_text(&rig), "");
        let calls = log(&rig);
        assert!(
            !calls
                .iter()
                .any(|c| c == "lease_accounts" || c.starts_with("delete_user")),
            "{calls:#?}"
        );
    }
}

/// Catches: a boot reset that serves although an account no lease of the ledger names could
/// not be deleted and shown gone (a Mac where the grant does not let the helper delete would
/// start running per-job accounts), one that does not name the account and the step, one
/// that stops at the first, or one whose retry is refused.
#[test]
fn an_account_that_cannot_be_deleted_by_name_fails_the_boot_reset_naming_it() {
    let rig = booting("boot-fails");
    account(&rig, "kbf-lease-pool-3", me() + 3);
    account(&rig, "kbf-lease-foo", me() + 1);
    rig.host.state().fail.insert("delete_user");
    let error = rig.helper.boot_reset(&[]).unwrap_err();
    assert_eq!(
        error,
        "the boot reset is incomplete: kbf-lease-foo: deleting user kbf-lease-foo: \
         delete_user failed; kbf-lease-pool-3: deleting user kbf-lease-pool-3: delete_user failed"
    );
    assert!(!rig.helper.reset_done());
    let refused = rig.helper.handle(
        Request::UserCreate {
            lease: "84.1".to_owned(),
            grant: None,
        },
        Vec::new(),
    );
    assert!(matches!(
        refused,
        Outcome::Reply(Reply::Refused { reason }) if reason == RESET_NEEDED
    ));
    // The cause gone, the boot reset can be run again.
    rig.host.state().fail.clear();
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, Vec::<LeaseId>::new());
    assert_eq!(names(&rig), Vec::<String>::new());
    assert!(rig.helper.reset_done());
}

/// Catches: an account whose processes outlive the kill, or that is not shown gone after
/// its deletion, passed over by the boot reset by name: each is a failure that names the
/// account and the step.
#[test]
fn an_account_that_is_not_shown_gone_fails_the_boot_reset_naming_the_step() {
    let rig = booting("boot-fails-proof");
    account(&rig, "kbf-lease-pool-0", me());
    rig.host.state().after_delete = Some(Box::new(|state| {
        state.foreign.insert(me());
    }));
    let error = rig.helper.boot_reset(&[]).unwrap_err();
    assert_eq!(
        error,
        format!(
            "the boot reset is incomplete: kbf-lease-pool-0: not shown gone (record): a record has uid {}",
            me()
        )
    );

    let rig = booting("boot-fails-kill");
    account(&rig, "kbf-lease-pool-0", me());
    rig.host.state().stubborn.insert(me(), 100);
    rig.host.state().procs.insert(me(), 1);
    let error = rig.helper.boot_reset(&[]).unwrap_err();
    assert_eq!(
        error,
        format!(
            "the boot reset is incomplete: kbf-lease-pool-0: 1 processes of uid {uid} remain \
             after 10 attempts: 1000 (proc0); uid {uid} is quarantined: no other lease gets it \
             until a delete succeeds",
            uid = me()
        )
    );
    assert!(rig.host.state().users.contains_key("kbf-lease-pool-0"));
}

/// Catches: a directory that cannot be asked taken for one with no accounts in it (the
/// boot reset would then start over accounts it never looked at), an account that cannot be
/// recorded deleted anyway (its lease id would be free to use), and a pool file that cannot
/// be removed followed by the deletion of the accounts it describes.
#[test]
fn what_cannot_be_looked_at_or_recorded_fails_the_boot_reset() {
    let rig = booting("boot-fails-list");
    rig.host.state().fail.insert("lease_accounts");
    assert_eq!(
        rig.helper.boot_reset(&[]).unwrap_err(),
        "the boot reset is incomplete: listing the lease accounts: lease_accounts failed"
    );

    let rig = booting("boot-fails-record");
    account(&rig, "kbf-lease-86-1", me());
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"));
    let error = rig.helper.boot_reset(&[]).unwrap_err();
    assert!(
        error.starts_with("the boot reset is incomplete: kbf-lease-86-1: recording it: "),
        "{error}"
    );
    assert!(rig.host.state().users.contains_key("kbf-lease-86-1"));
    assert!(!rig.helper.reset_done());

    // A pool file that cannot be unlinked (a folder with something in it): no pool account
    // is deleted, and each is named.
    let rig = booting("boot-fails-pool-file");
    std::fs::create_dir_all(rig.dir.join("pool/inside")).unwrap();
    account(&rig, "kbf-lease-pool-0", me());
    account(&rig, "kbf-lease-pool-1", me() + 1);
    let error = rig.helper.boot_reset(&[]).unwrap_err();
    assert!(
        error.starts_with(
            "the boot reset is incomplete: kbf-lease-pool-0: removing the pool file: "
        ) && error.contains("; kbf-lease-pool-1: removing the pool file: "),
        "{error}"
    );
    assert_eq!(names(&rig).len(), 2);
}

/// Catches: the boot reset deleting, by its name, an account that shares the uid of the
/// lease whose session it keeps (a pool account of the release before this one is on that
/// uid): the kept lease's processes would die with it.
#[test]
fn the_uid_of_the_kept_session_is_never_touched_by_name() {
    let rig = booting("boot-kept");
    let lease = LeaseId::new(87, 1);
    rig.helper.lock().record_created(lease, me()).unwrap();
    write_intent(&rig.dir.join("session-intent"), Intent::Lease(lease)).unwrap();
    account(&rig, &user_name(lease), me());
    account(&rig, "kbf-lease-pool-0", me());
    account(&rig, "kbf-lease-pool-1", me() + 1);
    let reset = rig.helper.boot_reset(&["87.1".to_owned()]).unwrap();
    assert_eq!(reset.kept, Some(lease));
    assert_eq!(
        names(&rig),
        ["kbf-lease-87-1".to_owned(), "kbf-lease-pool-0".to_owned()]
    );
    assert_eq!(rig.helper.lock().live_leases(), [lease]);
}
