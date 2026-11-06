//! `user-create` against the pool: a lease takes the lowest free pool user and makes no
//! user, a lease id is used once, an administrator needs a valid grant (and its user is
//! replaced, before and after), and a ledger that cannot record stops the verb.

use super::*;

/// Catches: a lease that takes anything but the lowest free slot (a slot leased to
/// another lease, a resetting or broken one), a user made or deleted for a lease, or a
/// lease that gets a slot past the pool's size.
#[test]
fn each_lease_takes_the_lowest_free_slot_and_makes_no_user() {
    let rig = rig("alloc", 3);
    let base = me();
    let users = rig.host.state().users.clone();
    assert_eq!(users.len(), 3);
    assert_eq!(
        users["kbf-lease-pool-1"],
        NewUser {
            name: "kbf-lease-pool-1".to_owned(),
            uid: base + 1,
            gid: my_gid(),
            home: rig.dir.join("Users/kbf-lease-pool-1"),
            admin: false,
        }
    );
    assert_eq!(rig.helper.user_create("1.1", None), Ok(base));
    assert_eq!(rig.helper.user_create("1.2", None), Ok(base + 1));
    // 1.1 ends: its slot is the lowest free one again.
    rig.helper.user_delete("1.1").unwrap();
    assert_eq!(rig.helper.user_create("1.3", None), Ok(base));
    assert_eq!(rig.helper.user_create("1.4", None), Ok(base + 2));
    exhausted(rig.helper.user_create("1.5", None));
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("create_user") || c.starts_with("delete_user")),
        "{calls:#?}"
    );
    assert_eq!(rig.host.state().users, users);
    // A pool shrunk below a free slot never leases it.
    let mut shrunk = rig_with(
        "alloc",
        UidRange::new(base, base + 2).unwrap(),
        Some(rig.dir.clone()),
    );
    shrunk.helper.boot_reset(&[]).unwrap();
    shrunk.helper.settings.pool = crate::lease::PoolSize::new(1).unwrap();
    assert_eq!(shrunk.helper.user_create("1.6", None), Ok(base));
    exhausted(shrunk.helper.user_create("1.7", None));
    shrunk.helper.user_delete("1.6").unwrap();
    // Nor one whose uid the range no longer holds.
    let narrowed = rig_with(
        "alloc",
        UidRange::new(base + 2, base + 2).unwrap(),
        Some(rig.dir.clone()),
    );
    narrowed.helper.boot_reset(&[]).unwrap();
    exhausted(narrowed.helper.user_create("1.8", None));
}

/// Catches: a lease id used twice (a gate grant names one lease: single use),
/// including after its lease ended, and including across a restart of the helper.
#[test]
fn a_lease_id_is_never_used_twice() {
    let rig = rig("reuse", 4);
    rig.helper.user_create("2.1", None).unwrap();
    let again = rig.helper.user_create("2.1", None).unwrap_err().to_string();
    assert_eq!(again, "lease 2.1 was used before; a lease id is used once");
    rig.helper.user_delete("2.1").unwrap();
    assert!(
        rig.helper
            .user_create("2.1", None)
            .unwrap_err()
            .to_string()
            .contains("was used before")
    );
    // A new helper over the same ledger (a reboot) remembers.
    let range = UidRange::new(me(), me() + 3).unwrap();
    let rebooted = rig_with("reuse", range, Some(rig.dir.clone()));
    let after = rebooted
        .helper
        .user_create("2.1", None)
        .unwrap_err()
        .to_string();
    assert!(after.contains("was used before"), "{after}");
    assert!(
        rig.helper
            .user_create("2.01", None)
            .unwrap_err()
            .to_string()
            .contains("not <term>.<seq>")
    );
}

/// Catches: an administrator made on the daemon's word (S10: "trust an admin flag
/// from the daemon"), a grant used twice, or a grant for another lease or Mac; and a pool
/// user left an administrator, or reset rather than replaced, after its lease.
#[test]
fn an_administrator_needs_a_valid_unused_grant() {
    let rig = rig("admin", 4);
    let grant = sign(7, &text("SERIAL1", "3.1", NOW_SECS));
    assert_eq!(rig.helper.user_create("3.1", Some(&grant)), Ok(me()));
    assert!(rig.host.state().users["kbf-lease-pool-0"].admin);
    let calls = log(&rig);
    let made: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("delete_user") || c.starts_with("create_user"))
        .collect();
    assert_eq!(
        made,
        [
            "delete_user kbf-lease-pool-0",
            "create_user kbf-lease-pool-0"
        ],
        "the slot's user is replaced by an administrator"
    );
    // Single use: the lease id is spent.
    assert!(
        rig.helper
            .user_create("3.1", Some(&grant))
            .unwrap_err()
            .to_string()
            .contains("was used before")
    );
    // A grant names one lease and one Mac, and the gate's key.
    let error = rig
        .helper
        .user_create("3.2", Some(&grant))
        .unwrap_err()
        .to_string();
    assert!(error.contains("names lease 3.1"), "{error}");
    let other_mac = sign(7, &text("SERIAL2", "3.2", NOW_SECS));
    assert!(
        rig.helper
            .user_create("3.2", Some(&other_mac))
            .unwrap_err()
            .to_string()
            .contains("serial")
    );
    let forged = sign(8, &text("SERIAL1", "3.2", NOW_SECS));
    assert!(
        rig.helper
            .user_create("3.2", Some(&forged))
            .unwrap_err()
            .to_string()
            .contains("does not verify")
    );
    // A refused grant took no slot and spent nothing: the plain lease still works.
    assert_eq!(rig.helper.user_create("3.2", None), Ok(me() + 1));
    assert!(!rig.host.state().users["kbf-lease-pool-1"].admin);

    // The administrator's lease ends: its user is replaced, never reset, by one that is
    // not an administrator.
    log(&rig);
    assert_eq!(rig.helper.user_delete("3.1"), Ok(true));
    let calls = log(&rig);
    assert!(
        calls.contains(&"delete_user kbf-lease-pool-0".to_owned()),
        "{calls:#?}"
    );
    assert!(!rig.host.state().users["kbf-lease-pool-0"].admin);
    assert_eq!(
        rig.helper.pool_status().free,
        [Slot::new(0), Slot::new(2), Slot::new(3)]
    );

    // An administrator that cannot be made leaves the lease live, to be ended.
    rig.host.state().fail.insert("create_user");
    let grant = sign(7, &text("SERIAL1", "3.4", NOW_SECS));
    let error = rig
        .helper
        .user_create("3.4", Some(&grant))
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "making kbf-lease-pool-0 an administrator for lease 3.4: creating user \
         kbf-lease-pool-0: create_user failed"
    );
    rig.host.state().fail.clear();
    assert_eq!(rig.helper.user_delete("3.4"), Ok(true));
    assert!(!rig.host.state().users["kbf-lease-pool-0"].admin);

    let mut keyless = rig_with("admin-nokeys", UidRange::new(me(), me()).unwrap(), None);
    keyless.helper.settings.grant_keys = None;
    let grant = sign(7, &text("SERIAL1", "3.3", NOW_SECS));
    let error = keyless
        .helper
        .user_create("3.3", Some(&grant))
        .unwrap_err()
        .to_string();
    assert!(error.contains("no gate key"), "{error}");
    assert!(keyless.host.state().users.is_empty());
}

/// Catches: a lease recorded when the ledger could not record it (after a reboot the
/// lease id would be reused, or the slot lent twice), or its end reported done when the
/// ledger could not say so.
#[test]
fn a_ledger_that_cannot_record_stops_the_verb() {
    let rig = rig("ledger-fail", 2);
    rig.helper.user_create("4.2", None).unwrap();
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"), true);
    let error = rig.helper.user_create("4.3", None).unwrap_err().to_string();
    assert!(error.starts_with("recording lease 4.3"), "{error}");
    let error = rig.helper.user_delete("4.2").unwrap_err();
    assert!(
        error.starts_with("recording the end of lease 4.2"),
        "{error}"
    );
    assert_eq!(
        rig.helper.pool_status().leased,
        [(Slot::new(0), LeaseId::new(4, 2))]
    );

    // A lease from before the pool: its user goes, and the ledger cannot say so.
    let (rig, _) = legacy("ledger-fail-legacy", "4.4");
    rig.helper
        .lock()
        .fail_writes(&rig.dir.join("ledger"), false);
    let error = rig.helper.user_delete("4.4").unwrap_err();
    assert!(
        error.starts_with("recording the deletion of lease 4.4"),
        "{error}"
    );
    assert!(!rig.host.state().users.contains_key("kbf-lease-4-4"));
    exhausted(rig.helper.user_create("4.5", None));
}
