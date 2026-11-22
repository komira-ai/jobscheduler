//! Each step of the deletion of an account that fails: the delete is refused naming the step,
//! the account's record (where it is still there) and the lease stay, `deleted` is not
//! written, and the uid is held (no other lease gets it). The steps are, in order, the look
//! for the uid's `DARWIN_USER_DIR`, the first settling (no process, no domain), the
//! sweep of the schedules, the wait for the processes, the IPC objects, the sweep of the home
//! and the shared places, the removal of the per-user folders entry, the second settling
//! with the GUI domain's discharge, and the record's deletion.

use super::*;

/// What the refusal must be.
enum Says {
    /// This text, exactly.
    Exactly(String),
    /// A text that starts like this (the rest is the system's own wording).
    StartsWith(String),
}

/// Deletes `lease` of a rig of one uid after `setup`, and asserts the refusal is `says`, the
/// lease is live with no `deleted` written, and its uid is held. Returns the rig.
#[track_caller]
fn refused(
    name: &str,
    lease: &str,
    setup: impl FnOnce(&Rig, u32, &str),
    says: impl FnOnce(u32, &str) -> Says,
) -> (Rig, u32) {
    let (rig, uid) = one_uid(name, lease);
    let account = crate::lease::user_name(parse_lease(lease).unwrap());
    setup(&rig, uid, &account);
    let error = rig.helper.user_delete(lease).unwrap_err();
    match says(uid, &account) {
        Says::Exactly(want) => assert_eq!(error, want),
        Says::StartsWith(want) => assert!(error.starts_with(&want), "{error}"),
    }
    let ledger = std::fs::read_to_string(rig.dir.join("ledger")).unwrap();
    assert!(!ledger.contains("deleted"), "{ledger}");
    assert!(rig.helper.lock().held_uids().contains(&uid));
    uid_held(&rig);
    (rig, uid)
}

/// Catches: a step that fails and is passed over (the account would be left, its uid freed),
/// or fails without naming itself.
#[test]
fn a_step_that_fails_before_anything_is_removed_refuses_naming_it() {
    let exactly = |text: String| Says::Exactly(text);
    let (rig, _) = refused(
        "steps-lookup",
        "20.1",
        |rig, _, _| {
            rig.host.state().fail.insert("account");
        },
        |_, account| exactly(format!("looking up user {account}: account failed")),
    );
    assert!(rig.host.state().users.contains_key("kbf-lease-20-1"));

    refused(
        "steps-user-dir",
        "20.2",
        |rig, _, _| {
            rig.host.state().fail.insert("user_dir");
        },
        |uid, _| {
            exactly(format!(
                "asking for the DARWIN_USER_DIR of uid {uid}: user_dir failed"
            ))
        },
    );
    refused(
        "steps-listing",
        "20.3",
        |rig, _, _| {
            rig.host.state().fail.insert("live_processes");
        },
        |uid, _| {
            exactly(format!(
                "listing the processes of uid {uid}: live_processes failed"
            ))
        },
    );
    refused(
        "steps-process",
        "20.4",
        |rig, uid, _| {
            rig.host.state().procs.insert(uid, 1);
        },
        |uid, _| {
            exactly(format!(
                "1 processes of uid {uid} remain after {EXIT_LOOKS} looks: 1000 (proc0); kill-uid first"
            ))
        },
    );
    refused(
        "steps-domain",
        "20.5",
        |rig, uid, _| {
            let mut state = rig.host.state();
            state.domains.insert(format!("user/{uid}"));
            state.fail.insert("bootout user");
        },
        |uid, _| {
            exactly(format!(
                "launchd domains of uid {uid} remain: user/{uid} (bootout: bootout user failed); kill-uid first"
            ))
        },
    );
    // A process that starts while the domain is booted out stops the delete at once.
    refused(
        "steps-started",
        "20.6",
        |rig, _, _| {
            rig.host.state().bootout_starts.push_back(2);
            rig.host.state().domains.insert(format!("user/{}", me()));
        },
        |uid, _| {
            Says::StartsWith(format!(
                "2 processes of uid {uid} remain after the bootout: 1000 (proc0), 1001 (proc1); they started while its launchd domains were booted out"
            ))
        },
    );
}

/// Catches: a failing sweep (of the crontab and `at` jobs, of the home and the shared
/// places) or a refused removal of the per-user folders entry passed over: each names the
/// place, and the account's record stays where it was not yet deleted.
#[test]
fn a_sweep_or_a_removal_that_fails_refuses_naming_the_place() {
    if me() == 0 {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let starts = |text: String| Says::StartsWith(text);
    let close = |path: &Path| {
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o500))
            .unwrap();
    };
    let open = |path: &Path| {
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
    };
    // The crontab named for the account is a folder that cannot be emptied.
    let (rig, _) = refused(
        "steps-schedules",
        "21.1",
        |rig, _, account| {
            let tab = rig.dir.join("tabs").join(account);
            std::fs::create_dir_all(&tab).unwrap();
            std::fs::write(tab.join("job"), "x").unwrap();
            close(&tab);
        },
        |_, account| starts(format!("the sweep for {account} is incomplete: ")),
    );
    open(&rig.dir.join("tabs/kbf-lease-21-1"));
    assert!(rig.host.state().users.contains_key("kbf-lease-21-1"));

    // The home holds a folder that cannot be emptied.
    let (rig, _) = refused(
        "steps-home",
        "21.2",
        |rig, _, account| {
            let locked = rig.dir.join("Users").join(account).join("locked");
            std::fs::create_dir_all(&locked).unwrap();
            std::fs::write(locked.join("f"), "x").unwrap();
            close(&locked);
        },
        |_, account| starts(format!("the sweep for {account} is incomplete: ")),
    );
    open(&rig.dir.join("Users/kbf-lease-21-2/locked"));
    assert!(rig.host.state().users.contains_key("kbf-lease-21-2"));

    // The uid's entry of the per-user folders cannot be emptied.
    let (rig, _) = refused(
        "steps-entry",
        "21.3",
        |rig, uid, _| {
            let entry = rig.dir.join("folders/ab/cdef");
            std::fs::create_dir_all(entry.join("T/locked")).unwrap();
            std::fs::write(entry.join("T/locked/f"), "x").unwrap();
            close(&entry.join("T/locked"));
            rig.host
                .state()
                .user_dirs
                .insert(uid, PathBuf::from("/var/folders/ab/cdef/0/"));
        },
        |_, account| starts(format!("the sweep for {account} is incomplete: ")),
    );
    open(&rig.dir.join("folders/ab/cdef/T/locked"));
    assert!(rig.host.state().users.contains_key("kbf-lease-21-3"));
}

/// Catches: a record that cannot be deleted taken as deleted (the account would be left, its
/// uid freed), and a GUI teardown record that cannot be discharged passed over (the next
/// teardown would judge a domain that is not owed).
#[test]
fn a_record_that_cannot_be_deleted_or_a_discharge_that_fails_refuses_naming_the_step() {
    let (rig, _) = refused(
        "steps-delete",
        "22.1",
        |rig, _, _| {
            rig.host.state().fail.insert("delete_user");
        },
        |_, account| Says::Exactly(format!("deleting user {account}: delete_user failed")),
    );
    assert!(rig.host.state().users.contains_key("kbf-lease-22-1"));

    // The record that the GUI domain is owed a teardown is a folder with something in it:
    // it cannot be removed once the domain is judged gone.
    let (rig, uid) = one_uid("steps-discharge", "22.2");
    let record = rig.dir.join(format!("gui-teardown.{uid}"));
    std::fs::create_dir_all(record.join("inside")).unwrap();
    let error = rig.helper.user_delete("22.2").unwrap_err();
    assert!(error.contains("the GUI teardown record"), "{error}");
    assert!(rig.helper.lock().held_uids().contains(&uid));
    uid_held(&rig);
    assert!(rig.host.state().users.contains_key("kbf-lease-22-2"));
}
