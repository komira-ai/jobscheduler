//! The pool against the fake host: a lease takes a free pool user and gives it back
//! reset, a reset that is not clean replaces the user, a replacement that fails breaks
//! the slot, and the pool is made, and recovered after a restart, from the ledger.

use std::path::PathBuf;

use super::*;
use crate::helper::tests::{Rig, add_legacy, log, me, my_gid, rig, rig_with};
use crate::helper::{IPCS, NewUser, Outcome};
use crate::lease::{PoolSize, UidRange};
use crate::proto::{PoolState, Reply, Request};

/// The calls in `calls` that make or delete a user record.
fn records(calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .filter(|c| c.starts_with("create_user") || c.starts_with("delete_user"))
        .cloned()
        .collect()
}

/// The home folder of slot `n`'s user in `rig`.
fn home(rig: &Rig, n: u32) -> PathBuf {
    rig.dir.join(format!("Users/kbf-lease-pool-{n}"))
}

/// The `DARWIN_USER_DIR` entry of `uid` in `rig`: what the fake host's answer names.
fn entry(rig: &Rig, uid: u32) -> PathBuf {
    rig.dir.join(format!("folders/zz/{uid}"))
}

/// The slots `slots`.
fn slots(slots: &[u32]) -> Vec<Slot> {
    slots.iter().copied().map(Slot::new).collect()
}

/// A rig over `rig`'s state directory and fake host after a restart, its boot reset not
/// yet run.
fn restarted(rig: Rig, name: &str) -> Rig {
    let range = rig.helper.settings.range;
    let host = rig.host.clone();
    let dir = rig.dir.clone();
    drop(rig);
    let mut again = rig_with(name, range, Some(dir));
    again.host = host.clone();
    again.helper.host = Box::new(host);
    again
}

/// Catches: a lease that gets a fresh uid (or a user made for it) instead of the pool's,
/// or a second lease on the same slot that is not handed the same uid and user: the
/// lease, its release and the next lease make and delete no user record at all.
#[test]
fn pool_reuses_uid_across_leases() {
    let rig = rig("pool-reuse", 1);
    let uid = rig.helper.user_create("70.1", None).unwrap();
    assert_eq!(uid, me());
    assert_eq!(rig.helper.user_delete("70.1"), Ok(true));
    assert_eq!(rig.helper.user_create("70.2", None), Ok(uid));
    let calls = log(&rig);
    assert_eq!(records(&calls), Vec::<String>::new(), "{calls:#?}");
    assert_eq!(
        rig.host.state().users.keys().collect::<Vec<_>>(),
        ["kbf-lease-pool-0"]
    );
    assert_eq!(
        rig.helper.pool_status().leased,
        [(Slot::new(0), LeaseId::new(70, 2))]
    );
}

/// Catches a reset that leaves anything of one lease for the next on the same uid: a
/// file in the home (and the home's own mode), a process, a System V shared memory
/// segment and semaphore, a launchd job in the user domain, a crontab, a file in a swept
/// folder, and files in the uid's `DARWIN_USER_DIR` entry (whose `0`, `C` and `T` stay,
/// emptied). Every one is planted by the first lease and none is there for the second.
#[test]
fn reset_leaves_no_residue() {
    let rig = rig("pool-residue", 1);
    let uid = rig.helper.user_create("71.1", None).unwrap();
    let home = home(&rig, 0);
    std::fs::create_dir_all(home.join("Library/Keychains")).unwrap();
    std::fs::write(home.join("Library/Keychains/login.keychain-db"), "secret").unwrap();
    std::fs::write(home.join("secret"), "bytes of lease 71.1").unwrap();
    std::fs::create_dir_all(rig.dir.join("tabs")).unwrap();
    std::fs::write(rig.dir.join("tabs/kbf-lease-pool-0"), "* * * * * job").unwrap();
    std::fs::create_dir_all(rig.dir.join("Shared")).unwrap();
    std::fs::write(rig.dir.join("Shared/left"), "x").unwrap();
    let entry = entry(&rig, uid);
    for folder in ["0", "C/com.apple.ScreenTimeAgent", "T"] {
        std::fs::create_dir_all(entry.join(folder)).unwrap();
    }
    std::fs::write(entry.join("T/scratch"), "x").unwrap();
    std::fs::write(entry.join("C/com.apple.ScreenTimeAgent/db"), "x").unwrap();
    std::fs::write(entry.join("stray"), "x").unwrap();
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 2);
        state
            .ipc
            .insert(65537, ('m', "kbf-lease-pool-0".to_owned()));
        state.ipc.insert(65538, ('s', uid.to_string()));
        state.ipc.insert(65539, ('m', "root".to_owned()));
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
    }
    assert_eq!(rig.helper.user_delete("71.1"), Ok(true));
    let calls = log(&rig);
    assert_eq!(records(&calls), Vec::<String>::new(), "reset, not replaced");

    assert_eq!(rig.helper.user_create("71.2", None), Ok(uid));
    // The home is removed whole and made again, empty, by the host.
    assert!(!home.exists());
    let made = format!("make_home {} {uid}:{}", home.display(), my_gid());
    assert!(calls.contains(&made), "{calls:#?}");
    assert!(!rig.dir.join("tabs/kbf-lease-pool-0").exists());
    assert_eq!(
        std::fs::read_dir(rig.dir.join("Shared")).unwrap().count(),
        0
    );
    let mut kept: Vec<_> = std::fs::read_dir(&entry)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    kept.sort();
    assert_eq!(kept, ["0", "C", "T"]);
    for folder in ["0", "C", "T"] {
        assert_eq!(std::fs::read_dir(entry.join(folder)).unwrap().count(), 0);
    }
    let state = rig.host.state();
    assert_eq!(state.procs.get(&uid), None);
    assert!(state.domains.is_empty(), "{:?}", state.domains);
    assert!(!state.agents.contains(&uid));
    assert_eq!(
        state.ipc.keys().collect::<Vec<_>>(),
        [&65539],
        "only root's segment stays"
    );
}

/// Catches: a reset that is not clean returning the user to free as it is, a
/// replacement that keeps the old record (or makes the new one under another name or
/// uid), or a slot free before its new user exists.
#[test]
fn failed_reset_deletes_and_replaces() {
    let rig = rig("pool-replace", 1);
    let uid = rig.helper.user_create("72.1", None).unwrap();
    std::fs::create_dir_all(home(&rig, 0)).unwrap();
    std::fs::write(home(&rig, 0).join("left"), "x").unwrap();
    // The reset's last step fails (its home cannot be made again): not clean.
    rig.host
        .state()
        .fail_script
        .insert("make_home", [true].into());
    log(&rig);
    assert_eq!(rig.helper.user_delete("72.1"), Ok(true));
    let calls = log(&rig);
    let made = format!("make_home {} {uid}:{}", home(&rig, 0).display(), my_gid());
    let deleted = calls
        .iter()
        .position(|c| c == "delete_user kbf-lease-pool-0")
        .unwrap();
    let homes: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == made)
        .map(|(i, _)| i)
        .collect();
    let created = calls
        .iter()
        .position(|c| c == "create_user kbf-lease-pool-0")
        .unwrap();
    // The reset's failed home, then the deletion, the new home and the new record.
    assert_eq!(homes.len(), 2, "{calls:#?}");
    assert!(homes[0] < deleted && deleted < homes[1] && homes[1] < created);
    assert_eq!(
        rig.host.state().users["kbf-lease-pool-0"],
        NewUser {
            name: "kbf-lease-pool-0".to_owned(),
            uid,
            gid: my_gid(),
            home: home(&rig, 0),
            admin: false,
        }
    );
    assert!(!home(&rig, 0).join("left").exists());
    assert_eq!(rig.helper.pool_status().free, slots(&[0]));
    assert_eq!(rig.helper.user_create("72.2", None), Ok(uid));
    let pool = std::fs::read_to_string(rig.dir.join("pool")).unwrap();
    assert_eq!(
        pool,
        "made 0 1000\nfree 0\nleased 0 72.1\nresetting 0\nfree 0\nleased 0 72.2\n"
            .replace("1000", &uid.to_string())
    );
}

/// Catches: a slot whose reset and replacement both failed handed to a lease, or
/// shown as anything but broken; and a broken slot that `pool-create` never repairs.
#[test]
fn replacement_failure_marks_slot_broken() {
    let rig = rig("pool-broken", 2);
    let uid = rig.helper.user_create("73.1", None).unwrap();
    rig.helper.user_create("73.2", None).unwrap();
    rig.host
        .state()
        .fail_script
        .insert("make_home", [true].into());
    rig.host.state().fail.insert("create_user");
    let error = rig.helper.user_delete("73.1").unwrap_err();
    assert_eq!(
        error,
        "slot 0's user kbf-lease-pool-0 could be neither reset nor replaced, so the slot \
         is broken and lent to no one until pool-create replaces it: creating user \
         kbf-lease-pool-0: create_user failed"
    );
    // The lease is over all the same: a repeat finds nothing to do.
    assert_eq!(rig.helper.user_delete("73.1"), Ok(false));
    let status = rig.helper.pool_status();
    assert_eq!(status.broken, slots(&[0]));
    assert_eq!(status.free, Vec::<Slot>::new());
    // Never lent: the pool is exhausted, and says so.
    let Err(LeaseError::Exhausted(full)) = rig.helper.user_create("73.3", None) else {
        panic!("a broken slot was lent");
    };
    assert_eq!(
        full.to_string(),
        "the pool has no free user: of 2 slots, 1 leased, 0 resetting, broken: [0], not \
         made: []; retry when a lease ends"
    );
    // A pool-create that cannot replace it either leaves it broken, and says why.
    let created = rig.helper.pool_create();
    assert_eq!(created.broken, slots(&[0]));
    assert_eq!(
        created.errors,
        [
            "slot 0: slot 0's user kbf-lease-pool-0 could be neither reset nor replaced, \
             so the slot is broken and lent to no one until pool-create replaces it: \
             creating user kbf-lease-pool-0: create_user failed"
        ]
    );
    // One that can repairs it.
    rig.host.state().fail.clear();
    let created = rig.helper.pool_create();
    assert_eq!(created.errors, Vec::<String>::new());
    assert_eq!(created.free, slots(&[0]));
    assert_eq!(rig.helper.user_create("73.3", None), Ok(uid));
}

/// Catches: a full pool that makes a user to make up for it, or that fails with a
/// refusal the caller cannot tell from any other.
#[test]
fn pool_exhaustion_is_typed_error() {
    let rig = rig("pool-full", 2);
    rig.helper.user_create("74.1", None).unwrap();
    rig.helper.user_create("74.2", None).unwrap();
    log(&rig);
    let full = rig.helper.user_create("74.3", None).unwrap_err();
    let LeaseError::Exhausted(exhausted) = &full else {
        panic!("{full:?}");
    };
    assert_eq!(exhausted.0.leased.len(), 2);
    assert!(full.to_string().starts_with(POOL_EXHAUSTED));
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("create_user") || c.starts_with("make_home")),
        "{calls:#?}"
    );
    // Nothing was spent: the lease id is still unused, and gets the next free user.
    rig.helper.user_delete("74.1").unwrap();
    assert_eq!(rig.helper.user_create("74.3", None), Ok(me()));
    // Over the socket's protocol, too, it is a refusal that starts with the same words.
    let reply = match rig.helper.handle(
        Request::UserCreate {
            lease: "74.4".to_owned(),
            grant: None,
        },
        Vec::new(),
    ) {
        Outcome::Reply(Reply::Refused { reason }) => reason,
        other => panic!("{other:?}"),
    };
    assert!(reply.starts_with(POOL_EXHAUSTED), "{reply}");
}

/// Catches: a restart that hands out a slot whose lease was cut off by a crash (leased
/// to a lease the ledger has not, or no longer, live) or whose reset was cut off
/// (resetting), without resetting it first.
#[test]
fn ledger_recovery_resets_orphans() {
    let rig = rig("pool-recover", 3);
    let base = me();
    rig.helper.user_create("75.1", None).unwrap();
    rig.helper.user_create("75.2", None).unwrap();
    // A crash after slot 1's `leased`, before the lease's `created`: the ledger, written
    // by hand as the crash left it, has the lease's record dropped.
    let ledger = std::fs::read_to_string(rig.dir.join("ledger")).unwrap();
    std::fs::write(
        rig.dir.join("ledger"),
        ledger.replace(&format!("created 75.2 {}\n", base + 1), ""),
    )
    .unwrap();
    // A crash during slot 0's reset, after `resetting`.
    let mut pool = std::fs::read_to_string(rig.dir.join("pool")).unwrap();
    pool.push_str("resetting 0\n");
    std::fs::write(rig.dir.join("pool"), pool).unwrap();
    std::fs::write(
        rig.dir.join("ledger"),
        std::fs::read_to_string(rig.dir.join("ledger")).unwrap() + "deleted 75.1\n",
    )
    .unwrap();
    std::fs::create_dir_all(home(&rig, 0)).unwrap();
    std::fs::write(home(&rig, 0).join("of-75.1"), "x").unwrap();
    std::fs::create_dir_all(home(&rig, 1)).unwrap();
    std::fs::write(home(&rig, 1).join("of-75.2"), "x").unwrap();

    let rig = restarted(rig, "pool-recover");
    {
        let status = rig.helper.pool_status();
        assert_eq!(status.resetting, slots(&[0]));
        assert_eq!(status.leased, [(Slot::new(1), LeaseId::new(75, 2))]);
    }
    rig.host.state().log.clear();
    // Nothing is lent before the boot reset.
    let early = Request::UserCreate {
        lease: "75.3".to_owned(),
        grant: None,
    };
    assert!(matches!(
        rig.helper.handle(early, Vec::new()),
        Outcome::Reply(Reply::Refused { reason }) if reason == crate::helper::RESET_NEEDED
    ));
    rig.helper.boot_reset(&[]).unwrap();
    let calls = log(&rig);
    for n in [0, 1] {
        let made = format!(
            "make_home {} {}:{}",
            home(&rig, n).display(),
            base + n,
            my_gid()
        );
        assert!(calls.contains(&made), "slot {n} was not reset: {calls:#?}");
    }
    assert!(!home(&rig, 0).join("of-75.1").exists());
    assert!(!home(&rig, 1).join("of-75.2").exists());
    assert_eq!(rig.helper.pool_status().free, slots(&[0, 1, 2]));
    let pool = std::fs::read_to_string(rig.dir.join("pool")).unwrap();
    assert!(
        pool.ends_with("resetting 0\nfree 0\nresetting 1\nfree 1\n"),
        "{pool}"
    );
}

/// Catches: a second setup that makes a user again (or a second record for one), or one
/// that adopts a user the ledger does not hold.
#[test]
fn pool_create_is_idempotent() {
    let rig = rig("pool-idempotent", 3);
    let users = rig.host.state().users.clone();
    assert_eq!(
        users.keys().collect::<Vec<_>>(),
        ["kbf-lease-pool-0", "kbf-lease-pool-1", "kbf-lease-pool-2"]
    );
    let pool = std::fs::read_to_string(rig.dir.join("pool")).unwrap();
    let created = rig.helper.pool_create();
    assert_eq!(created.free, slots(&[0, 1, 2]));
    assert_eq!(created.errors, Vec::<String>::new());
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("create_user") || c.starts_with("uid_taken")),
        "{calls:#?}"
    );
    assert_eq!(rig.host.state().users, users);
    assert_eq!(std::fs::read_to_string(rig.dir.join("pool")).unwrap(), pool);
    // And after a restart, whose boot reset makes the pool again.
    let rig = restarted(rig, "pool-idempotent");
    rig.helper.boot_reset(&[]).unwrap();
    assert!(records(&log(&rig)).is_empty());
    assert_eq!(rig.host.state().users, users);
    assert_eq!(std::fs::read_to_string(rig.dir.join("pool")).unwrap(), pool);
}

/// Catches: a slot made over a uid the ledger does not hold for it (a user record of
/// someone else's, a running process, a live lease's from before the pool), a slot past
/// the uid range, or a failure to make one that leaves it free or stops the others.
#[test]
fn pool_create_makes_a_slot_only_on_a_uid_nothing_else_has() {
    let base = me();
    let mut rig = rig_with("pool-make", UidRange::new(base, base + 4).unwrap(), None);
    rig.helper.settings.pool = PoolSize::new(5).unwrap();
    rig.host.state().procs.insert(base + 1, 1);
    add_legacy(&rig, "76.1");
    // The legacy lease holds base: the boot reset ends it first, then makes the pool.
    rig.host
        .state()
        .fail_script
        .insert("create_user", [false, true].into());
    rig.helper.boot_reset(&[]).unwrap();
    let status = rig.helper.pool_status();
    assert_eq!(status.free, slots(&[0, 3, 4]));
    assert_eq!(status.broken, slots(&[2]));
    assert_eq!(status.missing, slots(&[1]));
    log(&rig);

    // Each refusal, by its reason.
    let mut rig = rig;
    rig.helper.settings.pool = PoolSize::new(6).unwrap();
    rig.host.state().procs.clear();
    rig.host.state().foreign.insert(base + 1);
    let created = rig.helper.pool_create();
    assert_eq!(
        created.errors,
        [
            format!(
                "slot 1: a user record the ledger does not hold has uid {}; it is not adopted",
                base + 1
            ),
            "slot 5: the uid range ".to_owned()
                + &format!("{}-{} has no uid for it", base, base + 4),
        ]
    );
    // The broken slot was repaired on the way.
    assert_eq!(created.free, slots(&[0, 2, 3, 4]));
    rig.host.state().foreign.clear();
    rig.host.state().procs.insert(base + 1, 2);
    assert_eq!(
        rig.helper.pool_create().errors[0],
        format!("slot 1: 2 processes run as uid {}", base + 1)
    );
    rig.host.state().procs.clear();
    rig.host.state().fail.insert("live_processes");
    assert_eq!(
        rig.helper.pool_create().errors[0],
        format!(
            "slot 1: listing the processes of uid {}: live_processes failed",
            base + 1
        )
    );
    rig.host.state().fail.clear();
    rig.host.state().fail.insert("uid_taken");
    assert_eq!(
        rig.helper.pool_create().errors[0],
        format!("slot 1: looking up uid {}: uid_taken failed", base + 1)
    );
    rig.host.state().fail.clear();
    // A live lease from before the pool holds slot 1's uid.
    let legacy = LeaseId::new(76, 2);
    rig.helper.lock().record_created(legacy, base + 1).unwrap();
    assert_eq!(
        rig.helper.pool_create().errors[0],
        format!("slot 1: uid {} is held by a live lease", base + 1)
    );
    rig.helper.lock().record_deleted(legacy).unwrap();
    rig.helper.settings.pool = PoolSize::new(5).unwrap();
    assert_eq!(rig.helper.pool_create().errors, Vec::<String>::new());
    assert_eq!(rig.helper.pool_status().free, slots(&[0, 1, 2, 3, 4]));
}

/// Catches: a ledger write that fails while a slot is made, reset or recovered, taken
/// for success (the slot lent with no durable record of its state), or a failure that is
/// not named.
#[test]
fn a_pool_record_that_cannot_be_written_stops_the_slot() {
    let base = me();
    let first = |rig: &Rig| rig.helper.pool_create().errors.first().cloned();
    let mut rig = rig_with("pool-write", UidRange::new(base, base + 1).unwrap(), None);
    rig.helper.settings.pool = PoolSize::new(1).unwrap();
    rig.helper.lock().fail_pool_after(0);
    rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(
        first(&rig),
        Some("slot 0: recording slot 0: the disk is full".to_owned())
    );
    rig.helper.lock().fail_pool_after(1);
    assert_eq!(
        first(&rig),
        Some("slot 0: recording slot 0 free: the disk is full".to_owned())
    );
    assert_eq!(rig.helper.pool_status().resetting, slots(&[0]));

    let mut rig = rig_with(
        "pool-write-broken",
        UidRange::new(base, base).unwrap(),
        None,
    );
    rig.helper.settings.pool = PoolSize::new(1).unwrap();
    rig.host.state().fail.insert("create_user");
    rig.helper.lock().fail_pool_after(1);
    rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(
        first(&rig),
        None,
        "the slot is resetting, so pool-create leaves it to the recovery"
    );
    rig.host.state().fail.clear();
    let rig = restarted(rig, "pool-write-broken");
    rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(rig.helper.pool_status().free, slots(&[0]));

    // The end of a lease: its `free`, and its `broken`.
    let rig = self::rig("pool-write-end", 1);
    rig.helper.user_create("77.1", None).unwrap();
    rig.helper.lock().fail_pool_after(1);
    assert_eq!(
        rig.helper.user_delete("77.1"),
        Err("recording slot 0 free: the disk is full".to_owned())
    );
    assert_eq!(rig.helper.pool_status().resetting, slots(&[0]));
    let rig = restarted(rig, "pool-write-end");
    rig.helper.boot_reset(&[]).unwrap();
    rig.helper.user_create("77.2", None).unwrap();
    rig.host.state().fail.insert("make_home");
    rig.helper.lock().fail_pool_after(1);
    let error = rig.helper.user_delete("77.2").unwrap_err();
    assert_eq!(
        error,
        format!(
            "making {}: make_home failed; and recording slot 0 broken: the disk is full",
            home(&rig, 0).display()
        )
    );

    // An orphan whose `resetting` cannot be written stops the boot reset, naming it.
    let rig = self::rig("pool-write-orphan", 1);
    rig.helper.user_create("77.3", None).unwrap();
    std::fs::write(rig.dir.join("ledger"), "").unwrap();
    let rig = restarted(rig, "pool-write-orphan");
    rig.helper.lock().fail_pool_after(0);
    assert_eq!(
        rig.helper.boot_reset(&[]),
        Err(
            "the boot reset is incomplete: slot 0: recording slot 0 resetting: the disk is full"
                .to_owned()
        )
    );
}

/// Catches: an orphan whose reset and replacement fail stopping the boot reset (the Mac
/// would serve nothing for one bad user), or one lent anyway; and an administrator's
/// orphan reset rather than replaced.
#[test]
fn an_orphan_that_cannot_be_reset_is_broken_and_the_reset_goes_on() {
    let rig = rig("pool-orphan-broken", 2);
    let grant = crate::grant::testing::sign(
        7,
        &crate::grant::testing::text("SERIAL1", "78.1", crate::testing::NOW.cast_signed()),
    );
    rig.helper.user_create("78.1", Some(&grant)).unwrap();
    rig.helper.user_create("78.2", None).unwrap();
    std::fs::write(rig.dir.join("ledger"), "").unwrap();
    let rig = restarted(rig, "pool-orphan-broken");
    log(&rig);
    rig.host
        .state()
        .fail_script
        .insert("create_user", [true].into());
    rig.helper.boot_reset(&[]).unwrap();
    let calls = log(&rig);
    // The administrator's slot was replaced (and failed), never reset; the other reset.
    assert_eq!(
        records(&calls),
        [
            "delete_user kbf-lease-pool-0",
            "create_user kbf-lease-pool-0",
            "delete_user kbf-lease-pool-0",
            "create_user kbf-lease-pool-0",
        ],
        "replaced, failed, then repaired by the reset's pool-create"
    );
    let status = rig.helper.pool_status();
    assert_eq!(status.free, slots(&[0, 1]));
    assert!(!rig.host.state().users["kbf-lease-pool-0"].admin);
}

/// Catches: each step of the reset that does not hold taken as clean (so the user goes
/// back free as it is): its record gone, its record not readable, its IPC objects not
/// listed or not removed, its home not made again. Each replaces the user.
#[test]
fn every_reset_step_that_does_not_hold_replaces_the_user() {
    type Break = fn(&Rig, u32);
    let breaks: [(&str, Break); 5] = [
        ("its record is gone", |rig, _| {
            rig.host.state().users.remove("kbf-lease-pool-0");
        }),
        ("its record cannot be read", |rig, _| {
            rig.host
                .state()
                .fail_script
                .insert("account", [true].into());
        }),
        ("ipcs cannot run", |rig, uid| {
            rig.host.state().ipc.insert(1, ('q', uid.to_string()));
            // kill-uid's crontab sweep runs no program; the first is the reset's ipcs.
            rig.host
                .state()
                .fail_script
                .insert("command", [true].into());
        }),
        ("ipcrm cannot run", |rig, uid| {
            rig.host.state().ipc.insert(2, ('q', uid.to_string()));
            rig.host
                .state()
                .fail_script
                .insert("command", [false, true].into());
        }),
        ("the home cannot be made", |rig, _| {
            rig.host
                .state()
                .fail_script
                .insert("make_home", [true].into());
        }),
    ];
    for (n, (what, broken)) in breaks.iter().enumerate() {
        let rig = rig(&format!("pool-step-{n}"), 1);
        let uid = rig.helper.user_create("79.1", None).unwrap();
        broken(&rig, uid);
        log(&rig);
        assert_eq!(rig.helper.user_delete("79.1"), Ok(true), "{what}");
        let calls = log(&rig);
        assert_eq!(
            records(&calls).last().map(String::as_str),
            Some("create_user kbf-lease-pool-0"),
            "{what}: {calls:#?}"
        );
        assert!(
            calls.iter().any(|c| c == "delete_user kbf-lease-pool-0"),
            "{what}: {calls:#?}"
        );
        assert_eq!(rig.helper.pool_status().free, slots(&[0]), "{what}");
        assert!(rig.host.state().ipc.is_empty(), "{what}");
    }
}

/// Catches: `pool-create` and `pool-status` routed to the wrong verb, or a reply that
/// does not carry each slot's state.
#[test]
fn pool_verbs_are_routed_and_answer_each_slot() {
    let rig = rig("pool-handle", 3);
    rig.helper.user_create("80.1", None).unwrap();
    let reply = |request| match rig.helper.handle(request, Vec::new()) {
        Outcome::Reply(reply) => reply,
        Outcome::Running(child) => panic!("{child:?}"),
    };
    let want = Reply::Pool(Box::new(PoolState {
        size: 3,
        free: vec![1, 2],
        leased: vec![(0, "80.1".to_owned())],
        resetting: Vec::new(),
        broken: Vec::new(),
        missing: Vec::new(),
        errors: Vec::new(),
    }));
    assert_eq!(reply(Request::PoolStatus), want);
    assert_eq!(reply(Request::PoolCreate), want);
}

/// Catches an `ipcs` that fails (so the uid's IPC objects cannot be looked at) read as
/// none: neither the reset nor the replacement, which looks too, goes on, and the slot is
/// broken, naming why.
#[test]
fn ipc_objects_that_cannot_be_listed_break_the_slot() {
    let rig = rig("pool-ipcs", 1);
    let uid = rig.helper.user_create("81.1", None).unwrap();
    let failed = crate::autologin::CommandOutput {
        success: false,
        stdout: String::new(),
        stderr: "ipcs: sysctl failed\n".to_owned(),
    };
    rig.host
        .state()
        .answers
        .insert(format!("{IPCS} -a"), failed);
    assert_eq!(
        rig.helper.user_delete("81.1"),
        Err(format!(
            "slot 0's user kbf-lease-pool-0 could be neither reset nor replaced, so the slot \
             is broken and lent to no one until pool-create replaces it: listing the IPC \
             objects of uid {uid}: ipcs failed: ipcs: sysctl failed"
        ))
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));

    // One that ipcrm cannot remove is named, and breaks the slot the same way.
    let rig = self::rig("pool-ipcrm", 1);
    let uid = rig.helper.user_create("81.2", None).unwrap();
    rig.host
        .state()
        .ipc
        .insert(7, ('m', "kbf-lease-pool-0".to_owned()));
    rig.host.state().ipc_stuck.insert(7);
    let error = rig.helper.user_delete("81.2").unwrap_err();
    assert!(
        error.ends_with(&format!("IPC objects of uid {uid} remain: -m 7")),
        "{error}"
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));
}

/// Catches a leftover lease whose pool user can be neither reset nor replaced stopping
/// the boot reset (the Mac would serve nothing for one bad user), or that user lent
/// again: the lease is ended all the same and its slot is broken.
#[test]
fn a_leftover_lease_whose_user_breaks_does_not_stop_the_boot_reset() {
    let rig = rig("pool-leftover-broken", 2);
    rig.helper.user_create("82.1", None).unwrap();
    let rig = restarted(rig, "pool-leftover-broken");
    rig.host
        .state()
        .fail_script
        .insert("make_home", [true].into());
    rig.host
        .state()
        .fail_script
        .insert("create_user", [true, true].into());
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(82, 1)]);
    // Its replacement failed at the end of the lease and again at the repair.
    let status = rig.helper.pool_status();
    assert_eq!(status.broken, slots(&[0]));
    assert_eq!(status.free, slots(&[1]));
    assert_eq!(rig.helper.user_create("82.2", None), Ok(me() + 1));
}
