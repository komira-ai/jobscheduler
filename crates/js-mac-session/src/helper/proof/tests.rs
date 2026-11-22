//! The proof that a deleted account is gone, item by item: each item that does not hold
//! (or cannot be looked at) fails the delete naming it, leaves the lease live and its uid
//! held and writes no `deleted`; the ledger says `deleted` only after the proof; and a delete
//! that finds nothing of the account left (a record already gone) is no error.

use std::path::{Path, PathBuf};

use super::*;
use crate::helper::NewUser;
use crate::helper::tests::{Rig, log, one_uid, uid_held};
use crate::lease::user_name;
use crate::testing::{AfterDelete, State};

/// Deletes the lease `lease` of a rig of one uid, `make` giving what shows up on the fake host
/// just after the record is deleted (it is given the rig's directory and the uid). Returns the
/// rig, the uid, the account's name and the refusal.
fn refused_after(
    name: &str,
    lease: &str,
    make: impl FnOnce(&Path, u32, &str) -> AfterDelete,
) -> (Rig, u32, String, String) {
    let (rig, uid) = one_uid(name, lease);
    let account = user_name(crate::lease::parse_lease(lease).unwrap());
    let after = make(rig.dir.path(), uid, &account);
    rig.host.state().after_delete = Some(after);
    let error = rig.helper.user_delete(lease).unwrap_err();
    (rig, uid, account, error)
}

/// Asserts what a failed proof must leave: the lease live in the ledger, no `deleted`
/// written, and the uid held (no lease gets it).
fn lease_stays_live(rig: &Rig, lease: &str, uid: u32) {
    let id = crate::lease::parse_lease(lease).unwrap();
    assert_eq!(rig.helper.lock().get(id).map(|e| e.deleted), Some(false));
    let text = std::fs::read_to_string(rig.dir.join("ledger")).unwrap();
    assert!(!text.contains("deleted"), "{text}");
    assert!(rig.helper.lock().held_uids().contains(&uid));
    uid_held(rig);
}

/// Catches: a deletion taken as complete while a record still has the uid (a record made
/// by another name, or one a directory cache still answers for), or one that has the
/// account's name, or a lookup that failed read as "no record".
#[test]
fn a_record_that_remains_by_name_or_by_uid_keeps_the_lease_live() {
    let (rig, uid, _, error) = refused_after("proof-uid", "10.1", |_, uid, _| {
        Box::new(move |state: &mut State| {
            state.foreign.insert(uid);
        })
    });
    assert_eq!(
        error,
        format!("not shown gone (record): a record has uid {uid}")
    );
    lease_stays_live(&rig, "10.1", uid);

    let (rig, uid, _, error) = refused_after("proof-name", "10.2", |dir, uid, account| {
        let user = NewUser {
            name: account.to_owned(),
            uid: uid + 1,
            gid: 0,
            home: dir.join("Users").join(account),
        };
        Box::new(move |state: &mut State| {
            state.users.insert(user.name.clone(), user.clone());
        })
    });
    assert_eq!(
        error,
        "not shown gone (record): a record named kbf-lease-10-2 remains"
    );
    lease_stays_live(&rig, "10.2", uid);

    // A lookup that fails: the first `account` call is the delete's own, the second the proof's.
    let (rig, uid) = one_uid("proof-lookup", "10.3");
    rig.host
        .state()
        .fail_script
        .insert("account", [false, true].into());
    assert_eq!(
        rig.helper.user_delete("10.3").unwrap_err(),
        "not shown gone (record): looking up kbf-lease-10-3: account failed"
    );
    lease_stays_live(&rig, "10.3", uid);
    rig.host
        .state()
        .fail_script
        .insert("uid_taken", [true].into());
    assert_eq!(
        rig.helper.user_delete("10.3").unwrap_err(),
        format!("not shown gone (record): looking up uid {uid}: uid_taken failed")
    );
    lease_stays_live(&rig, "10.3", uid);
}

/// Catches: a deletion taken as complete while a process of the uid runs (one started
/// after the last look), while a launchd domain of the uid still prints, or when the
/// process table or the domain's record of a GUI teardown could not be read.
#[test]
fn a_process_or_a_domain_that_remains_keeps_the_lease_live() {
    let (rig, uid, _, error) = refused_after("proof-process", "11.1", |_, uid, _| {
        Box::new(move |state: &mut State| {
            state.procs.insert(uid, 1);
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (processes): 1 processes of uid {uid} remain after the account was deleted: 1000 (proc0)"
        )
    );
    lease_stays_live(&rig, "11.1", uid);

    let (rig, uid, _, error) = refused_after("proof-domain", "11.2", |_, uid, _| {
        Box::new(move |state: &mut State| {
            state.domains.insert(format!("user/{uid}"));
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (processes): launchd domains of uid {uid} remain after the account was deleted: user/{uid}"
        )
    );
    lease_stays_live(&rig, "11.2", uid);

    let (rig, uid, _, error) = refused_after("proof-table", "11.3", |_, _, _| {
        Box::new(|state: &mut State| {
            state.fail.insert("live_processes");
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (processes): listing the processes of uid {uid}: live_processes failed"
        )
    );
    lease_stays_live(&rig, "11.3", uid);

    // The record of an owed GUI teardown is a link: refused, never followed.
    let (rig, uid, _, error) = refused_after("proof-gui-record", "11.4", |dir, uid, _| {
        let record = dir.join(format!("gui-teardown.{uid}"));
        Box::new(move |_: &mut State| {
            std::os::unix::fs::symlink(dir_of(&record), &record).unwrap();
        })
    });
    assert!(
        error.starts_with(&format!(
            "not shown gone (processes): the GUI teardown record of uid {uid} ("
        )),
        "{error}"
    );
    lease_stays_live(&rig, "11.4", uid);
}

/// The directory of `path`: a place for a link to point at.
fn dir_of(path: &Path) -> PathBuf {
    path.parent().unwrap().to_owned()
}

/// Catches: a deletion taken as complete while the home path exists (a folder made again
/// after its removal, or a link), or when the path cannot be looked at (the system's
/// answer other than "no such file or directory" is not "gone").
#[test]
fn a_home_that_exists_or_cannot_be_looked_at_keeps_the_lease_live() {
    let (rig, uid, account, error) = refused_after("proof-home", "12.1", |dir, _, account| {
        let home = dir.join("Users").join(account);
        Box::new(move |_: &mut State| std::fs::create_dir(&home).unwrap())
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (home): the home {} exists",
            rig.dir.join("Users").join(&account).display()
        )
    );
    lease_stays_live(&rig, "12.1", uid);

    // A link where the home was counts as the path existing, and is not followed.
    let (rig, uid, account, error) = refused_after("proof-home-link", "12.2", |dir, _, account| {
        let home = dir.join("Users").join(account);
        let target = dir.to_owned();
        Box::new(move |_: &mut State| std::os::unix::fs::symlink(&target, &home).unwrap())
    });
    assert!(
        error.starts_with("not shown gone (home): the home ") && error.ends_with(" exists"),
        "{error}"
    );
    assert!(rig.dir.join("Users").join(&account).is_symlink());
    lease_stays_live(&rig, "12.2", uid);

    // The folder of the homes is a file: the path cannot be looked at.
    let (rig, uid, account, error) = refused_after("proof-home-error", "12.3", |dir, _, _| {
        let homes = dir.join("Users");
        Box::new(move |_: &mut State| {
            std::fs::remove_dir_all(&homes).unwrap();
            std::fs::write(&homes, "x").unwrap();
        })
    });
    let home = rig.dir.join("Users").join(&account);
    assert_eq!(
        error,
        format!(
            "not shown gone (home): looking at {}: Not a directory (os error 20)",
            home.display()
        )
    );
    lease_stays_live(&rig, "12.3", uid);
}

/// Catches: a deletion taken as complete while a directory two levels down in the per-user
/// folders is the uid's (the system's `<xx>/<hash>` entry, left or made again), or when the
/// folders cannot be searched; and a refusal that names more paths than it can hold.
#[test]
fn an_entry_of_the_uid_in_the_per_user_folders_keeps_the_lease_live() {
    let (rig, uid, _, error) = refused_after("proof-folders", "13.1", |dir, _, _| {
        let entry = dir.join("folders/ab/cdef");
        Box::new(move |_: &mut State| std::fs::create_dir_all(&entry).unwrap())
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (per-user folders): owned by uid {uid}: {}",
            rig.dir.join("folders/ab/cdef").display()
        )
    );
    lease_stays_live(&rig, "13.1", uid);

    let (rig, uid, _, error) = refused_after("proof-folders-error", "13.2", |dir, _, _| {
        let folders = dir.join("folders");
        Box::new(move |_: &mut State| std::fs::write(&folders, "x").unwrap())
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (per-user folders): searching {} for uid {uid}'s entries: Not a directory (os error 20)",
            rig.dir.join("folders").display()
        )
    );
    lease_stays_live(&rig, "13.2", uid);
}

/// Catches: a deletion taken as complete while a place the sweep covers holds an entry the
/// uid owns, or an entry named for the account (its crontab), or when a place cannot be
/// looked in.
#[test]
fn something_of_the_uid_in_a_swept_place_keeps_the_lease_live() {
    let (rig, uid, _, error) = refused_after("proof-places", "14.1", |dir, _, _| {
        let shared = dir.join("Shared");
        Box::new(move |_: &mut State| {
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::write(shared.join("left"), "x").unwrap();
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (swept places): left of uid {uid}'s or kbf-lease-14-1's: {}",
            rig.dir.join("Shared/left").display()
        )
    );
    lease_stays_live(&rig, "14.1", uid);

    // The crontab named for the account, owned by no one the sweep knows.
    let (rig, uid, _, error) = refused_after("proof-crontab", "14.2", |dir, _, account| {
        let tabs = dir.join("tabs");
        let tab = tabs.join(account);
        Box::new(move |_: &mut State| {
            std::fs::create_dir_all(&tabs).unwrap();
            std::fs::write(&tab, "x").unwrap();
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (swept places): left of uid {uid}'s or kbf-lease-14-2's: {}",
            rig.dir.join("tabs/kbf-lease-14-2").display()
        )
    );
    lease_stays_live(&rig, "14.2", uid);

    let (rig, uid, _, error) = refused_after("proof-places-error", "14.3", |dir, _, _| {
        let shared = dir.join("Shared");
        Box::new(move |_: &mut State| std::fs::write(&shared, "x").unwrap())
    });
    assert!(
        error
            .starts_with("not shown gone (swept places): looking in the places the sweep covers: "),
        "{error}"
    );
    lease_stays_live(&rig, "14.3", uid);
}

/// Catches: a deletion taken as complete while a System V IPC object of the uid is listed
/// (by the uid's number, as `ipcs` names an owner whose record is gone), or when `ipcs`
/// could not be run; and a proof that lists no more than the removal did when it found
/// none (the object a job made after that listing would never be seen).
#[test]
fn an_ipc_object_of_the_uid_keeps_the_lease_live() {
    let (rig, uid, _, error) = refused_after("proof-ipc", "15.1", |_, uid, _| {
        Box::new(move |state: &mut State| {
            state.ipc.insert(77, ('m', uid.to_string()));
        })
    });
    assert_eq!(
        error,
        format!(
            "not shown gone (IPC): IPC objects of uid {uid} remain after the account was deleted: -m 77"
        )
    );
    lease_stays_live(&rig, "15.1", uid);

    let (rig, uid, _, error) = refused_after("proof-ipc-error", "15.2", |_, _, _| {
        Box::new(|state: &mut State| {
            state.fail.insert("command");
        })
    });
    assert_eq!(
        error,
        format!("not shown gone (IPC): listing the IPC objects of uid {uid}: command failed")
    );
    lease_stays_live(&rig, "15.2", uid);

    // With no object listed at the removal, `ipcs` is run again for the proof.
    let (rig, _) = one_uid("proof-ipc-fresh", "15.3");
    assert_eq!(rig.helper.user_delete("15.3"), Ok(true));
    let calls = log(&rig);
    let lists = calls
        .iter()
        .filter(|c| c.starts_with("command /usr/bin/ipcs"))
        .count();
    assert_eq!(lists, 2, "{calls:#?}");
}

/// Catches: `deleted` written before the proof (a crash between would free a uid whose
/// account was not shown gone), a proof made before the record was deleted (it would show
/// the record, not its absence), or a delete that is refused for a record that is already
/// gone (a delete repeated after a crash, or made for an account never made).
#[test]
fn deleted_is_written_only_after_the_account_is_shown_gone() {
    let (rig, uid) = one_uid("proof-order", "16.1");
    // What the ledger says at the moment the record is deleted: still live.
    let ledger = rig.dir.join("ledger");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let at_delete = seen.clone();
    rig.host.state().after_delete = Some(Box::new(move |_: &mut State| {
        *at_delete.lock().unwrap() = std::fs::read_to_string(&ledger).unwrap();
    }));
    assert_eq!(rig.helper.user_delete("16.1"), Ok(true));
    assert_eq!(*seen.lock().unwrap(), format!("created 16.1 {uid}\n"));
    let text = std::fs::read_to_string(rig.dir.join("ledger")).unwrap();
    assert_eq!(text, format!("created 16.1 {uid}\ndeleted 16.1\n"));
    // The proof looks after the record went.
    let calls = log(&rig);
    let position = |prefix: &str, from: usize| {
        calls
            .iter()
            .skip(from)
            .position(|call| call.starts_with(prefix))
            .map(|at| at + from)
            .unwrap_or_else(|| panic!("no {prefix} after {from}: {calls:#?}"))
    };
    let deleted = position("delete_user kbf-lease-16-1", 0);
    assert!(position("account kbf-lease-16-1", deleted) > deleted);
    assert!(position("uid_taken", deleted) > deleted);

    // A record already gone is no error: the proof holds and `deleted` is written.
    let (rig, uid) = one_uid("proof-missing", "16.2");
    rig.host.state().users.clear();
    assert_eq!(rig.helper.user_delete("16.2"), Ok(false));
    assert_eq!(
        std::fs::read_to_string(rig.dir.join("ledger")).unwrap(),
        format!("created 16.2 {uid}\ndeleted 16.2\n")
    );
}

/// Catches: a refused delete that cannot be completed by repeating it once the cause is
/// gone (the lease would be held for good), or a uid freed by the repeat while what held it
/// is still there.
#[test]
fn a_delete_repeated_once_the_cause_is_gone_completes_and_frees_the_uid() {
    let (rig, uid, _, error) = refused_after("proof-repeat", "17.1", |dir, _, _| {
        let shared = dir.join("Shared");
        Box::new(move |_: &mut State| {
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::write(shared.join("left"), "x").unwrap();
        })
    });
    assert!(error.contains("left of uid"), "{error}");
    lease_stays_live(&rig, "17.1", uid);
    // The record is gone already, and the delete sweeps what the uid owns in `Shared`
    // before it shows the account gone, so repeating it completes.
    assert_eq!(rig.helper.user_delete("17.1"), Ok(false));
    assert!(!rig.dir.join("Shared/left").exists());
    assert_eq!(rig.helper.user_create("17.2"), Ok(uid));
}

/// Catches: a refusal that names the paths without a bound (a frame would not hold it), or
/// that counts the rest wrongly.
#[test]
fn a_refusal_names_at_most_eight_paths_and_counts_the_rest() {
    let path = |n: usize| PathBuf::from(format!("/p/{n}"));
    let few: Vec<PathBuf> = (0..8).map(path).collect();
    assert_eq!(
        listed(&few),
        "/p/0, /p/1, /p/2, /p/3, /p/4, /p/5, /p/6, /p/7"
    );
    let many: Vec<PathBuf> = (0..11).map(path).collect();
    assert_eq!(
        listed(&many),
        "/p/0, /p/1, /p/2, /p/3, /p/4, /p/5, /p/6, /p/7, and 3 more"
    );
}
