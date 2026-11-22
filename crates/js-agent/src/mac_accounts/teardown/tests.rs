//! Tests of deleting an account: the order of the steps, what each step's failure holds the
//! account at, and that a held account is tried again and its uid is never chosen meanwhile.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};

use super::*;
use crate::mac_accounts::tests::fake::Print;
use crate::mac_accounts::tests::rig::{Rig, me};
use crate::mac_accounts::{Account, AccountError, Held, Pool};

/// A rig of four uids with no account kept ready, and the first account, lent.
fn lent(name: &str) -> (Rig, Pool, Account) {
    lent_on(Rig::new(name, 4, 0))
}

/// The first account lent over `rig`.
fn lent_on(rig: Rig) -> (Rig, Pool, Account) {
    let pool = rig.open();
    let account = pool.lend("job-1").unwrap().account;
    rig.fake.take_calls();
    (rig, pool, account)
}

fn held(result: Result<crate::mac_accounts::Retired, Held>) -> Held {
    match result {
        Ok(retired) => panic!("the account was deleted: {retired:?}"),
        Err(held) => held,
    }
}

/// Asserts the order of a deletion of an account with nothing running: the processes are
/// ended and the domains booted out and looked at, the crontab and at jobs are removed,
/// that is done once more, the IPC objects go, the record is deleted, and only then is the
/// account shown gone, each item looked at again after the deletion.
#[test]
fn a_deletion_makes_these_calls_in_this_order() {
    let (rig, pool, account) = lent("order");
    pool.retire(account).unwrap();
    let u = me();
    let round = [
        format!("processes {u}"),
        format!("kill_all {u}"),
        format!("bootout gui/{u}"),
        format!("bootout user/{u}"),
        format!("processes {u}"),
        format!("launchctl_print gui/{u}"),
        format!("launchctl_print user/{u}"),
    ];
    let want = [
        round.as_slice(),
        round.as_slice(),
        &[
            "command /usr/bin/ipcs -a".to_owned(),
            "delete_account kbf-acct-1".to_owned(),
            "account kbf-acct-1".to_owned(),
            format!("uid_taken {u}"),
            format!("processes {u}"),
            format!("launchctl_print gui/{u}"),
            format!("launchctl_print user/{u}"),
            "command /usr/bin/ipcs -a".to_owned(),
        ],
    ]
    .concat();
    assert_eq!(rig.fake.take_calls(), want);
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-1");
}

/// Asserts that the crontab and the at jobs of the account are removed after the first kill
/// and before the second, so a job that fired meanwhile is killed too and none starts after.
#[test]
fn the_crontab_and_at_jobs_go_between_the_kill_rounds() {
    let (rig, pool, account) = lent("schedules");
    let tab = rig.dir.join("at/tabs/kbf-acct-1");
    let job = rig.dir.join("at/jobs/a.job");
    std::fs::write(&tab, "* * * * * x\n").unwrap();
    std::fs::write(&job, "x").unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let looking = Arc::clone(&seen);
    rig.fake.on("kill_all", move || {
        looking.lock().unwrap().push((tab.exists(), job.exists()));
    });
    pool.retire(account).unwrap();
    assert_eq!(*seen.lock().unwrap(), [(true, true), (false, false)]);
}

/// Asserts that a crontab that cannot be removed holds the account at `schedules`, and that
/// the second kill still ran: the removal's failure is judged after, never instead.
#[test]
fn schedules_that_cannot_be_removed_hold_the_account_after_the_second_kill() {
    let (rig, pool, account) = lent("schedules-fail");
    std::fs::remove_dir(rig.dir.join("at/tabs")).unwrap();
    std::fs::write(rig.dir.join("at/tabs"), "").unwrap();
    let held = held(pool.retire(account.clone()));
    assert_eq!(held.step, Step::Schedules);
    assert!(
        held.why.contains("at/tabs: Not a directory"),
        "{}",
        held.why
    );
    let calls = rig.fake.take_calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("kill_all")).count(),
        2
    );
    assert_eq!(rig.lines().last().unwrap(), "held kbf-acct-1 schedules");
    assert_eq!(pool.held(), [account]);
}

/// Asserts that a process that survives every round holds the account at `kill`, naming it,
/// after ten rounds with a pause between each; that the ledger says `held` and the pool lists
/// the account; and that retiring the held account tries every step again, and once the
/// cause is gone it is deleted and recorded `gone`, with each pid reported once.
#[test]
fn a_process_that_survives_holds_the_account_and_a_retry_deletes_it() {
    let (rig, pool, account) = lent("immortal");
    rig.fake.add_process(me(), 40, "daemon");
    rig.fake.state().immortal.insert(me());
    let held = held(pool.retire(account.clone()));
    assert_eq!(held.account, account);
    assert_eq!(held.step, Step::Kill);
    assert_eq!(
        held.why,
        format!(
            "1 processes of uid {} remain after 10 rounds: 40 (daemon)",
            me()
        )
    );
    assert_eq!(
        held.to_string(),
        format!(
            "account kbf-acct-1 (uid {}) is held at kill: 1 processes of uid {} remain after 10 rounds: 40 (daemon)",
            me(),
            me()
        )
    );
    assert_eq!(rig.lines().last().unwrap(), "held kbf-acct-1 kill");
    assert_eq!(pool.held(), std::slice::from_ref(&account));
    let calls = rig.fake.take_calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("kill_all")).count(),
        10
    );
    assert_eq!(rig.fake.state().pauses, 10);
    assert!(
        calls.iter().all(|c| !c.starts_with("delete_account")),
        "nothing was deleted"
    );

    rig.fake.state().immortal.clear();
    let retired = pool.retire(account).unwrap();
    assert_eq!(retired.killed, [40]);
    assert!(pool.held().is_empty());
    assert_eq!(rig.lines().last().unwrap(), "gone kbf-acct-1");
}

/// Asserts that a refusal names at most eight processes and says how many more, and counts
/// every pid that appeared in the kill, including one that started in a later round.
#[test]
fn the_processes_a_refusal_names_are_at_most_eight() {
    let (rig, pool, account) = lent("many");
    for pid in 1..=10 {
        rig.fake.add_process(me(), pid, "p");
    }
    rig.fake.state().immortal.insert(me());
    let held = held(pool.retire(account.clone()));
    assert_eq!(
        held.why,
        format!(
            "10 processes of uid {} remain after 10 rounds: 1 (p), 2 (p), 3 (p), 4 (p), 5 (p), 6 (p), 7 (p), 8 (p), and 2 more",
            me()
        )
    );
    rig.fake.state().immortal.clear();
    rig.fake.add_process(me(), 11, "late");
    let retired = pool.retire(account).unwrap();
    assert_eq!(retired.killed, (1..=11).collect::<Vec<i32>>());
}

/// Asserts that a launchd domain that still prints after ten rounds holds the account at
/// `domains`, naming the domain and why its bootout failed; and that processes and a domain
/// together are a `kill` failure that names both.
#[test]
fn a_domain_that_stays_holds_the_account_at_domains() {
    let (rig, pool, account) = lent("domain");
    let user = format!("user/{}", me());
    rig.fake.state().print.insert(user.clone(), Print::Stays);
    rig.fake.state().stubborn.insert(user.clone());
    let at_domains = held(pool.retire(account.clone()));
    assert_eq!(at_domains.step, Step::Domains);
    assert_eq!(
        at_domains.why,
        format!(
            "launchd domains of uid {} remain after 10 rounds: {user} (bootout: Boot-out failed: 5: Input/output error)",
            me()
        )
    );
    rig.fake.add_process(me(), 7, "agent");
    rig.fake.state().immortal.insert(me());
    let at_kill = held(pool.retire(account.clone()));
    assert_eq!(at_kill.step, Step::Kill);
    assert!(
        at_kill.why.starts_with(&format!(
            "1 processes of uid {} remain after 10 rounds: 7 (agent); launchd domains",
            me()
        )),
        "{}",
        at_kill.why
    );
    rig.fake.state().stubborn.clear();
    rig.fake.state().immortal.clear();
    pool.retire(account).unwrap();
}

/// Asserts that a kill that cannot be sent and a process table that cannot be read each hold
/// the account at `kill`, with the call's reason.
#[test]
fn a_kill_or_a_listing_that_fails_holds_the_account_at_kill() {
    let (rig, pool, account) = lent("kill-fails");
    let u = me();
    rig.fake.fail("kill_all");
    let held1 = held(pool.retire(account.clone()));
    assert_eq!(held1.step, Step::Kill);
    assert_eq!(
        held1.why,
        format!("killing the processes of uid {u}: kill_all {u} failed")
    );
    rig.fake.heal("kill_all");
    rig.fake.fail(&format!("processes {u}"));
    let held2 = held(pool.retire(account.clone()));
    assert_eq!(held2.step, Step::Kill);
    assert_eq!(
        held2.why,
        format!("listing the processes of uid {u}: processes {u} failed")
    );
    rig.fake.heal(&format!("processes {u}"));
    pool.retire(account).unwrap();
}

/// Asserts that an IPC object that stays holds the account at `ipc`, and is removed by the
/// retry.
#[test]
fn an_ipc_object_that_stays_holds_the_account_at_ipc() {
    let (rig, pool, account) = lent("ipc");
    rig.fake.add_ipc('m', 100, "kbf-acct-1", "kbf-acct-1");
    rig.fake.state().ipcrm_fails = true;
    let held = held(pool.retire(account.clone()));
    assert_eq!(held.step, Step::Ipc);
    assert_eq!(
        held.why,
        format!(
            "IPC objects of uid {} remain after their removal: -m 100",
            me()
        )
    );
    rig.fake.state().ipcrm_fails = false;
    pool.retire(account).unwrap();
    assert!(rig.fake.state().ipc.is_empty());
}

/// Asserts that a shared place that cannot be swept holds the account at `sweep`, naming it.
#[test]
fn a_shared_place_that_cannot_be_swept_holds_the_account_at_sweep() {
    let (rig, pool, account) = lent("sweep");
    std::fs::remove_dir(rig.dir.join("Shared")).unwrap();
    std::fs::write(rig.dir.join("Shared"), "").unwrap();
    let held = held(pool.retire(account));
    assert_eq!(held.step, Step::Sweep);
    assert!(held.why.contains("Shared: Not a directory"), "{}", held.why);
}

/// Asserts that a record that cannot be deleted holds the account at `record`, that nothing
/// of the home is removed before it (the deletion goes record, home, folders), and that a
/// record that is not there is no error.
#[test]
fn a_record_that_cannot_be_deleted_holds_the_account_at_record() {
    let (rig, pool, account) = lent("record");
    rig.fake.fail("delete_account kbf-acct-1");
    let held = held(pool.retire(account.clone()));
    assert_eq!(held.step, Step::Record);
    assert_eq!(
        held.why,
        "deleting the record kbf-acct-1: delete_account kbf-acct-1 failed"
    );
    assert!(
        rig.home(1).is_dir(),
        "the home stays until the record is gone"
    );
    rig.fake.heal("delete_account kbf-acct-1");
    rig.fake.state().users.remove("kbf-acct-1");
    pool.retire(account).unwrap();
    assert!(!rig.home(1).exists());
}

/// Asserts that a home that cannot be removed holds the account at `home`.
#[test]
fn a_home_that_cannot_be_removed_holds_the_account_at_home() {
    let (rig, pool, account) = lent("home");
    std::fs::remove_dir_all(rig.dir.join("Users")).unwrap();
    std::fs::write(rig.dir.join("Users"), "").unwrap();
    let held = held(pool.retire(account));
    assert_eq!(held.step, Step::Home);
    assert!(held.why.contains("Users: Not a directory"), "{}", held.why);
}

/// Asserts that the uid's directories in the per-user folders are removed whole, and that
/// folders that cannot be searched, or a directory that cannot be removed, hold the account
/// at `folder`, naming the path.
#[test]
fn the_per_user_folders_are_removed_and_a_refusal_holds_the_account_at_folder() {
    let (rig, pool, account) = lent("folder");
    let hash = rig.dir.join("folders/ab/hash");
    std::fs::create_dir_all(hash.join("T/inner")).unwrap();
    std::fs::write(hash.join("T/inner/f"), "x").unwrap();
    let folders = rig.dir.join("folders");
    std::fs::rename(&folders, rig.dir.join("folders-kept")).unwrap();
    std::fs::write(&folders, "").unwrap();
    let held1 = held(pool.retire(account.clone()));
    assert_eq!(held1.step, Step::Folder);
    assert!(
        held1.why.contains("folders: Not a directory"),
        "{}",
        held1.why
    );
    std::fs::remove_file(&folders).unwrap();
    std::fs::rename(rig.dir.join("folders-kept"), &folders).unwrap();

    if me() != 0 {
        let inner = hash.join("T/inner");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o500)).unwrap();
        let held2 = held(pool.retire(account.clone()));
        assert_eq!(held2.step, Step::Folder);
        assert!(held2.why.contains("hash/T/inner/f: "), "{}", held2.why);
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    pool.retire(account).unwrap();
    assert!(!hash.exists());
    assert!(folders.join("ab").is_dir(), "only the uid's directory goes");
}

/// Asserts that a held account's uid is chosen by no one until a retirement shows it gone:
/// with one uid in the range, a loan is refused naming the account that holds it, and after
/// the retirement the uid is used again by a new account under a new number.
#[test]
fn a_held_uid_is_chosen_by_no_one_until_the_account_is_shown_gone() {
    let rig = Rig::new("held-uid", 1, 0);
    let pool = rig.open();
    let account = pool.lend("job-1").unwrap().account;
    rig.fake.add_process(me(), 40, "daemon");
    rig.fake.state().immortal.insert(me());
    held(pool.retire(account.clone()));
    let Err(AccountError::NoFreeUid { reasons, .. }) = pool.lend("job-2") else {
        panic!("the held uid was lent");
    };
    assert_eq!(reasons, [format!("{}: held by kbf-acct-1", me())]);
    rig.fake.state().immortal.clear();
    pool.retire(account).unwrap();
    let next = pool.lend("job-2").unwrap().account;
    assert_eq!((next.name.as_str(), next.uid), ("kbf-acct-2", me()));
}

/// Asserts that a ledger that cannot record the end of a deletion is the failure, at
/// `ledger`: after a deletion that worked (the account is gone, the ledger does not know,
/// and the next open deletes it again), and after one that failed (naming both).
#[test]
fn a_ledger_that_cannot_record_the_end_is_the_failure() {
    let (rig, pool, account) = lent("ledger");
    pool.ledger().fail_writes(&rig.dir.join("state/accounts"));
    let held1 = held(pool.retire(account.clone()));
    assert_eq!(held1.step, Step::Ledger);
    assert!(held1.why.starts_with("recording gone: "), "{}", held1.why);
    assert!(!rig.home(1).exists(), "the account itself was deleted");

    let (rig, pool, account) = lent_on(rig.next("ledger-held", 4, 0));
    rig.fake.add_process(me(), 40, "daemon");
    rig.fake.state().immortal.insert(me());
    pool.ledger().fail_writes(&rig.dir.join("state/accounts"));
    let held2 = held(pool.retire(account));
    assert_eq!(held2.step, Step::Ledger);
    assert!(
        held2.why.starts_with("recording held at kill: "),
        "{}",
        held2.why
    );
    assert!(held2.why.contains("; 1 processes of uid"), "{}", held2.why);
}
