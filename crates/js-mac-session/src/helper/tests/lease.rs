//! `user-create`: a lease gets an account of its own, with a uid chosen after the last one
//! given; a lease id is used once; no lease user is ever an administrator (a request with a
//! grant is refused); a name that is too long is refused before anything is recorded; a
//! failure after `created` leaves the lease live; and a ledger that cannot record stops the
//! verb.

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use super::*;

/// The position of the first call that starts with `prefix`.
fn at(calls: &[String], prefix: &str) -> usize {
    calls
        .iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or_else(|| panic!("no call {prefix}: {calls:#?}"))
}

/// Catches: an account made with another name, uid, group or home than the lease's (the
/// agent derives the home from the name), a home made after the account (a crash between
/// would leave an account with no home), a password set (the account would then be able to
/// log in and could have a secure token), or a login shell that is not `/usr/bin/false`.
#[test]
fn a_lease_gets_an_account_of_its_own_with_a_new_home() {
    let rig = rig("account", 3);
    let base = me();
    assert_eq!(rig.helper.user_create("1.1"), Ok(base));
    let home = rig.dir.join("Users/kbf-lease-1-1");
    assert_eq!(
        rig.host.state().users["kbf-lease-1-1"],
        NewUser {
            name: "kbf-lease-1-1".to_owned(),
            uid: base,
            gid: my_gid(),
            home: home.clone(),
        }
    );
    assert!(home.is_dir());
    assert_eq!(
        rig.helper.lock().get(LeaseId::new(1, 1)),
        Some(Entry {
            uid: base,
            deleted: false
        })
    );
    let calls = log(&rig);
    assert!(calls.contains(&format!("make_home {} {base}:{}", home.display(), my_gid())));
    assert!(at(&calls, "make_home") < at(&calls, "create_user"));
    assert!(
        !calls.iter().any(|c| c.starts_with("reset_password")),
        "{calls:#?}"
    );
    assert_eq!(crate::helper::SHELL, "/usr/bin/false");
}

/// Catches: an account record made visible on the login window (not hidden), able to log in
/// (a shell other than `/usr/bin/false`, or any password or authentication authority, which
/// would give it a `ShadowHash` and a secure token), in a group but the lease's (an
/// administrator through `admin`, or any other), or under another name, uid, group or home
/// than the lease's.
#[test]
fn the_account_record_is_hidden_has_no_login_and_no_password_and_one_group() {
    let user = NewUser {
        name: "kbf-lease-1-1".to_owned(),
        uid: 650,
        gid: 20,
        home: PathBuf::from("/Users/kbf-lease-1-1"),
    };
    let attributes = crate::helper::account_attributes(&user);
    let want = [
        ("dsAttrTypeStandard:UniqueID", "650"),
        ("dsAttrTypeStandard:PrimaryGroupID", "20"),
        (
            "dsAttrTypeStandard:NFSHomeDirectory",
            "/Users/kbf-lease-1-1",
        ),
        ("dsAttrTypeStandard:UserShell", "/usr/bin/false"),
        ("dsAttrTypeStandard:RealName", "kbf-lease-1-1"),
        ("dsAttrTypeNative:IsHidden", "1"),
    ];
    let got: Vec<(&str, &str)> = attributes.iter().map(|(a, v)| (*a, v.as_str())).collect();
    assert_eq!(got, want);
    // Nothing that makes a password, an authentication authority or a group membership.
    for (attribute, _) in &attributes {
        for forbidden in [
            "Password",
            "AuthenticationAuthority",
            "ShadowHash",
            "GroupMembership",
        ] {
            assert!(!attribute.contains(forbidden), "{attribute}");
        }
    }
}

/// Catches: a real home made with the wrong mode or owner, one that takes over a folder
/// that is there (an account would start with what another left), one made through a link,
/// or an error of the system lost.
#[test]
fn the_home_is_made_mode_0700_and_never_taken_over() {
    let dir = scratch("make-home");
    let homes = dir.join("Users");
    std::fs::create_dir(&homes).unwrap();
    make_home(&homes, "kbf-lease-1-1", me(), my_gid()).unwrap();
    let made = std::fs::metadata(homes.join("kbf-lease-1-1")).unwrap();
    assert_eq!(made.permissions().mode() & 0o7777, 0o700);
    assert_eq!((made.uid(), made.gid()), (me(), my_gid()));

    // A folder that is there is refused, and what is in it stays.
    std::fs::write(homes.join("kbf-lease-1-1/left"), "x").unwrap();
    let error = make_home(&homes, "kbf-lease-1-1", me(), my_gid()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        error.to_string(),
        format!("the home {} exists", homes.join("kbf-lease-1-1").display())
    );
    assert!(homes.join("kbf-lease-1-1/left").exists());
    // So is a link at the name, whose target is never touched.
    std::os::unix::fs::symlink(dir.path(), homes.join("kbf-lease-1-2")).unwrap();
    let error = make_home(&homes, "kbf-lease-1-2", me(), my_gid()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    // A link as the folder of homes is not followed.
    std::os::unix::fs::symlink(&homes, dir.join("Users-link")).unwrap();
    assert!(make_home(&dir.join("Users-link"), "kbf-lease-1-3", me(), my_gid()).is_err());
    assert!(!homes.join("kbf-lease-1-3").exists());
    // Any other refusal of the system is the error.
    let error = make_home(&homes, "absent/child", me(), my_gid()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    if me() != 0 {
        let error = make_home(&homes, "kbf-lease-1-4", me() + 1, my_gid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}

/// Catches: a uid chosen from the first free one (it comes back as soon as it is free, not
/// as late as the range allows), one that does not wrap at the end of the range, a uid given
/// to two live leases, or a lease that gets a uid past the range.
#[test]
fn each_lease_gets_the_next_uid_after_the_last_given_and_the_range_wraps() {
    let rig = rig("next", 4);
    let base = me();
    assert_eq!(rig.helper.user_create("1.1"), Ok(base));
    assert_eq!(rig.helper.user_create("1.2"), Ok(base + 1));
    // 1.1 ends: its uid is free, but the next lease goes on after the last given.
    assert_eq!(rig.helper.user_delete("1.1"), Ok(true));
    assert_eq!(rig.helper.user_create("1.3"), Ok(base + 2));
    assert_eq!(rig.helper.user_create("1.4"), Ok(base + 3));
    // At the end of the range it wraps, to the uid 1.1 left.
    assert_eq!(rig.helper.user_create("1.5"), Ok(base));
    let error = rig.helper.user_create("1.6").unwrap_err();
    let range = UidRange::new(base, base + 3).unwrap();
    assert_eq!(
        error,
        format!(
            "no free uid in {range}: {}: held by a live lease; {}: held by a live lease; {}: held by a live lease; {}: held by a live lease",
            base + 1,
            base + 2,
            base + 3,
            base
        )
    );
    // The refused lease is not recorded, and a restart picks up where the ledger says.
    assert_eq!(rig.helper.lock().get(LeaseId::new(1, 6)), None);
    assert_eq!(rig.helper.lock().last_uid(), Some(base));
    let rebooted = rig_with("next", range, Some(rig.dir.clone()));
    assert_eq!(rebooted.helper.lock().last_uid(), Some(base));
}

/// Catches: a uid handed out although a user record has it, a process runs as it, or the
/// uid owns a directory under the per-user folders; each is passed over for the next free
/// one.
#[test]
fn a_uid_with_a_record_a_process_or_an_owned_folder_is_passed_over() {
    let rig = rig("busy", 4);
    let base = me();
    // The test owns what it makes, so it is the first uid's.
    std::fs::create_dir_all(rig.dir.join("folders/ab/cdef")).unwrap();
    rig.host.state().foreign.insert(base + 1);
    rig.host.state().procs.insert(base + 2, 1);
    assert_eq!(rig.helper.user_create("2.1"), Ok(base + 3));
    // The ledger gives the last uid, so the next one starts the search after it: all busy
    // but the first once its folder is gone, then the others once they are.
    std::fs::remove_dir_all(rig.dir.join("folders/ab/cdef")).unwrap();
    assert_eq!(rig.helper.user_create("2.2"), Ok(base));
    rig.host.state().foreign.clear();
    assert_eq!(rig.helper.user_create("2.3"), Ok(base + 1));
    rig.host.state().procs.clear();
    assert_eq!(rig.helper.user_create("2.4"), Ok(base + 2));
}

/// Catches: a uid handed out although the uid owns something in the places the sweep covers
/// (the new account would inherit it), or one passed over for something it does not own.
#[test]
fn a_uid_that_owns_something_in_a_swept_place_is_passed_over() {
    let rig = rig("busy-swept", 2);
    let base = me();
    std::fs::create_dir_all(rig.dir.join("Shared")).unwrap();
    std::fs::write(rig.dir.join("Shared/left"), "x").unwrap();
    assert_eq!(rig.helper.user_create("3.1"), Ok(base + 1));
    std::fs::remove_file(rig.dir.join("Shared/left")).unwrap();
    assert_eq!(rig.helper.user_create("3.2"), Ok(base));
}

/// Catches: a refusal that does not name why each uid is not free (the operator would have
/// to guess), one that names more than eight, one that counts the rest wrongly, and a uid
/// freed by a look that failed (an error is never "free").
#[test]
fn with_no_free_uid_the_refusal_names_why_each_is_not_free() {
    let rig = rig("all-busy", 4);
    let base = me();
    let range = UidRange::new(base, base + 3).unwrap();
    std::fs::create_dir_all(rig.dir.join("folders/ab/cdef")).unwrap();
    rig.host.state().foreign.insert(base + 1);
    rig.host.state().procs.insert(base + 2, 1);
    // The look at the last uid's record fails; the others' are fine.
    rig.host
        .state()
        .fail_script
        .insert("uid_taken", [false, false, false, true].into());
    assert_eq!(
        rig.helper.user_create("4.1"),
        Err(format!(
            "no free uid in {range}: {base}: {} is owned by it; {}: a user record has it; {}: 1 processes run as it; {}: looking up its record: uid_taken failed",
            rig.dir.join("folders/ab/cdef").display(),
            base + 1,
            base + 2,
            base + 3
        ))
    );
    assert_eq!(rig.helper.lock().get(LeaseId::new(4, 1)), None);

    // Ten uids, all with a record: eight are named, and the rest counted.
    let many = super::rig("all-busy-many", 10);
    for uid in many.helper.settings.range.uids() {
        many.host.state().foreign.insert(uid);
    }
    let named: Vec<String> = many
        .helper
        .settings
        .range
        .uids()
        .take(8)
        .map(|uid| format!("{uid}: a user record has it"))
        .collect();
    assert_eq!(
        many.helper.user_create("4.2"),
        Err(format!(
            "no free uid in {}: {}; and 2 more",
            many.helper.settings.range,
            named.join("; ")
        ))
    );
}

/// Catches: a failing look at a uid read as "free" (the account would be made on a uid whose
/// state is not known), and each look's failure unnamed: the directory, the process table,
/// the per-user folders, the places the sweep covers.
#[test]
fn an_error_in_looking_at_a_uid_is_not_free_and_is_named() {
    let rig = rig("look-errors", 1);
    let range = rig.helper.settings.range;
    let uid = me();
    let refused = |reason: &str| Err(format!("no free uid in {range}: {uid}: {reason}"));
    rig.host.state().fail.insert("uid_taken");
    assert_eq!(
        rig.helper.user_create("5.1"),
        refused("looking up its record: uid_taken failed")
    );
    rig.host.state().fail.clear();
    rig.host.state().fail.insert("live_processes");
    assert_eq!(
        rig.helper.user_create("5.1"),
        refused("listing its processes: live_processes failed")
    );
    rig.host.state().fail.clear();
    // The per-user folders cannot be searched (a file where the folders are).
    std::fs::write(rig.dir.join("folders"), "x").unwrap();
    let error = rig.helper.user_create("5.1").unwrap_err();
    let want = format!(
        "no free uid in {range}: {uid}: searching {}",
        rig.dir.join("folders").display()
    );
    assert!(error.starts_with(&want), "{error}");
    std::fs::remove_file(rig.dir.join("folders")).unwrap();
    // The places the sweep covers cannot be looked in.
    std::fs::write(rig.dir.join("Shared"), "x").unwrap();
    let error = rig.helper.user_create("5.1").unwrap_err();
    let want = format!("no free uid in {range}: {uid}: looking in the swept places: ");
    assert!(error.starts_with(&want), "{error}");
    std::fs::remove_file(rig.dir.join("Shared")).unwrap();
    // Nothing was recorded, and with the looks right the lease gets the uid.
    assert_eq!(rig.helper.lock().get(LeaseId::new(5, 1)), None);
    assert_eq!(rig.helper.user_create("5.1"), Ok(uid));

    // One failing look passes over that uid only: the next is looked at and taken.
    let second = super::rig("look-error-one", 2);
    second
        .host
        .state()
        .fail_script
        .insert("uid_taken", [true].into());
    assert_eq!(second.helper.user_create("5.2"), Ok(me() + 1));
    assert_eq!(second.helper.lock().held_uids(), BTreeSet::from([me() + 1]));
}

/// Catches: a lease id used twice, including after its lease ended, and including across a
/// restart of the helper.
#[test]
fn a_lease_id_is_never_used_twice() {
    let rig = rig("reuse", 4);
    rig.helper.user_create("2.1").unwrap();
    let again = rig.helper.user_create("2.1").unwrap_err();
    assert_eq!(again, "lease 2.1 was used before; a lease id is used once");
    rig.helper.user_delete("2.1").unwrap();
    assert!(
        rig.helper
            .user_create("2.1")
            .unwrap_err()
            .contains("was used before")
    );
    // A new helper over the same ledger (a reboot) remembers.
    let range = UidRange::new(me(), me() + 3).unwrap();
    let rebooted = rig_with("reuse", range, Some(rig.dir.clone()));
    let after = rebooted.helper.user_create("2.1").unwrap_err();
    assert!(after.contains("was used before"), "{after}");
    assert!(
        rig.helper
            .user_create("2.01")
            .unwrap_err()
            .contains("not <term>.<seq>")
    );
}

/// Catches: an account name past 31 bytes accepted (the directory service would cut or refuse
/// it), or refused after something was recorded or made: the lease id stays unused.
#[test]
fn a_name_that_is_too_long_is_refused_before_anything_is_recorded() {
    let rig = rig("long-name", 2);
    let error = rig
        .helper
        .user_create("1099511627775.12345678")
        .unwrap_err();
    assert_eq!(
        error,
        "lease 1099511627775.12345678 would have the account name \
         kbf-lease-1099511627775-12345678, 32 bytes; an account name is at most 31"
    );
    assert_eq!(
        rig.helper
            .lock()
            .get(LeaseId::new(1_099_511_627_775, 12_345_678)),
        None
    );
    assert!(rig.helper.lock().live_leases().is_empty());
    assert_eq!(log(&rig), Vec::<String>::new());
    // One byte shorter is made.
    assert_eq!(rig.helper.user_create("1099511627775.1234567"), Ok(me()));
}

/// Catches: a lease user made an administrator, or a lease taken, for a `user-create` that
/// carries a grant (the field a release with signed admin grants sent), whatever it holds;
/// and a refused grant that spends the lease id or a uid.
#[test]
fn a_user_create_with_a_grant_is_refused_and_makes_nothing() {
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
    assert_eq!(log(&rig), Vec::<String>::new());
    assert_eq!(
        rig.helper.lock().get(LeaseId::new(3, 1)),
        None,
        "the id is not spent"
    );
    // The same lease without a grant gets the first uid.
    assert_eq!(rig.helper.user_create("3.1"), Ok(me()));
    assert!(rig.helper.lock().held_uids().contains(&me()));
}

/// Catches: a lease not recorded before its home is made (a crash after the home and before
/// the record would leave a home no lease holds); a failure after `created` that frees the
/// uid (the home or the account made would then be inherited) or that cannot be ended: the
/// lease stays live, its uid held, and `user-delete` deletes whatever was made, so the
/// delete finds no record, the proof holds and `deleted` is written (the crash point
/// between `created` and the account).
#[test]
fn a_failure_after_created_leaves_the_lease_live_and_its_delete_ends_it() {
    // The home cannot be made: nothing but the record is left.
    let rig = rig("after-created", 3);
    rig.host.state().fail.insert("make_home");
    let error = rig.helper.user_create("6.1").unwrap_err();
    assert_eq!(
        error,
        format!(
            "making {}: make_home failed",
            rig.dir.join("Users/kbf-lease-6-1").display()
        )
    );
    rig.host.state().fail.clear();
    assert_eq!(
        rig.helper.lock().get(LeaseId::new(6, 1)),
        Some(Entry {
            uid: me(),
            deleted: false
        })
    );
    assert!(!log(&rig).iter().any(|c| c.starts_with("create_user")));
    assert!(rig.host.state().users.is_empty());
    assert_eq!(rig.helper.user_create("6.2"), Ok(me() + 1));
    assert_eq!(rig.helper.user_delete("6.1"), Ok(false));
    assert_eq!(rig.helper.lock().held_uids(), BTreeSet::from([me() + 1]));

    // The account cannot be made: the home is there, and the delete takes it.
    rig.host.state().fail.insert("create_user");
    let error = rig.helper.user_create("6.3").unwrap_err();
    assert_eq!(error, "creating user kbf-lease-6-3: create_user failed");
    rig.host.state().fail.clear();
    let home = rig.dir.join("Users/kbf-lease-6-3");
    assert!(home.is_dir());
    assert!(rig.helper.lock().held_uids().contains(&(me() + 2)));
    assert_eq!(rig.helper.user_delete("6.3"), Ok(false));
    assert!(!home.exists());
    assert!(
        rig.host
            .state()
            .users
            .keys()
            .all(|name| name != "kbf-lease-6-3")
    );
}

/// Catches: a home that is already there taken over for the new account (it would hold what
/// another left), or a refusal that spends nothing: the lease is recorded (so the delete
/// that follows sweeps the path), no account is made, and the delete ends the lease.
#[test]
fn a_home_that_exists_is_refused_and_the_delete_removes_it() {
    let rig = rig("home-exists", 2);
    let home = rig.dir.join("Users/kbf-lease-7-1");
    std::fs::create_dir_all(home.join("Library")).unwrap();
    std::fs::write(home.join("Library/left"), "x").unwrap();
    let error = rig.helper.user_create("7.1").unwrap_err();
    assert_eq!(
        error,
        format!("making {}: File exists (os error 17)", home.display())
    );
    assert!(!log(&rig).iter().any(|c| c.starts_with("create_user")));
    assert!(
        home.join("Library/left").exists(),
        "nothing of it was touched"
    );
    assert!(rig.helper.lock().held_uids().contains(&me()));
    assert_eq!(rig.helper.user_delete("7.1"), Ok(false));
    assert!(!home.exists());
}

/// Catches: a lease recorded when the ledger could not record it (after a reboot the
/// lease id would be reused, or the uid given twice), or its end reported done when the
/// ledger could not say so.
#[test]
fn a_ledger_that_cannot_record_stops_the_verb() {
    let rig = rig("ledger-fail", 3);
    rig.helper.user_create("4.2").unwrap();
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"));
    log(&rig);
    let error = rig.helper.user_create("4.3").unwrap_err();
    assert!(error.starts_with("recording lease 4.3"), "{error}");
    // Nothing was made for it.
    assert!(!log(&rig).iter().any(|c| c.starts_with("make_home")));
    // The account of 4.2 is deleted and shown gone, and the ledger cannot say so: the lease
    // stays live, its uid held, and the next delete finds the account gone.
    let error = rig.helper.user_delete("4.2").unwrap_err();
    assert!(
        error.starts_with("recording the deletion of lease 4.2"),
        "{error}"
    );
    assert!(!rig.host.state().users.contains_key("kbf-lease-4-2"));
    assert_eq!(rig.helper.lock().held_uids(), BTreeSet::from([me()]));
}
