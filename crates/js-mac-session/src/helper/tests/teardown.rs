//! The lease user's teardown against a fake launchd: `kill-uid` kills, then boots the
//! uid's domains out, until neither a process nor a domain is left; `user-delete` removes
//! the uid's own entry under the per-user folders, named by its `DARWIN_USER_DIR`; and a
//! step that fails leaves the lease live, so its uid is held and handed to no one else.

use std::path::{Path, PathBuf};

use super::*;

/// A rig of ONE uid, the test's own, holding lease `lease`: a later lease gets a uid
/// only if this one was freed.
fn one(name: &str, lease: &str) -> (Rig, u32) {
    let rig = rig(name, 1);
    let uid = rig.helper.user_create(lease, None).unwrap();
    log(&rig);
    (rig, uid)
}

/// Asserts what a failed step must leave: the user's record, the lease live (its uid
/// held: no other lease gets it), and the refusal `want` as the helper said it.
fn held(rig: &Rig, user: &str, got: Result<impl std::fmt::Debug, String>, want: &str) {
    assert_eq!(got.unwrap_err(), want);
    assert!(
        rig.host.state().users.contains_key(user),
        "{user} was deleted"
    );
    let range = rig.helper.settings.range;
    assert_eq!(
        rig.helper.user_create("99.1", None),
        Err(format!("no free uid in {range}")),
        "the uid of {user} was handed out again"
    );
}

/// What launchd has after the teardown: the domains, and the uid's processes at two
/// looks more (a restart launchd had due shows at the first).
fn after(rig: &Rig, uid: u32) -> (Vec<String>, usize, usize) {
    let domains = rig.host.state().domains.iter().cloned().collect();
    let first = rig.host.live_processes(uid).unwrap().len();
    let second = rig.host.live_processes(uid).unwrap().len();
    (domains, first, second)
}

/// Catches the leak the two-run isolation test found on a Mac: a per-user agent
/// (`distnoted`) that launchd restarts after `kill-uid` and that outlives the user. The
/// job runs as the uid, so booting `user/<uid>` out before the kill does nothing (the
/// live job makes the domain again), and launchd restarts its agents from that domain
/// for as long as it exists. Red when the domain is not booted out after the kill: the
/// domain survives `kill-uid`.
#[test]
fn kill_uid_boots_the_user_domain_out_after_the_kill() {
    let (rig, uid) = one("teardown-agent", "21.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
        state.respawns.insert(uid, 1);
    }
    assert_eq!(rig.helper.kill_uid("21.1"), Ok(()));
    let calls = log(&rig);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
    let first_kill = calls
        .iter()
        .position(|c| c.starts_with("kill_all"))
        .unwrap();
    let first_bootout = calls
        .iter()
        .position(|c| *c == format!("bootout user/{uid}"))
        .unwrap();
    assert!(first_kill < first_bootout, "{calls:#?}");

    // An agent left running with no domain to restart it from is killed, and stays so.
    rig.host.state().agents.insert(uid);
    assert_eq!(rig.helper.kill_uid("21.1"), Ok(()));
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
}

/// Catches a teardown that stops after one kill and one bootout while a process of the
/// uid still runs: here a job survives the first kill, so the domain is made again and
/// launchd restarts the agent the kill ended (the fake launchd restarts it once). The
/// kill and the bootout must repeat until no process and no domain is left, and the
/// rounds that leave something must say so by a look. Red with a single round, or with
/// the bootout before the kill.
#[test]
fn kill_uid_repeats_kill_and_bootout_until_nothing_is_left() {
    let (rig, uid) = one("teardown-repeat", "22.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.stubborn.insert(uid, 1);
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
        state.respawns.insert(uid, 1);
    }
    assert_eq!(rig.helper.kill_uid("22.1"), Ok(()));
    let calls = log(&rig);
    // launchd restarted the agent the first kill ended: the restart was used.
    assert_eq!(rig.host.state().respawns[&uid], 0);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
    let kill = format!("kill_all {uid}");
    let (gui, user) = (format!("bootout gui/{uid}"), format!("bootout user/{uid}"));
    let look = format!("live_processes {uid}");
    let exists = [
        format!("domain_exists gui/{uid}"),
        format!("domain_exists user/{uid}"),
    ];
    let mut want = vec![
        // The job survived: the domain is made again, and the agent is back.
        kill.clone(),
        gui.clone(),
        user.clone(),
        look.clone(),
        "pause".to_owned(),
        // Nothing is left.
        kill.clone(),
        gui.clone(),
        user.clone(),
        look.clone(),
    ];
    want.extend(exists.clone());
    // After the crontab and `at` jobs are swept, one round more.
    want.extend([kill, gui, user, look]);
    want.extend(exists);
    assert_eq!(calls, want);
}

/// Catches a `kill-uid` that reports success while launchd keeps a domain of the uid (a
/// bootout that fails, or one that appears to work while the domain stays), that waits
/// on it without a bound, or that cannot tell the two apart in what it says; and a
/// failure that frees the uid. Each refusal leaves the lease live.
#[test]
fn a_domain_that_stays_refuses_kill_uid_and_holds_the_uid() {
    let (rig, uid) = one("teardown-domain", "23.1");
    let user = "kbf-lease-23-1";
    rig.host.state().domains.insert(format!("user/{uid}"));
    // Its agent runs, and launchd restarts it no more.
    rig.host.state().agents.insert(uid);
    rig.host.state().fail.insert("bootout user");
    let got = rig.helper.kill_uid("23.1");
    held(
        &rig,
        user,
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} rounds: \
             user/{uid} (bootout: bootout user failed)"
        ),
    );
    let bootouts = log(&rig)
        .iter()
        .filter(|c| **c == format!("bootout user/{uid}"))
        .count();
    assert_eq!(bootouts, KILL_ROUNDS);

    // A bootout that says it worked while a process of the uid keeps the domain (no
    // look happens to see the process).
    rig.host.state().fail.clear();
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.stubborn.insert(uid, usize::MAX);
        state.live_script.extend([0; KILL_ROUNDS]);
    }
    let got = rig.helper.kill_uid("23.1");
    {
        let mut state = rig.host.state();
        state.procs.clear();
        state.stubborn.clear();
    }
    held(
        &rig,
        user,
        got,
        &format!("launchd domains of uid {uid} remain after {KILL_ROUNDS} rounds: user/{uid}"),
    );

    rig.host.state().fail.insert("domain_exists");
    let got = rig.helper.kill_uid("23.1");
    held(
        &rig,
        user,
        got,
        &format!("asking launchd for gui/{uid}: domain_exists failed"),
    );
}

/// Catches a `user-delete` that deletes the record while launchd keeps a domain of the
/// uid (the record goes, the agents stay), and one that frees the uid when it refuses.
#[test]
fn a_domain_that_stays_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-delete-domain", "24.1");
    rig.host.state().domains.insert(format!("gui/{uid}"));
    rig.host.state().fail.insert("bootout gui");
    let got = rig.helper.user_delete("24.1");
    held(
        &rig,
        "kbf-lease-24-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain: gui/{uid} (bootout: bootout gui failed); \
             kill-uid first"
        ),
    );
    assert!(!log(&rig).iter().any(|c| c.starts_with("delete_user")));
}

/// The entry `folders/<xx>/<hash>` as macOS makes it for a user, with a folder a daemon
/// made for the user under `C` (the one the Mac's run found left).
fn entry(folders: &Path, xx: &str, hash: &str) -> PathBuf {
    let entry = folders.join(xx).join(hash);
    for dir in ["0", "C/com.apple.ScreenTimeAgent", "T"] {
        std::fs::create_dir_all(entry.join(dir)).unwrap();
    }
    std::fs::write(entry.join("C/com.apple.ScreenTimeAgent/db"), "x").unwrap();
    std::fs::write(entry.join("T/tmp"), "x").unwrap();
    entry
}

/// Catches a `user-delete` that leaves the uid's per-user folders entry (the Mac's run
/// found a `com.apple.ScreenTimeAgent` folder under `C` that the sweep left), one that
/// removes only the `0` its `DARWIN_USER_DIR` names, and one that removes another user's
/// entry beside it. Red when the entry is not removed.
#[test]
fn user_delete_removes_the_entry_its_darwin_user_dir_names() {
    let (rig, uid) = one("teardown-folders", "25.1");
    let folders = rig.dir.join("folders");
    let mine = entry(&folders, "ab", "cdef0123");
    let theirs = entry(&folders, "ab", "other");
    rig.host
        .state()
        .user_dirs
        .insert(uid, PathBuf::from("/var/folders/ab/cdef0123/0/"));
    assert_eq!(rig.helper.user_delete("25.1"), Ok(true));
    assert!(!mine.exists(), "the uid's entry is left");
    assert!(theirs.join("C/com.apple.ScreenTimeAgent/db").exists());
    assert!(folders.join("ab").exists());
    // Asked once, before anything was removed, with the user's ids.
    let calls = log(&rig);
    assert_eq!(calls[1], format!("user_dir {uid}:{}", my_gid()));
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("user_dir")).count(),
        1
    );
}

/// Catches a `user-delete` that goes on without the uid's `DARWIN_USER_DIR` (a guessed
/// path, or the entry left), that removes a path its answer names outside the per-user
/// folders, or anything at all before refusing; and a refusal that frees the uid.
#[test]
fn a_darwin_user_dir_that_cannot_be_had_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-user-dir", "26.1");
    let user = "kbf-lease-26-1";
    let home = rig.dir.join("Users").join(user);
    std::fs::create_dir_all(&home).unwrap();
    rig.host.state().fail.insert("user_dir");
    let got = rig.helper.user_delete("26.1");
    held(
        &rig,
        user,
        got,
        &format!("asking for the DARWIN_USER_DIR of uid {uid}: user_dir failed"),
    );
    rig.host.state().fail.clear();

    // An answer outside the per-user folders: the home it names is not touched.
    let outside = home.join("0");
    std::fs::create_dir_all(&outside).unwrap();
    rig.host.state().user_dirs.insert(uid, outside.clone());
    let got = rig.helper.user_delete("26.1");
    held(
        &rig,
        user,
        got,
        &format!(
            "the DARWIN_USER_DIR {} is not an entry of /var/folders; nothing is removed for it",
            outside.display()
        ),
    );
    assert!(outside.exists());
    let calls = log(&rig);
    assert!(
        calls.iter().all(|c| !c.starts_with("live_processes")),
        "refused before the first look: {calls:#?}"
    );
}

/// Catches a `user-delete` that deletes the record when the entry its `DARWIN_USER_DIR`
/// names cannot be removed (here a link where the entry should be: never followed,
/// never removed), and a refusal that frees the uid.
#[test]
fn an_entry_that_cannot_be_removed_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-entry", "27.1");
    let folders = rig.dir.join("folders");
    let elsewhere = entry(&rig.dir, "el", "sewhere");
    std::fs::create_dir_all(folders.join("ab")).unwrap();
    let link = folders.join("ab/cdef0123");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    rig.host
        .state()
        .user_dirs
        .insert(uid, PathBuf::from("/var/folders/ab/cdef0123/0"));
    let got = rig.helper.user_delete("27.1");
    held(
        &rig,
        "kbf-lease-27-1",
        got,
        &format!(
            "the sweep for kbf-lease-27-1 is incomplete: {}: not a directory on its parent's \
             filesystem; not removed",
            link.display()
        ),
    );
    assert!(elsewhere.join("T/tmp").exists());
    assert!(link.symlink_metadata().is_ok());
}

/// Catches a `user-delete` that deletes the record when launchd made a domain of the uid
/// again during the sweep: the last look, just before the record goes, boots it out and
/// refuses while it stays. Red when the last look ignores the domains.
#[test]
fn a_domain_found_at_the_last_look_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-last-look", "28.1");
    // Neither domain at the first look; `gui/<uid>` at the last.
    rig.host
        .state()
        .exists_script
        .extend([false, false, true, false]);
    let got = rig.helper.user_delete("28.1");
    held(
        &rig,
        "kbf-lease-28-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain: gui/{uid} (bootout: no such domain gui/{uid}); \
             kill-uid first"
        ),
    );
    assert!(!log(&rig).iter().any(|c| c.starts_with("delete_user")));
}

/// Catches a `kill-uid` whose second kill, after the crontab and `at` jobs are swept,
/// reports success while a domain of the uid stays, and a refusal there that frees the
/// uid. Red when the second kill's result is dropped.
#[test]
fn a_domain_that_stays_after_the_schedules_refuses_kill_uid_and_holds_the_uid() {
    let (rig, uid) = one("teardown-second-kill", "29.1");
    let mut script = vec![false, false];
    for _ in 0..KILL_ROUNDS {
        script.extend([false, true]);
    }
    rig.host.state().exists_script.extend(script);
    let got = rig.helper.kill_uid("29.1");
    held(
        &rig,
        "kbf-lease-29-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} rounds: \
             user/{uid} (bootout: no such domain user/{uid})"
        ),
    );
    assert!(rig.host.state().exists_script.is_empty());
}

/// Catches a repeated `user-delete` that asks for the `DARWIN_USER_DIR` of a uid whose
/// record a failed earlier delete already removed (macOS has none of its own for it, so
/// the delete could never finish), and one that cannot look the record up and goes on.
#[test]
fn a_delete_repeated_after_the_record_went_completes_without_the_darwin_user_dir() {
    let (rig, uid) = one("teardown-repeat-delete", "30.1");
    rig.host.state().fail.insert("user_dir");
    rig.host.state().users.clear();
    assert_eq!(rig.helper.user_delete("30.1"), Ok(false));
    assert!(!log(&rig).iter().any(|c| c.starts_with("user_dir")));
    assert!(
        rig.helper.user_create("30.2", None).is_ok(),
        "uid {uid} is free"
    );

    let (rig, uid) = one("teardown-account", "31.1");
    rig.host.state().fail.insert("account");
    let got = rig.helper.user_delete("31.1");
    held(
        &rig,
        "kbf-lease-31-1",
        got,
        "looking up user kbf-lease-31-1: account failed",
    );
    assert!(rig.host.state().users.values().any(|u| u.uid == uid));
}
