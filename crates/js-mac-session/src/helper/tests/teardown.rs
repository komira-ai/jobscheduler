//! The lease user's teardown against a fake launchd: `kill-uid` kills, then boots the
//! uid's domains out, until neither a process nor a domain is left (a domain that still
//! prints, or that `launchctl print` gives no answer for, is not gone); `user-delete`
//! removes the uid's own entry under the per-user folders, named by its
//! `DARWIN_USER_DIR`; and a step that fails leaves the lease live, so its uid is held and
//! handed to no one else. The outcome rule itself is in `outcome.rs`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::*;
use crate::testing::{found, no_domain};

/// The tail of a `kill-uid` refusal: the uid stays quarantined.
fn quarantined(uid: u32) -> String {
    format!("uid {uid} is quarantined: no other lease gets it until a delete succeeds")
}

/// A rig of ONE uid holding `lease` from before the pool ([`legacy`]): a later lease gets
/// the uid only once its user is deleted and the pool's slot made.
fn one(name: &str, lease: &str) -> (Rig, u32) {
    legacy(name, lease)
}

/// Asserts what a failed step must leave: the user's record, the lease live (its uid
/// held: no other lease gets it), and the refusal `want` as the helper said it.
fn held(rig: &Rig, user: &str, got: Result<impl std::fmt::Debug, String>, want: &str) {
    assert_eq!(got.unwrap_err(), want);
    assert!(
        rig.host.state().users.contains_key(user),
        "{user} was deleted"
    );
    exhausted(rig.helper.user_create("99.1", None));
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
/// attempts that leave something must say so by a look. Red with a single attempt, or
/// with the bootout before the kill.
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
    let bootout = format!("bootout user/{uid}");
    let look = format!("live_processes {uid}");
    // The user never logged in: `gui/<uid>` is never asked about.
    let print = format!("launchctl print user/{uid}");
    // The look for a GUI session; then the job survives the first kill: the domain is
    // made again, and the agent is back.
    let mut want = vec![look.clone(), kill.clone(), bootout.clone(), look.clone()];
    want.extend([print.clone(), "pause".to_owned()]);
    // Nothing is left.
    want.extend([kill.clone(), bootout.clone(), look.clone(), print.clone()]);
    // After the crontab and `at` jobs are swept, one attempt more (the bootout of a
    // domain that is gone fails, which decides nothing).
    want.extend([kill, bootout, look, print]);
    assert_eq!(calls, want);
}

/// Catches a `kill-uid` that reports success while launchd keeps a domain of the uid (a
/// bootout that fails, or one that appears to work while the domain stays, or a print
/// that cannot be run), that waits on it without a bound, or that cannot tell the cases
/// apart in what it says; and a failure that frees the uid. Each refusal leaves the
/// lease live.
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
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: \
             user/{uid} (bootout: bootout user failed); {}",
            quarantined(uid)
        ),
    );
    let bootouts = log(&rig)
        .iter()
        .filter(|c| **c == format!("bootout user/{uid}"))
        .count();
    assert_eq!(bootouts, KILL_ROUNDS);

    // A bootout that says it worked while a process of the uid keeps the domain (no
    // look happens to see the process, the look for a GUI session included).
    rig.host.state().fail.clear();
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.stubborn.insert(uid, usize::MAX);
        state.live_script.extend([0; KILL_ROUNDS + 1]);
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
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: user/{uid}; {}",
            quarantined(uid)
        ),
    );

    // `launchctl` cannot be run: no look says the domain is gone.
    rig.host.state().fail.insert("launchctl print");
    let got = rig.helper.kill_uid("23.1");
    held(
        &rig,
        user,
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: \
             user/{uid} (launchctl print: launchctl print failed); {}",
            quarantined(uid)
        ),
    );
}

/// Catches a `user-delete` that deletes the record while launchd keeps a domain of the
/// uid (the record goes, the agents stay), one that frees the uid when it refuses, and
/// one that asks about `gui/<uid>` of a uid with no process left (so no login session).
#[test]
fn a_domain_that_stays_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-delete-domain", "24.1");
    rig.host.state().domains.insert(format!("user/{uid}"));
    rig.host.state().fail.insert("bootout user");
    let got = rig.helper.user_delete("24.1");
    held(
        &rig,
        "kbf-lease-24-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain: user/{uid} (bootout: bootout user failed); \
             kill-uid first"
        ),
    );
    let calls = log(&rig);
    assert!(!calls.iter().any(|c| c.starts_with("delete_user")));
    assert!(!calls.iter().any(|c| c.contains("gui/")), "{calls:#?}");
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
/// refuses while it still prints. Red when the last look ignores the domain.
#[test]
fn a_domain_found_at_the_last_look_refuses_user_delete_and_holds_the_uid() {
    let (rig, uid) = one("teardown-last-look", "28.1");
    // `user/<uid>` does not print at the first look's check; it does at the last's,
    // though its bootout found no such domain.
    let user = format!("user/{uid}");
    rig.host
        .state()
        .print_script
        .extend([no_domain(&user), found()]);
    let got = rig.helper.user_delete("28.1");
    held(
        &rig,
        "kbf-lease-28-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain: {user} (bootout: no such domain {user}); \
             kill-uid first"
        ),
    );
    assert!(!log(&rig).iter().any(|c| c.starts_with("delete_user")));
}

/// Catches a `kill-uid` whose second pass, after the crontab and `at` jobs are swept,
/// reports success while a domain of the uid still prints, and a refusal there that
/// frees the uid. Red when the second pass's result is dropped.
#[test]
fn a_domain_that_stays_after_the_schedules_refuses_kill_uid_and_holds_the_uid() {
    let (rig, uid) = one("teardown-second-kill", "29.1");
    let user = format!("user/{uid}");
    // The first pass's attempt: `user/<uid>` does not print; every attempt of the
    // second: it does.
    let mut script = vec![no_domain(&user)];
    script.extend(std::iter::repeat_n(found(), KILL_ROUNDS));
    rig.host.state().print_script.extend(script);
    let got = rig.helper.kill_uid("29.1");
    held(
        &rig,
        "kbf-lease-29-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: \
             {user} (bootout: no such domain {user}); {}",
            quarantined(uid)
        ),
    );
    assert!(rig.host.state().print_script.is_empty());
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
    assert!(rig.helper.pool_create().errors.is_empty());
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

/// Catches the refusal that left every Mac job's user behind: a pool user whose agents
/// are gone has no launchd domain at all, so each bootout of `user/<uid>` fails ("no such
/// domain"). The failure decides nothing: `kill-uid` succeeds after one attempt in each
/// pass.
#[test]
fn kill_uid_of_a_uid_with_no_domain_succeeds_though_each_bootout_fails() {
    let (rig, uid) = one("teardown-no-domain", "32.1");
    rig.host.state().procs.insert(uid, 1);
    assert_eq!(rig.helper.kill_uid("32.1"), Ok(()));
    let attempt = [
        format!("kill_all {uid}"),
        format!("bootout user/{uid}"),
        format!("live_processes {uid}"),
        format!("launchctl print user/{uid}"),
    ];
    let mut want = vec![format!("live_processes {uid}")];
    want.extend(attempt.clone());
    want.extend(attempt);
    assert_eq!(log(&rig), want);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
}

/// `launchctl print`'s answer when a signal ended it.
fn killed() -> PrintAnswer {
    PrintAnswer {
        code: None,
        stderr: "killed\n".to_owned(),
    }
}

/// Catches a `launchctl print` that gave no answer (a signal ended it, or it could not
/// be run) read as a domain that is gone, which would let a teardown free a uid launchd
/// still serves, and a teardown that fails at the first such look instead of looking
/// again. `kill-uid` looks again at its next attempt and completes when that look
/// answers; it fails naming the domain and why when no look answers. `user-delete`, at
/// its first and its last look, refuses naming the domain. The uid stays held.
#[test]
fn a_print_that_gives_no_answer_is_never_read_as_gone() {
    let (rig, uid) = one("teardown-no-answer", "39.1");
    let user = format!("user/{uid}");
    // Once: the next attempt's look answers.
    rig.host.state().print_script.push_back(killed());
    assert_eq!(rig.helper.kill_uid("39.1"), Ok(()));
    let kills = log(&rig)
        .iter()
        .filter(|c| c.starts_with("kill_all"))
        .count();
    assert_eq!(
        kills, 3,
        "two attempts in the first pass, one in the second"
    );
    // Every look of the first pass.
    rig.host
        .state()
        .print_script
        .extend(std::iter::repeat_n(killed(), KILL_ROUNDS));
    let got = rig.helper.kill_uid("39.1");
    held(
        &rig,
        "kbf-lease-39-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: {user} \
             (launchctl print: launchctl print was ended by a signal: killed); {}",
            quarantined(uid)
        ),
    );
    // `user-delete`'s first look, then its last.
    for before in [0, 1] {
        let mut script = vec![no_domain(&user); before];
        script.push(killed());
        rig.host.state().print_script.extend(script);
        let got = rig.helper.user_delete("39.1");
        let want = format!(
            "launchd domains of uid {uid} remain: {user} (launchctl print: launchctl print \
             was ended by a signal: killed); kill-uid first"
        );
        held(&rig, "kbf-lease-39-1", got, &want);
        assert!(rig.host.state().print_script.is_empty());
        assert!(!log(&rig).iter().any(|c| c.starts_with("delete_user")));
    }
}

/// Catches a teardown of a uid with a GUI session that, once `gui/<uid>` is gone, reads
/// a failed look at `user/<uid>` as absence: the GUI domain is booted out and does not
/// print, and `launchctl print user/<uid>` cannot be run at any attempt. `kill-uid`
/// fails naming `user/<uid>` alone, the record stays and the uid is held.
#[test]
fn a_print_that_fails_after_the_gui_domain_went_refuses_the_teardown() {
    let (rig, uid) = one("teardown-partial", "40.1");
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    {
        let mut state = rig.host.state();
        state.gui.insert(uid);
        state.domains.insert(gui.clone());
        // Each attempt: `gui/<uid>` answers, `user/<uid>` cannot be looked at.
        let script = [false, true].repeat(KILL_ROUNDS);
        state
            .fail_script
            .insert("launchctl print", VecDeque::from(script));
    }
    let got = rig.helper.kill_uid("40.1");
    held(
        &rig,
        "kbf-lease-40-1",
        got,
        &format!(
            "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: {user} \
             (launchctl print: launchctl print failed); {}",
            quarantined(uid)
        ),
    );
    let calls = log(&rig);
    assert_eq!(
        calls[..5],
        [
            format!("live_processes {uid}"),
            format!("kill_all {uid}"),
            format!("bootout {gui}"),
            format!("bootout {user}"),
            format!("live_processes {uid}"),
        ]
    );
    assert!(rig.host.state().domains.is_empty());
}
