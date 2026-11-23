//! `user-create` against the pool: a lease takes the lowest free pool user and makes no
//! user, a lease id is used once, no lease user is ever an administrator (a request with a
//! grant is refused), and a ledger that cannot record stops the verb.

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
        }
    );
    assert_eq!(rig.helper.user_create("1.1"), Ok(base));
    assert_eq!(rig.helper.user_create("1.2"), Ok(base + 1));
    // 1.1 ends: its slot is the lowest free one again.
    rig.helper.user_delete("1.1").unwrap();
    assert_eq!(rig.helper.user_create("1.3"), Ok(base));
    assert_eq!(rig.helper.user_create("1.4"), Ok(base + 2));
    exhausted(rig.helper.user_create("1.5"));
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
    assert_eq!(shrunk.helper.user_create("1.6"), Ok(base));
    exhausted(shrunk.helper.user_create("1.7"));
    shrunk.helper.user_delete("1.6").unwrap();
    // Nor one whose uid the range no longer holds.
    let narrowed = rig_with(
        "alloc",
        UidRange::new(base + 2, base + 2).unwrap(),
        Some(rig.dir.clone()),
    );
    narrowed.helper.boot_reset(&[]).unwrap();
    exhausted(narrowed.helper.user_create("1.8"));
}

/// Catches: a lease id used twice,
/// including after its lease ended, and including across a restart of the helper.
#[test]
fn a_lease_id_is_never_used_twice() {
    let rig = rig("reuse", 4);
    rig.helper.user_create("2.1").unwrap();
    let again = rig.helper.user_create("2.1").unwrap_err().to_string();
    assert_eq!(again, "lease 2.1 was used before; a lease id is used once");
    rig.helper.user_delete("2.1").unwrap();
    assert!(
        rig.helper
            .user_create("2.1")
            .unwrap_err()
            .to_string()
            .contains("was used before")
    );
    // A new helper over the same ledger (a reboot) remembers.
    let range = UidRange::new(me(), me() + 3).unwrap();
    let rebooted = rig_with("reuse", range, Some(rig.dir.clone()));
    let after = rebooted.helper.user_create("2.1").unwrap_err().to_string();
    assert!(after.contains("was used before"), "{after}");
    assert!(
        rig.helper
            .user_create("2.01")
            .unwrap_err()
            .to_string()
            .contains("not <term>.<seq>")
    );
}

/// Catches: a lease user made an administrator, or a lease taken, for a `user-create` that
/// carries a grant (the field a release with signed admin grants sent), whatever it holds;
/// and a refused grant that spends the lease id or a slot.
#[test]
fn a_user_create_with_a_grant_is_refused_and_takes_no_user() {
    let rig = rig("admin", 2);
    let with_grant = |lease: &str, grant: &str| Request::UserCreate {
        lease: lease.to_owned(),
        grant: Some(grant.to_owned()),
    };
    for grant in ["a.b", "", "kbf-grant-v1"] {
        let Outcome::Reply(reply) = rig.helper.handle(with_grant("3.1", grant), Vec::new()) else {
            panic!("not a reply");
        };
        assert_eq!(
            reply,
            Reply::Refused {
                reason: NO_ADMINISTRATOR.to_owned()
            },
            "grant {grant:?}"
        );
    }
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("create_user") || c.starts_with("delete_user")),
        "{calls:#?}"
    );
    assert_eq!(
        rig.helper.lock().get(LeaseId::new(3, 1)),
        None,
        "the id is not spent"
    );
    assert_eq!(rig.helper.pool_status().free, [Slot::new(0), Slot::new(1)]);
    // The same lease without a grant takes the lowest slot, and its user is no member of
    // `admin`.
    assert_eq!(rig.helper.user_create("3.1"), Ok(me()));
    assert!(rig.host.state().admins.is_empty());
}

/// Catches: a lease recorded when the ledger could not record it (after a reboot the
/// lease id would be reused, or the slot lent twice), or its end reported done when the
/// ledger could not say so.
#[test]
fn a_ledger_that_cannot_record_stops_the_verb() {
    let rig = rig("ledger-fail", 2);
    rig.helper.user_create("4.2").unwrap();
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"), true);
    let error = rig.helper.user_create("4.3").unwrap_err().to_string();
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
    exhausted(rig.helper.user_create("4.5"));
}
