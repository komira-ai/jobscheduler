//! `kill-uid` judged by its outcome against the fake launchd and process table: a uid
//! with no GUI login session is never asked about `gui/<uid>`, whatever `launchctl
//! print` would answer; a uid with a session has both domains booted out; a bootout's
//! own exit status decides nothing; and only an outcome that still does not hold after
//! the last attempt fails, quarantining the uid.

use super::*;
use crate::testing::found;

/// A rig of ONE uid holding `lease` from before the pool ([`legacy`]): a later lease gets
/// the uid only once its user is deleted and the pool's slot made.
fn one(name: &str, lease: &str) -> (Rig, u32) {
    legacy(name, lease)
}

/// The calls of one attempt of `kill-uid` that ends with nothing left: the kill, the
/// bootout of each of `domains` (of `uid`), the look at the processes, and `launchctl
/// print` of each of `domains`.
fn attempt(uid: u32, domains: &[&str]) -> Vec<String> {
    let mut calls = vec![format!("kill_all {uid}")];
    calls.extend(domains.iter().map(|d| format!("bootout {d}/{uid}")));
    calls.push(format!("live_processes {uid}"));
    calls.extend(domains.iter().map(|d| format!("launchctl print {d}/{uid}")));
    calls
}

/// The calls of a `kill-uid` of `uid` whose first and second pass each end after one
/// attempt: the look for a GUI session, then an attempt, the sweep of the crontab and
/// `at` jobs (no call), and another attempt.
fn two_passes(uid: u32, domains: &[&str]) -> Vec<String> {
    let mut calls = vec![format!("live_processes {uid}")];
    calls.extend(attempt(uid, domains));
    calls.extend(attempt(uid, domains));
    calls
}

/// `launchctl print`'s answer when it exits `code` with `stderr`.
fn answer(code: i32, stderr: &str) -> PrintAnswer {
    PrintAnswer {
        code: Some(code),
        stderr: stderr.to_owned(),
    }
}

/// launchd's answers for a domain it does not have, as Macs have given them (112 and
/// 113 "Could not find domain", 125 "Domain does not support specified action"), and
/// others no teardown may depend on.
fn absent_answers() -> Vec<PrintAnswer> {
    vec![
        answer(
            112,
            "Bad request.\nCould not find domain for user gui: 59126\n",
        ),
        answer(113, "Could not find domain for user: 59126\n"),
        answer(
            125,
            "Could not print domain: 125: Domain does not support specified action\n",
        ),
        answer(1, "Operation not permitted\n"),
        answer(64, ""),
    ]
}

/// Asserts that no call of `calls` names `gui/<uid>`.
fn never_asked_about_gui(uid: u32, calls: &[String]) {
    let gui = format!("gui/{uid}");
    assert!(!calls.iter().any(|c| c.contains(&gui)), "{calls:#?}");
}

/// Catches the refusal a Mac's two-run isolation test hit: a pool user never logs in,
/// and `launchctl print gui/<uid>` for it answers 112, 113 or 125 from run to run, so a
/// teardown that asks about `gui/<uid>` and matches exit statuses fails on the one it
/// does not know. A uid with no login session is never asked about `gui/<uid>`, and
/// `kill-uid` and `user-delete` both complete whatever `launchctl print` answers for a
/// domain that is not there.
#[test]
fn a_session_less_uid_is_never_asked_about_its_gui_domain_whatever_print_answers() {
    for (n, absent) in absent_answers().into_iter().enumerate() {
        let lease = format!("51.{}", n + 1);
        let code = absent.code.unwrap();
        let (rig, uid) = one(&format!("outcome-sessionless-{code}"), &lease);
        {
            let mut state = rig.host.state();
            state.procs.insert(uid, 1);
            state.domains.insert(format!("user/{uid}"));
            state.agents.insert(uid);
            state.absent_print = Some(absent);
        }
        assert_eq!(rig.helper.kill_uid(&lease), Ok(()), "exit {code}");
        let calls = log(&rig);
        assert_eq!(calls, two_passes(uid, &["user"]), "exit {code}");
        never_asked_about_gui(uid, &calls);
        assert!(rig.host.state().domains.is_empty(), "exit {code}");

        assert_eq!(rig.helper.user_delete(&lease), Ok(true), "exit {code}");
        never_asked_about_gui(uid, &log(&rig));
    }
}

/// Catches a GUI login session left behind, or a session looked for after the kill
/// ended its login window: a uid that owns a `loginwindow` process (a whole-machine
/// lease's user, the VM user) has `gui/<uid>` and then `user/<uid>` booted out in every
/// attempt, and both judged gone even when `launchctl print` answers 125 for them.
#[test]
fn a_uid_with_a_gui_session_gets_both_domains_booted_out() {
    let (rig, uid) = one("outcome-gui", "52.1");
    {
        let mut state = rig.host.state();
        state.gui.insert(uid);
        state.procs.insert(uid, 2);
        state.domains.insert(format!("gui/{uid}"));
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
        state.absent_print = Some(absent_answers().remove(2));
    }
    assert_eq!(rig.helper.kill_uid("52.1"), Ok(()));
    assert_eq!(log(&rig), two_passes(uid, &["gui", "user"]));
    let state = rig.host.state();
    assert!(state.domains.is_empty());
    assert!(
        !state.gui.contains(&uid),
        "the login window outlived the kill"
    );
}

/// Catches a bootout's exit status taken as the verdict: `launchctl bootout` can end the
/// domain and still exit non-zero ("Boot-out failed: 5: Input/output error"), and it
/// fails for a domain that is not there (each second pass here). With no process and no
/// domain left after it, `kill-uid` succeeds at its first attempt.
#[test]
fn a_bootout_that_exits_non_zero_but_leaves_nothing_succeeds() {
    let (rig, uid) = one("outcome-bootout-fails", "53.1");
    let user = format!("user/{uid}");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.domains.insert(user.clone());
        state.agents.insert(uid);
        state.bootout_fails_anyway.insert(user);
        state.absent_print = Some(absent_answers().remove(2));
    }
    assert_eq!(rig.helper.kill_uid("53.1"), Ok(()));
    assert_eq!(log(&rig), two_passes(uid, &["user"]));
    assert!(rig.host.state().domains.is_empty());
}

/// Catches a teardown that stops at its first look when something came back: the first
/// bootout fails and leaves `user/<uid>`, so launchd restarts the agent the first kill
/// ended (once). The look sees it and the domain, and the second attempt kills it and
/// boots the domain out; nothing is left.
#[test]
fn a_process_that_respawns_once_is_killed_on_the_second_attempt() {
    let (rig, uid) = one("outcome-respawn", "54.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.domains.insert(format!("user/{uid}"));
        state.agents.insert(uid);
        state.respawns.insert(uid, 1);
        state
            .fail_script
            .insert("bootout user", std::collections::VecDeque::from([true]));
    }
    assert_eq!(rig.helper.kill_uid("54.1"), Ok(()));
    let mut want = vec![
        format!("live_processes {uid}"),
        format!("kill_all {uid}"),
        format!("bootout user/{uid}"),
        format!("live_processes {uid}"),
        format!("launchctl print user/{uid}"),
        "pause".to_owned(),
    ];
    want.extend(attempt(uid, &["user"]));
    want.extend(attempt(uid, &["user"]));
    assert_eq!(log(&rig), want);
    let state = rig.host.state();
    assert_eq!(state.respawns[&uid], 0, "launchd never restarted the agent");
    assert!(state.domains.is_empty());
    assert!(state.agents.is_empty());
}

/// Catches a teardown that frees a uid something of which still runs, that gives up
/// before its last attempt or never does, or that does not say what remains: a process
/// that outlives every kill fails `kill-uid` after exactly [`KILL_ROUNDS`] attempts,
/// naming it; the uid is quarantined (the record stays, no other lease gets the uid, and
/// `user-delete` refuses), until a teardown whose outcome holds frees it.
#[test]
fn a_process_that_never_goes_away_fails_after_the_last_attempt_and_quarantines_the_uid() {
    let (rig, uid) = one("outcome-stuck", "55.1");
    rig.host.state().procs.insert(uid, 1);
    rig.host.state().stubborn.insert(uid, usize::MAX);
    let got = rig.helper.kill_uid("55.1");
    assert_eq!(
        got,
        Err(format!(
            "1 processes of uid {uid} remain after {KILL_ROUNDS} attempts: 1000 (proc0); \
             uid {uid} is quarantined: no other lease gets it until a delete succeeds"
        ))
    );
    let calls = log(&rig);
    let kills = calls.iter().filter(|c| c.starts_with("kill_all")).count();
    assert_eq!(kills, KILL_ROUNDS, "{calls:#?}");
    let quarantined = || {
        assert!(rig.host.state().users.contains_key("kbf-lease-55-1"));
        uid_held(&rig);
    };
    quarantined();
    assert_eq!(
        rig.helper.user_delete("55.1"),
        Err(format!(
            "1 processes of uid {uid} remain after {EXIT_LOOKS} looks: 1000 (proc0); \
             kill-uid first"
        ))
    );
    quarantined();

    // The process is gone: the repeated teardown holds, and the uid is free again.
    rig.host.state().stubborn.clear();
    assert_eq!(rig.helper.kill_uid("55.1"), Ok(()));
    assert_eq!(rig.helper.user_delete("55.1"), Ok(true));
    assert!(rig.helper.pool_create().errors.is_empty());
    assert_eq!(rig.helper.user_create("55.2", None), Ok(uid));
}

/// Catches a domain read as gone when `launchctl print` printed it: `user/<uid>` prints
/// again at the look after its bootout (launchd made it again), so `kill-uid` goes
/// round again, boots it out and judges it once more.
#[test]
fn a_domain_that_prints_after_its_bootout_is_booted_out_again() {
    let (rig, uid) = one("outcome-prints-again", "56.1");
    rig.host.state().print_script.push_back(found());
    assert_eq!(rig.helper.kill_uid("56.1"), Ok(()));
    let mut want = vec![format!("live_processes {uid}")];
    want.extend(attempt(uid, &["user"]));
    want.push("pause".to_owned());
    want.extend(attempt(uid, &["user"]));
    want.extend(attempt(uid, &["user"]));
    assert_eq!(log(&rig), want);
}
