//! The edges of the teardown's loops and the quarantine: success on the last allowed
//! attempt, a failure that clears, each launchd domain on its own and both at once, a
//! check that fails once after one succeeded, a job launchd started during the final
//! bootout, a quarantine that outlives a restart of the helper, and a delete repeated
//! after the record went that still finds what the uid left.

use std::collections::VecDeque;
use std::path::PathBuf;

use super::*;

/// A rig of ONE uid, the test's own, holding lease `lease`.
fn one(name: &str, lease: &str) -> (Rig, u32) {
    let rig = rig(name, 1);
    let uid = rig.helper.user_create(lease, None).unwrap();
    log(&rig);
    (rig, uid)
}

/// How many logged calls are exactly `call`.
fn count(calls: &[String], call: &str) -> usize {
    calls.iter().filter(|c| *c == call).count()
}

/// Asserts that no other lease can have the uid: the lease is held.
fn uid_held(rig: &Rig) {
    let range = rig.helper.settings.range;
    assert_eq!(
        rig.helper.user_create("99.1", None),
        Err(format!("no free uid in {range}"))
    );
}

/// Catches a bound off by one: processes that are gone at the last allowed attempt are a
/// success, not a refusal, and the loop takes exactly [`KILL_ROUNDS`] attempts for it.
#[test]
fn kill_uid_succeeds_on_the_last_allowed_round() {
    let (rig, uid) = one("bounds-last-round", "41.1");
    rig.host.state().procs.insert(uid, 1);
    rig.host.state().stubborn.insert(uid, KILL_ROUNDS - 1);
    assert_eq!(rig.helper.kill_uid("41.1"), Ok(()));
    let calls = log(&rig);
    // Every attempt of the first pass, and the one of the second.
    assert_eq!(count(&calls, &format!("kill_all {uid}")), KILL_ROUNDS + 1);
}

/// Catches a bootout failure taken as final, or one that is ignored: a bootout of
/// `user/<uid>` that fails twice (the domain still prints) and then works ends in
/// success, after exactly three attempts of the first pass, with the domain gone.
#[test]
fn a_bootout_failure_that_clears_ends_in_success() {
    let (rig, uid) = one("bounds-transient", "42.1");
    rig.host.state().domains.insert(format!("user/{uid}"));
    rig.host
        .state()
        .fail_script
        .insert("bootout user", VecDeque::from([true, true]));
    assert_eq!(rig.helper.kill_uid("42.1"), Ok(()));
    let calls = log(&rig);
    // Three in the first pass (the domain went at the third), and the second pass's
    // one, of a domain that is gone (it fails, which decides nothing).
    assert_eq!(count(&calls, &format!("bootout user/{uid}")), 4);
    assert_eq!(count(&calls, &format!("kill_all {uid}")), 4);
    assert!(rig.host.state().domains.is_empty());
}

/// The tail of a `kill-uid` refusal: the uid stays quarantined.
fn quarantined(uid: u32) -> String {
    format!("uid {uid} is quarantined: no other lease gets it until a delete succeeds")
}

/// Catches a teardown that looks at one domain only, or names the wrong one: for a uid
/// with a GUI session, the GUI domain alone is booted out; the GUI domain alone staying
/// is named alone; both staying are both named, in order, each with why its bootout
/// failed. A uid with no session is not asked about a `gui/<uid>` at all.
#[test]
fn each_domain_is_handled_and_named_on_its_own() {
    let (rig, uid) = one("bounds-domains", "43.1");
    let gui = format!("gui/{uid}");
    let session = |rig: &Rig| {
        let mut state = rig.host.state();
        state.gui.insert(uid);
        state.domains.insert(gui.clone());
    };
    session(&rig);
    assert_eq!(rig.helper.kill_uid("43.1"), Ok(()));
    assert!(rig.host.state().domains.is_empty());

    session(&rig);
    rig.host.state().fail.insert("bootout gui");
    assert_eq!(
        rig.helper.kill_uid("43.1"),
        Err(format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: \
             gui/{uid} (bootout: bootout gui failed); {}",
            quarantined(uid)
        ))
    );
    uid_held(&rig);

    session(&rig);
    rig.host.state().domains.insert(format!("user/{uid}"));
    rig.host.state().fail.insert("bootout user");
    assert_eq!(
        rig.helper.kill_uid("43.1"),
        Err(format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: \
             gui/{uid} (bootout: bootout gui failed), user/{uid} (bootout: bootout user \
             failed); {}",
            quarantined(uid)
        ))
    );
    uid_held(&rig);

    // No session: `gui/<uid>` is neither booted out nor looked at.
    rig.host.state().fail.clear();
    rig.host.state().domains.remove(&format!("user/{uid}"));
    log(&rig);
    assert_eq!(rig.helper.kill_uid("43.1"), Ok(()));
    assert!(!log(&rig).iter().any(|c| c.contains(&gui)));
}

/// Catches a launchd check whose later failure is taken for absence, or one failure
/// taken as final: for a uid with a GUI session, the first attempt's check of
/// `gui/<uid>` works and that of `user/<uid>` cannot be run; the next attempt looks at
/// both again, and `kill-uid` succeeds there.
#[test]
fn a_check_that_fails_once_after_one_worked_is_looked_at_again() {
    let (rig, uid) = one("bounds-check", "44.1");
    rig.host.state().gui.insert(uid);
    rig.host
        .state()
        .fail_script
        .insert("launchctl print", VecDeque::from([false, true]));
    assert_eq!(rig.helper.kill_uid("44.1"), Ok(()));
    let calls = log(&rig);
    assert_eq!(count(&calls, &format!("launchctl print user/{uid}")), 3);
    assert_eq!(count(&calls, &format!("kill_all {uid}")), 3);
}

/// Catches a last look that lists the processes only before the bootout: a job launchd
/// started just before the final bootout ended its domain keeps running, and the
/// record must not go. Red when the processes are not looked at after the bootout.
#[test]
fn a_job_started_during_the_final_bootout_refuses_user_delete() {
    let (rig, uid) = one("bounds-final-bootout", "45.1");
    // Nothing starts at the first look's bootout; at the last, a job launchd starts
    // just before the domain goes leaves one process.
    rig.host.state().bootout_starts.extend([0, 1]);
    let got = rig.helper.user_delete("45.1");
    assert_eq!(
        got,
        Err(format!(
            "1 processes of uid {uid} remain after the bootout: 1000 (proc0); they started \
             while its launchd domains were booted out, so the delete stops here"
        ))
    );
    let calls = log(&rig);
    assert_eq!(
        count(&calls, &format!("bootout user/{uid}")),
        2,
        "{calls:#?}"
    );
    assert!(!calls.iter().any(|c| c.starts_with("delete_user")));
    assert!(rig.host.state().users.contains_key("kbf-lease-45-1"));
    uid_held(&rig);
}

/// Catches a last look that waits out a job launchd started during the final bootout:
/// a short-lived job may write again what the sweep removed (its entry, a file in the
/// home) before it exits, so seeing it at all stops the delete, with no second listing,
/// and the record stays. The repeated delete, with nothing started, sweeps everything
/// again (the entry the job wrote included) and completes. Red when the look after the
/// bootout waits for the job to exit.
#[test]
fn a_job_seen_after_the_final_bootout_stops_the_delete_even_if_it_exits() {
    let (rig, uid) = one("bounds-short-job", "49.1");
    let entry = rig.dir.join(format!("folders/zz/{uid}"));
    // Looks: the first settle (before and after its bootout), the middle look, the last
    // settle's first look: none; after the last bootout: the job, then nothing.
    rig.host.state().live_script.extend([0, 0, 0, 0, 1, 0]);
    // What the job wrote again before it exited.
    std::fs::create_dir_all(entry.join("C/again")).unwrap();
    let got = rig.helper.user_delete("49.1");
    assert_eq!(
        got,
        Err(format!(
            "1 processes of uid {uid} remain after the bootout: 1000 (proc0); they started \
             while its launchd domains were booted out, so the delete stops here"
        ))
    );
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("delete_user")),
        "{calls:#?}"
    );
    assert!(!calls.iter().any(|c| *c == "pause"), "waited: {calls:#?}");
    uid_held(&rig);
    rig.host.state().live_script.clear();
    std::fs::create_dir_all(entry.join("C/again")).unwrap();
    assert_eq!(rig.helper.user_delete("49.1"), Ok(true));
    assert!(!entry.exists());
}

/// Catches a delete repeated after the record went that frees the uid while what it
/// left sits behind a mount in the per-user folders: an `<xx>` on another filesystem,
/// or an entry of the uid on one, refuses the delete and the uid stays held.
#[test]
#[cfg(not(target_os = "macos"))] // A mount is pretended, which only works off macOS.
fn a_mount_in_the_folders_refuses_a_delete_without_a_record() {
    for (n, mounted) in ["ab", "ab/cdef0123"].into_iter().enumerate() {
        let lease = format!("50.{}", n + 1);
        let (rig, uid) = one(&format!("bounds-mounted-{n}"), &lease);
        let folders = rig.dir.join("folders");
        std::fs::create_dir_all(folders.join("ab/cdef0123")).unwrap();
        crate::sweep::pretend_mounted(&[folders.join(mounted)]);
        rig.host.state().users.clear();
        let got = rig.helper.user_delete(&lease);
        crate::sweep::pretend_mounted(&[]);
        assert_eq!(
            got,
            Err(format!(
                "searching {} for the entries of uid {uid}: {}: another filesystem is \
                 mounted here; what uid {uid} left in it cannot be removed",
                folders.display(),
                folders.join(mounted).display()
            ))
        );
        uid_held(&rig);
        assert!(folders.join("ab/cdef0123").exists());
    }
}

/// The helper over the same directory again, as after a restart, before its boot
/// reset; what the reopened ledger says of `uid`.
fn restarted(rig: Rig, name: &str, uid: u32) -> Rig {
    let range = rig.helper.settings.range;
    let dir = rig.dir.clone();
    drop(rig);
    let again = rig_with(name, range, Some(dir));
    assert!(
        again.helper.lock().held_uids().contains(&uid),
        "uid {uid} is free after the restart"
    );
    again
}

/// Catches a quarantine kept only in memory: after a failed `user-delete` (a domain that
/// stays; and a deletion whose record went but whose ledger write failed) the reopened
/// ledger still holds the uid, a boot reset that cannot end the user fails and keeps
/// holding it, and the reset repeated once nothing stops it completes the delete and
/// frees the uid.
#[test]
fn a_held_uid_stays_held_across_a_restart_until_a_delete_completes() {
    let (rig, uid) = one("bounds-restart", "46.1");
    rig.host.state().domains.insert(format!("user/{uid}"));
    rig.host.state().fail.insert("bootout user");
    assert!(rig.helper.user_delete("46.1").is_err());
    let rig = restarted(rig, "bounds-restart", uid);
    rig.host.state().domains.insert(format!("user/{uid}"));
    rig.host.state().fail.insert("bootout user");
    let got = rig.helper.boot_reset(&[]).unwrap_err();
    assert!(
        got.starts_with("the boot reset is incomplete: lease 46.1: "),
        "{got}"
    );
    assert!(rig.helper.lock().held_uids().contains(&uid));
    rig.host.state().fail.clear();
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted.len(), 1);
    assert_eq!(rig.helper.user_create("46.2", None), Ok(uid));

    // The record goes, and the ledger cannot say so.
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"));
    let got = rig.helper.user_delete("46.2").unwrap_err();
    assert!(
        got.starts_with("recording the deletion of lease 46.2"),
        "{got}"
    );
    assert!(!rig.host.state().users.contains_key("kbf-lease-46-2"));
    uid_held(&rig);
    let rig = restarted(rig, "bounds-restart", uid);
    rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(rig.helper.user_create("46.3", None), Ok(uid));
}

/// Catches a delete repeated after the record went that frees the uid while the uid's
/// per-user folders entry is still there (an earlier teardown that did not finish, or
/// a record removed by someone else): with no `DARWIN_USER_DIR` to ask for, every entry
/// the uid owns there goes, and nothing else does.
#[test]
fn a_record_gone_with_its_entry_left_still_has_the_entry_removed() {
    let (rig, _uid) = one("bounds-residual", "47.1");
    let folders = rig.dir.join("folders");
    let mine = folders.join("ab/cdef0123");
    std::fs::create_dir_all(mine.join("C/com.apple.ScreenTimeAgent")).unwrap();
    std::fs::write(mine.join("C/com.apple.ScreenTimeAgent/db"), "x").unwrap();
    let file = folders.join("ab/not-an-entry");
    std::fs::write(&file, "x").unwrap();
    rig.host.state().users.clear();
    assert_eq!(rig.helper.user_delete("47.1"), Ok(false));
    assert!(!mine.exists());
    assert!(file.exists());
    let calls = log(&rig);
    assert!(
        !calls.iter().any(|c| c.starts_with("user_dir")),
        "{calls:#?}"
    );

    // When the folders cannot be searched, the delete is refused and the uid held.
    let (rig, uid) = one("bounds-residual-stuck", "48.1");
    let folders: PathBuf = rig.dir.join("folders");
    std::fs::write(&folders, "not a directory").unwrap();
    rig.host.state().users.clear();
    let got = rig.helper.user_delete("48.1").unwrap_err();
    assert!(
        got.starts_with(&format!(
            "searching {} for the entries of uid {uid}: ",
            folders.display()
        )),
        "{got}"
    );
    uid_held(&rig);
}
