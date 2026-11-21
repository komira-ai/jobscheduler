//! A pool user is never deleted: `pool-create` adopts the pool's own users that are
//! already on the Mac (and refuses any other record on a slot's uid), and an
//! administrator's lease adds its user to `admin` and has it taken out at its end.

use std::path::PathBuf;

use super::tests::{entry, grant, home, records, restarted, slots};
use crate::helper::tests::{Rig, log, me, my_gid, rig, rig_with};
use crate::helper::{DSEDITGROUP, Host as _, NewUser, UserRecord};
use crate::lease::UidRange;

/// Catches a first install on a Mac that already has the pool's users (an earlier install
/// left them, and they can never be deleted) that cannot use them, or that deletes or
/// makes them again: with no ledger, `pool-create` adopts each user record that is the
/// pool's own, kills what runs as it, empties its home and its per-user folder, sets its
/// record back (group, shell, `admin` membership) and records the slot free; no record is
/// made or deleted.
#[test]
fn pool_create_adopts_the_pool_s_own_users_a_lost_ledger_left() {
    let rig = rig("pool-adopt", 2);
    let uid = me();
    let name = |n: u32| format!("kbf-lease-pool-{n}");
    std::fs::create_dir_all(home(&rig, 0)).unwrap();
    std::fs::write(home(&rig, 0).join("left"), "x").unwrap();
    let kept = entry(&rig, uid);
    std::fs::create_dir_all(kept.join("T")).unwrap();
    std::fs::write(kept.join("T/scratch"), "x").unwrap();
    {
        let mut state = rig.host.state();
        state.procs.insert(uid + 1, 1);
        state.users.get_mut(&name(0)).unwrap().admin = true;
        state.records.insert(
            name(1),
            UserRecord {
                uid: uid + 1,
                gid: my_gid() + 5,
                home: home(&rig, 1),
                hidden: true,
                shell: "/bin/sh".to_owned(),
            },
        );
    }
    // The ledger is gone; the users stay.
    std::fs::remove_file(rig.dir.join("ledger")).unwrap();
    std::fs::remove_file(rig.dir.join("pool")).unwrap();
    let rig = restarted(rig, "pool-adopt");
    log(&rig);
    rig.helper.boot_reset(&[]).unwrap();
    let calls = log(&rig);
    assert_eq!(records(&calls), Vec::<String>::new(), "{calls:#?}");
    let status = rig.helper.pool_status();
    assert_eq!(status.free, slots(&[0, 1]));
    assert_eq!(status.missing, slots(&[]));
    let state = rig.host.state();
    assert_eq!(state.procs.get(&(uid + 1)), None);
    assert!(!state.users[&name(0)].admin);
    assert!(!home(&rig, 0).join("left").exists());
    assert!(home(&rig, 0).is_dir());
    assert!(kept.join("T").is_dir());
    assert!(!kept.join("T/scratch").exists());
    assert!(state.records.is_empty(), "set back: {:?}", state.records);
    drop(state);
    assert_eq!(
        rig.host.user_record(&name(1)).unwrap().unwrap().gid,
        my_gid()
    );
    assert_eq!(rig.helper.user_create("86.1", None), Ok(uid));
}

/// Catches a record that is not the pool's own adopted because it has the uid: a record
/// of another name, one that is not hidden, one with another home, or one whose uid is
/// not the slot's is refused, naming why, and the slot is left unmade; nothing of it is
/// reset, made or deleted. A directory that cannot be asked, and an adopted user whose
/// reset cannot finish, leave the slot unmade or broken with the reason.
#[test]
fn a_record_on_a_slot_s_uid_that_is_not_the_pool_s_own_is_refused() {
    let uid = me();
    let name = "kbf-lease-pool-0";
    let pool = |label: &str| rig_with(label, UidRange::new(uid, uid).unwrap(), None);
    let own = |rig: &Rig| NewUser {
        name: name.to_owned(),
        uid,
        gid: my_gid(),
        home: home(rig, 0),
        admin: false,
    };
    let record = |rig: &Rig| UserRecord {
        uid,
        gid: my_gid(),
        home: home(rig, 0),
        hidden: true,
        shell: "/bin/zsh".to_owned(),
    };
    type Change = fn(&mut UserRecord);
    let cases: [(&str, Change, &str); 3] = [
        ("hidden", |r| r.hidden = false, "it is not hidden"),
        (
            "home",
            |r| r.home = PathBuf::from("/Users/elsewhere"),
            "its home is /Users/elsewhere",
        ),
        ("uid", |r| r.uid += 100, "its uid is "),
    ];
    for (label, change, why) in cases {
        let rig = pool(&format!("pool-refuse-{label}"));
        let mut changed = record(&rig);
        change(&mut changed);
        {
            let mut state = rig.host.state();
            state.users.insert(name.to_owned(), own(&rig));
            state.records.insert(name.to_owned(), changed);
        }
        let created = rig.helper.pool_create();
        assert_eq!(created.missing, slots(&[0]), "{label}");
        assert_eq!(created.errors.len(), 1, "{label}");
        assert!(
            created.errors[0].starts_with(&format!(
                "slot 0: a user record the ledger does not hold has uid {uid} and is not the \
                 pool's own {name} ("
            )) && created.errors[0].contains(why)
                && created.errors[0].ends_with("); it is not adopted"),
            "{label}: {:?}",
            created.errors
        );
        let calls = log(&rig);
        assert!(
            !calls.iter().any(|c| c.starts_with("restore_user")
                || c.starts_with("create_user")
                || c.starts_with("delete_user")),
            "{label}: {calls:#?}"
        );
    }

    // A record of another name has the uid: the pool's own name has none.
    let rig = pool("pool-refuse-name");
    rig.host.state().foreign.insert(uid);
    assert_eq!(
        rig.helper.pool_create().errors,
        [format!(
            "slot 0: a user record the ledger does not hold has uid {uid} and is not \
             kbf-lease-pool-0; it is not adopted"
        )]
    );

    // The directory cannot be asked.
    let rig = pool("pool-refuse-ask");
    rig.host.state().users.insert(name.to_owned(), own(&rig));
    rig.host.state().fail.insert("user_record");
    assert_eq!(
        rig.helper.pool_create().errors,
        ["slot 0: reading the record kbf-lease-pool-0: user_record failed"]
    );

    // An own record whose reset cannot finish: adopted, then broken, with the reason.
    let rig = pool("pool-adopt-broken");
    rig.host.state().users.insert(name.to_owned(), own(&rig));
    rig.host.state().fail.insert("restore_user");
    let created = rig.helper.pool_create();
    assert_eq!(created.broken, slots(&[0]));
    assert_eq!(
        created.errors,
        [
            "slot 0: slot 0's user kbf-lease-pool-0 could not be reset in place, so the \
             slot is broken and lent to no one until pool-create resets it again: setting \
             back the record of user kbf-lease-pool-0: restore_user failed"
        ]
    );
}

/// Catches an administrator's lease that leaves a membership the next lease inherits,
/// and a grant that keeps a slot it could not use: a lease with a grant has its user in
/// `admin` while it runs, and when it ends the user is taken out, asked about and free;
/// a slot whose user the directory still says is a member after the reset is broken.
#[test]
fn an_administrator_s_user_is_added_for_the_lease_and_removed_at_its_end() {
    let rig = rig("pool-admin-lease", 1);
    rig.helper
        .user_create("83.2", Some(&grant("83.2")))
        .unwrap();
    assert!(rig.host.state().users["kbf-lease-pool-0"].admin);
    log(&rig);
    assert_eq!(rig.helper.user_delete("83.2"), Ok(true));
    let calls = log(&rig);
    let removed = calls
        .iter()
        .position(|c| c == "set_admin kbf-lease-pool-0 false")
        .unwrap();
    let asked = calls
        .iter()
        .position(|c| c.starts_with(&format!("command {DSEDITGROUP} -o checkmember")))
        .unwrap();
    assert!(removed < asked, "{calls:#?}");
    assert_eq!(records(&calls), Vec::<String>::new());
    assert!(!rig.host.state().users["kbf-lease-pool-0"].admin);
    assert_eq!(rig.helper.pool_status().free, slots(&[0]));

    // A removal that does not take is found by the directory's answer: the slot is
    // broken, not retired for good, and a later reset that works frees it.
    rig.helper
        .user_create("83.3", Some(&grant("83.3")))
        .unwrap();
    rig.host.state().answers.insert(
        format!("{DSEDITGROUP} -o checkmember -m kbf-lease-pool-0 admin"),
        crate::autologin::CommandOutput {
            success: true,
            stdout: "yes kbf-lease-pool-0 is a member of admin\n".to_owned(),
            stderr: String::new(),
        },
    );
    let error = rig.helper.user_delete("83.3").unwrap_err();
    assert!(
        error.ends_with("the directory says it is a member"),
        "{error}"
    );
    assert_eq!(rig.helper.pool_status().broken, slots(&[0]));
    rig.host.state().answers.clear();
    assert_eq!(rig.helper.pool_create().free, slots(&[0]));
}
