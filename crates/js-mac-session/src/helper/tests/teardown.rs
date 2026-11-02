//! The lease user's teardown against a fake launchd: `kill-uid` kills, then boots the
//! uid's domains out, until neither a process nor a domain is left; `user-delete` removes
//! the uid's own entry under the per-user folders, named by its `DARWIN_USER_DIR`; and a
//! step that fails leaves the lease live, so its uid is held and handed to no one else.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::*;
use crate::testing::{found, no_domain};

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
    let bootout = format!("bootout user/{uid}");
    let look = format!("live_processes {uid}");
    // The user never logged in: launchd has no `gui/<uid>`, which is never booted out.
    let print = [
        format!("launchctl print gui/{uid}"),
        format!("launchctl print user/{uid}"),
    ];
    let mut want = vec![kill.clone()];
    // The job survived: the domain is made again, and the agent is back.
    want.extend(print.clone());
    want.extend([
        bootout.clone(),
        look.clone(),
        "pause".to_owned(),
        kill.clone(),
    ]);
    // Nothing is left.
    want.extend(print.clone());
    want.extend([bootout, look.clone()]);
    want.extend(print.clone());
    // After the crontab and `at` jobs are swept, one round more, with no domain left.
    want.push(kill);
    want.extend(print.clone());
    want.push(look);
    want.extend(print);
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

    rig.host.state().fail.insert("launchctl print");
    let got = rig.helper.kill_uid("23.1");
    held(
        &rig,
        user,
        got,
        &format!("asking launchd for gui/{uid}: launchctl print failed"),
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
    // Neither domain at the first look (before and after its bootout); `gui/<uid>` at
    // the last, where it goes before its bootout and is there again after it.
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    let mut script = Vec::new();
    for _ in 0..2 {
        script.extend([no_domain(&gui), no_domain(&user)]);
    }
    for _ in 0..2 {
        script.extend([found(), no_domain(&user)]);
    }
    rig.host.state().print_script.extend(script);
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
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    // The first kill's round: neither domain, before and after its bootout.
    let mut script = Vec::new();
    for _ in 0..2 {
        script.extend([no_domain(&gui), no_domain(&user)]);
    }
    // Each round of the second: `user/<uid>`, before and after its bootout.
    for _ in 0..KILL_ROUNDS * 2 {
        script.extend([no_domain(&gui), found()]);
    }
    rig.host.state().print_script.extend(script);
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

/// The calls of one `kill-uid` round of `uid` that finds no process left: the kill, then
/// for each domain `launchctl print` (and `bootout` of those in `booted`), the look,
/// and `launchctl print` of each domain again.
fn round(uid: u32, booted: &[&str]) -> Vec<String> {
    let mut calls = vec![format!("kill_all {uid}")];
    for kind in ["gui", "user"] {
        calls.push(format!("launchctl print {kind}/{uid}"));
        if booted.contains(&kind) {
            calls.push(format!("bootout {kind}/{uid}"));
        }
    }
    calls.push(format!("live_processes {uid}"));
    calls.extend(["gui", "user"].map(|kind| format!("launchctl print {kind}/{uid}")));
    calls
}

/// Catches the refusal that left every Mac job's user behind: a pool user never logs in,
/// so launchd has no `gui/<uid>` (and, once its agents are gone, no `user/<uid>`), and
/// `launchctl print` answers exit 112 "Could not find domain". That answer is absence:
/// `kill-uid` succeeds and boots out nothing. Red when 112 is read as a failure, or when
/// a domain launchd does not have is booted out anyway.
#[test]
fn kill_uid_of_a_uid_with_no_domain_boots_out_nothing() {
    let (rig, uid) = one("teardown-no-domain", "32.1");
    rig.host.state().procs.insert(uid, 1);
    assert_eq!(rig.helper.kill_uid("32.1"), Ok(()));
    // The kill's round, then (after the crontab and `at` jobs are swept) the second's.
    let want = [round(uid, &[]), round(uid, &[])].concat();
    assert_eq!(log(&rig), want);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
}

/// Catches a teardown that boots out a domain launchd does not have, or skips one it
/// has: with only `user/<uid>` (a user whose agent runs, never logged in), that domain
/// alone is booted out, once, after the kill, and the second kill finds nothing to boot
/// out.
#[test]
fn kill_uid_of_a_uid_with_only_a_user_domain_boots_out_that_one() {
    let (rig, uid) = one("teardown-user-only", "33.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
    }
    assert_eq!(rig.helper.kill_uid("33.1"), Ok(()));
    let want = [round(uid, &["user"]), round(uid, &[])].concat();
    assert_eq!(log(&rig), want);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
}

/// Catches a teardown that boots out one domain of a logged-in user and leaves the
/// other: with `gui/<uid>` and `user/<uid>`, each is looked at and booted out, the GUI
/// domain first, both after the kill; the second kill finds neither.
#[test]
fn kill_uid_of_a_uid_with_both_domains_boots_out_both() {
    let (rig, uid) = one("teardown-both", "34.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 2);
        state.domains.insert(format!("gui/{uid}"));
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
    }
    assert_eq!(rig.helper.kill_uid("34.1"), Ok(()));
    let want = [round(uid, &["gui", "user"]), round(uid, &[])].concat();
    assert_eq!(log(&rig), want);
    assert_eq!(after(&rig, uid), (Vec::new(), 0, 0));
}

/// Catches a `launchctl print` failure read as a domain that is gone, which would let
/// `kill-uid` report success while launchd still serves the uid: an exit status that is
/// not launchd's not-found answer, its not-found exit status without the words, and the
/// words with another exit status are each an error naming the domain and what
/// launchctl said; nothing is booted out, and the uid stays held.
#[test]
fn a_launchctl_print_failure_that_is_not_not_found_refuses_kill_uid() {
    let (rig, uid) = one("teardown-print-fails", "35.1");
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    let failed = |code, stderr: &str| PrintAnswer {
        code: Some(code),
        stderr: stderr.to_owned(),
    };
    for (script, want) in [
        (
            vec![failed(5, "Input/output error\n")],
            format!("asking launchd for {gui}: launchctl print exited 5: Input/output error"),
        ),
        (
            vec![no_domain(&gui), failed(112, "Bad request.\n")],
            format!("asking launchd for {user}: launchctl print exited 112: Bad request."),
        ),
        (
            vec![failed(1, "Could not find domain for user gui: 7\n")],
            format!(
                "asking launchd for {gui}: launchctl print exited 1: Could not find domain \
                 for user gui: 7"
            ),
        ),
    ] {
        rig.host.state().print_script.extend(script);
        let got = rig.helper.kill_uid("35.1");
        held(&rig, "kbf-lease-35-1", got, &want);
        let calls = log(&rig);
        assert!(
            !calls.iter().any(|c| c.starts_with("bootout")),
            "{calls:#?}"
        );
    }
}

/// `launchctl print`'s answer when it fails with `code` and `stderr`.
fn print_failed(code: i32, stderr: &str) -> PrintAnswer {
    PrintAnswer {
        code: Some(code),
        stderr: stderr.to_owned(),
    }
}

/// Catches a bootout failure taken as final when the domain went between the look and
/// the bootout: launchd shows `gui/<uid>`, the bootout then finds no such domain, and
/// the look after it finds the domain gone. `kill-uid` and `user-delete` both complete.
#[test]
fn a_domain_gone_between_the_look_and_its_bootout_is_no_refusal() {
    let (rig, uid) = one("teardown-gone-before-bootout", "36.1");
    // Only the first look sees the domain; launchd (the fake's state) has none.
    rig.host.state().print_script.push_back(found());
    assert_eq!(rig.helper.kill_uid("36.1"), Ok(()));
    let mut want = vec![
        format!("kill_all {uid}"),
        format!("launchctl print gui/{uid}"),
        format!("bootout gui/{uid}"),
    ];
    want.extend(round(uid, &[]).into_iter().skip(2));
    want.extend(round(uid, &[]));
    assert_eq!(log(&rig), want);

    rig.host.state().print_script.push_back(found());
    assert_eq!(rig.helper.user_delete("36.1"), Ok(true));
    let calls = log(&rig);
    let bootouts: Vec<&String> = calls.iter().filter(|c| c.starts_with("bootout")).collect();
    assert_eq!(bootouts, [&format!("bootout gui/{uid}")]);
    assert!(calls.contains(&"delete_user kbf-lease-36-1".to_owned()));
}

/// Catches a domain skipped at the look before the bootout and never looked at again:
/// launchd makes `gui/<uid>` after the first look found none. `kill-uid` sees it at the
/// look after the bootout, goes round again and boots it out; `user-delete` refuses
/// naming it, and keeps the record and the uid.
#[test]
fn a_domain_that_appears_after_the_first_look_is_found_by_the_second() {
    let (rig, uid) = one("teardown-appears", "37.1");
    let gui = format!("gui/{uid}");
    rig.host.state().domains.insert(gui.clone());
    rig.host.state().print_script.push_back(no_domain(&gui));
    assert_eq!(rig.helper.kill_uid("37.1"), Ok(()));
    let mut want = vec![
        format!("kill_all {uid}"),
        format!("launchctl print gui/{uid}"),
        format!("launchctl print user/{uid}"),
        format!("live_processes {uid}"),
        format!("launchctl print gui/{uid}"),
        format!("launchctl print user/{uid}"),
        "pause".to_owned(),
    ];
    want.extend(round(uid, &["gui"]));
    want.extend(round(uid, &[]));
    assert_eq!(log(&rig), want);
    assert!(rig.host.state().domains.is_empty());

    let (rig, uid) = one("teardown-appears-delete", "38.1");
    let gui = format!("gui/{uid}");
    rig.host.state().domains.insert(gui.clone());
    rig.host.state().print_script.push_back(no_domain(&gui));
    let got = rig.helper.user_delete("38.1");
    held(
        &rig,
        "kbf-lease-38-1",
        got,
        &format!("launchd domains of uid {uid} remain: {gui}; kill-uid first"),
    );
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("bootout") || c.starts_with("delete_user")),
        "{calls:#?}"
    );
}

/// Catches a failure of the look after the bootout read as absence: the looks before
/// the bootout work, and a later one fails (launchctl not running, or another exit),
/// in `kill-uid`'s first and second pass and in `user-delete`'s first and last look.
/// Each refuses naming the domain, deletes nothing, and keeps the uid held.
#[test]
fn a_print_that_fails_after_the_bootout_refuses_the_teardown() {
    let (rig, uid) = one("teardown-verify-fails", "39.1");
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    let absent = |n: usize| -> Vec<PrintAnswer> {
        (0..n)
            .map(|i| no_domain(if i % 2 == 0 { &gui } else { &user }))
            .collect()
    };
    let io = print_failed(5, "Input/output error\n");
    // The first pass's look at `gui/<uid>` after the bootout cannot run launchctl.
    rig.host
        .state()
        .fail_script
        .insert("launchctl print", VecDeque::from([false, false, true]));
    let got = rig.helper.kill_uid("39.1");
    let want = format!("asking launchd for {gui}: launchctl print failed");
    held(&rig, "kbf-lease-39-1", got, &want);
    // The second pass's look at `user/<uid>` after the bootout exits 5.
    let mut script = absent(4 + 3);
    script.push(io.clone());
    rig.host.state().print_script.extend(script);
    let got = rig.helper.kill_uid("39.1");
    let want = format!("asking launchd for {user}: launchctl print exited 5: Input/output error");
    held(&rig, "kbf-lease-39-1", got, &want);
    // `user-delete`'s first look after its bootout, then its last.
    for before in [2, 4 + 3] {
        let mut script = absent(before);
        script.push(io.clone());
        rig.host.state().print_script.extend(script);
        let got = rig.helper.user_delete("39.1");
        let domain = if before == 2 { &gui } else { &user };
        let want =
            format!("asking launchd for {domain}: launchctl print exited 5: Input/output error");
        held(&rig, "kbf-lease-39-1", got, &want);
        assert!(rig.host.state().print_script.is_empty());
    }
    let calls = log(&rig);
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("bootout") || c.starts_with("delete_user")),
        "{calls:#?}"
    );
}

/// Catches a teardown that, after booting out `gui/<uid>`, reads a failed look at
/// `user/<uid>` as absence: the GUI domain is booted out, then the look at the user
/// domain exits 5. `kill-uid` and `user-delete` refuse naming `user/<uid>`, the record
/// stays and the uid is held.
#[test]
fn a_print_that_fails_after_the_gui_domain_went_refuses_the_teardown() {
    let (rig, uid) = one("teardown-partial", "40.1");
    let (gui, user) = (format!("gui/{uid}"), format!("user/{uid}"));
    let want = format!("asking launchd for {user}: launchctl print exited 5: Input/output error");
    let io = print_failed(5, "Input/output error\n");
    rig.host.state().domains.insert(gui.clone());
    rig.host.state().print_script.extend([found(), io.clone()]);
    let got = rig.helper.kill_uid("40.1");
    assert_eq!(
        log(&rig),
        [
            format!("kill_all {uid}"),
            format!("launchctl print {gui}"),
            format!("bootout {gui}"),
            format!("launchctl print {user}"),
        ]
    );
    held(&rig, "kbf-lease-40-1", got, &want);
    assert!(rig.host.state().domains.is_empty());

    rig.host.state().domains.insert(gui.clone());
    rig.host.state().print_script.extend([found(), io]);
    let got = rig.helper.user_delete("40.1");
    held(&rig, "kbf-lease-40-1", got, &want);
    let calls = log(&rig);
    assert!(calls.contains(&format!("bootout {gui}")), "{calls:#?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("delete_user")),
        "{calls:#?}"
    );
    assert!(rig.host.state().domains.is_empty());
}
