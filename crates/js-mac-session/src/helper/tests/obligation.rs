//! The edges of `kill-uid`'s GUI-session decision and its attempt loop: a GUI teardown
//! that failed stays owed across a retry, a restart of the helper and a direct delete; a
//! session that only the intent record, a later look, or an unreadable process name
//! shows; a process named like the login window with no session; processes and domains
//! that both remain; recovery on the last allowed attempt; and a process table that
//! cannot be read.

use std::collections::VecDeque;

use super::*;
use crate::autologin::{Intent, write_intent};

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

/// The record that `uid`'s GUI domain is owed a teardown, in the rig.
fn owed(rig: &Rig, uid: u32) -> bool {
    rig.dir.join(format!("gui-teardown.{uid}")).exists()
}

/// A uid with a GUI session whose `gui/<uid>` bootout fails: the login window is
/// ended by the kill, the domain stays.
fn stuck_gui(rig: &Rig, uid: u32) {
    let mut state = rig.host.state();
    state.gui.insert(uid);
    state.domains.insert(format!("gui/{uid}"));
    state.fail.insert("bootout gui");
}

/// The refusal of a `kill-uid` whose `gui/<uid>` stayed.
fn gui_stayed(uid: u32) -> String {
    format!(
        "launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: gui/{uid} \
         (bootout: bootout gui failed); uid {uid} is quarantined: no other lease gets it \
         until a delete succeeds"
    )
}

/// Catches a GUI teardown obligation lost with the login window: the kill ends
/// `loginwindow`, the bootout of `gui/<uid>` fails and the domain stays. A retried
/// `kill-uid` and a direct `user-delete` (which sees no process, so no session) still
/// boot out and judge `gui/<uid>`, refusing while it stays; the record survives both
/// refusals, and a delete that judges it gone discharges it and frees the uid.
#[test]
fn a_failed_gui_teardown_stays_owed_across_a_retry_and_a_direct_delete() {
    let (rig, uid) = one("owed-retry", "61.1");
    let gui = format!("gui/{uid}");
    stuck_gui(&rig, uid);
    assert_eq!(rig.helper.kill_uid("61.1"), Err(gui_stayed(uid)));
    assert!(owed(&rig, uid));
    assert!(
        !rig.host.state().gui.contains(&uid),
        "the login window lives"
    );
    log(&rig);
    assert_eq!(rig.helper.kill_uid("61.1"), Err(gui_stayed(uid)));
    assert_eq!(count(&log(&rig), &format!("bootout {gui}")), KILL_ROUNDS);
    assert_eq!(
        rig.helper.user_delete("61.1"),
        Err(format!(
            "launchd domains of uid {uid} remain: {gui} (bootout: bootout gui failed); \
             kill-uid first"
        ))
    );
    assert!(owed(&rig, uid));
    uid_held(&rig);

    rig.host.state().fail.clear();
    assert_eq!(rig.helper.user_delete("61.1"), Ok(true));
    let calls = log(&rig);
    assert_eq!(count(&calls, &format!("bootout {gui}")), 3, "{calls:#?}");
    assert!(!owed(&rig, uid));
    assert!(rig.host.state().domains.is_empty());
    assert_eq!(rig.helper.user_create("61.2", None), Ok(uid));
}

/// Catches a GUI teardown obligation kept only in memory: after the failed `kill-uid`
/// the helper restarts; its boot reset (which ends every live lease) boots out and
/// judges `gui/<uid>` though no login window is left, fails while the domain stays,
/// and completes once it goes.
#[test]
fn a_failed_gui_teardown_stays_owed_across_a_restart() {
    let (rig, uid) = one("owed-restart", "62.1");
    stuck_gui(&rig, uid);
    assert!(rig.helper.kill_uid("62.1").is_err());
    let range = rig.helper.settings.range;
    let dir = rig.dir.clone();
    drop(rig);
    let rig = rig_with("owed-restart", range, Some(dir));
    {
        let mut state = rig.host.state();
        state.domains.insert(format!("gui/{uid}"));
        state.fail.insert("bootout gui");
    }
    let got = rig.helper.boot_reset(&[]).unwrap_err();
    assert_eq!(
        got,
        format!(
            "the boot reset is incomplete: lease 62.1: {}",
            gui_stayed(uid)
        )
    );
    assert!(owed(&rig, uid));
    rig.host.state().fail.clear();
    assert_eq!(rig.helper.boot_reset(&[]).unwrap().deleted.len(), 1);
    assert!(!owed(&rig, uid));
    assert!(rig.host.state().domains.is_empty());
}

/// Catches a GUI session missed because it is not seen at the first look: one that the
/// intent record names (the login window logs in only that user, so a login may be on
/// its way); one whose login completes after the first look, during the first pass;
/// and one that completes between the passes. Each has `gui/<uid>` booted out and
/// judged, from the attempt after it was seen on, and discharged once it is gone.
#[test]
fn a_session_seen_after_the_first_look_is_torn_down_too() {
    let (rig, uid) = one("owed-later", "63.1");
    let gui = format!("bootout gui/{uid}");
    write_intent(
        &rig.dir.join("session-intent"),
        Intent::Lease(LeaseId::new(63, 1)),
    )
    .unwrap();
    assert_eq!(rig.helper.kill_uid("63.1"), Ok(()));
    let calls = log(&rig);
    assert_eq!(count(&calls, &gui), 2, "{calls:#?}");
    assert!(!owed(&rig, uid));
    crate::autologin::remove(&rig.dir.join("session-intent")).unwrap();

    // At the first attempt's look, then at the second pass's: each time the next
    // attempt boots out `gui/<uid>`.
    for (after, want) in [(1, 2), (2, 1)] {
        {
            let mut state = rig.host.state();
            state.login_after.insert(uid, after);
            state.domains.insert(format!("gui/{uid}"));
        }
        assert_eq!(rig.helper.kill_uid("63.1"), Ok(()), "after {after}");
        let calls = log(&rig);
        assert_eq!(count(&calls, &gui), want, "after {after}: {calls:#?}");
        let first = calls.iter().position(|c| *c == gui).unwrap();
        // Seen at a look of the processes before that attempt.
        let looks = count(&calls[..first], &format!("live_processes {uid}"));
        assert_eq!(looks, after + 1, "after {after}: {calls:#?}");
        assert!(rig.host.state().domains.is_empty(), "after {after}");
        assert!(!owed(&rig, uid), "after {after}");
    }
}

/// Catches a session decision that trusts a name it could not read, or that a job can
/// fool into harm: a process whose name cannot be read does not rule a session out, so
/// `gui/<uid>` is judged; a job of a session-less uid named like the login window only
/// costs a bootout of `gui/<uid>` that fails and a look that finds it gone (any
/// answer but 0), and the teardown completes.
#[test]
fn an_unreadable_name_or_a_job_named_loginwindow_is_judged_safely() {
    let (rig, uid) = one("owed-names", "64.1");
    let gui = format!("bootout gui/{uid}");
    {
        let mut state = rig.host.state();
        state.unreadable.insert(uid);
        state.domains.insert(format!("gui/{uid}"));
    }
    assert_eq!(rig.helper.kill_uid("64.1"), Ok(()));
    assert_eq!(count(&log(&rig), &gui), 2);
    assert!(rig.host.state().domains.is_empty());

    // No session: the name alone. `launchctl print gui/<uid>` answers 125.
    {
        let mut state = rig.host.state();
        state.gui.insert(uid);
        state.absent_print = Some(PrintAnswer {
            code: Some(125),
            stderr: "Could not print domain: 125: Domain does not support specified action\n"
                .to_owned(),
        });
    }
    assert_eq!(rig.helper.kill_uid("64.1"), Ok(()));
    let calls = log(&rig);
    assert_eq!(count(&calls, &gui), 2, "{calls:#?}");
    assert!(!owed(&rig, uid));
    assert_eq!(rig.helper.user_delete("64.1"), Ok(true));
    assert!(!log(&rig).iter().any(|c| c.contains("gui/")));
}

/// Catches a final refusal that names only one kind of what remains: a process that
/// outlives every kill, and `user/<uid>` whose `launchctl print` a signal ends at every
/// look, are named together, in full. The uid stays held and `user-delete` refuses.
#[test]
fn processes_and_domains_that_both_remain_are_named_together() {
    let (rig, uid) = one("owed-both", "65.1");
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 1);
        state.stubborn.insert(uid, usize::MAX);
        state.absent_print = Some(PrintAnswer {
            code: None,
            stderr: "killed\n".to_owned(),
        });
    }
    assert_eq!(
        rig.helper.kill_uid("65.1"),
        Err(format!(
            "1 processes of uid {uid} remain after {KILL_ROUNDS} attempts: 1000 (proc0); \
             launchd domains of uid {uid} remain after {KILL_ROUNDS} attempts: user/{uid} \
             (launchctl print: launchctl print was ended by a signal: killed); uid {uid} is \
             quarantined: no other lease gets it until a delete succeeds"
        ))
    );
    assert_eq!(
        rig.helper.user_delete("65.1"),
        Err(format!(
            "1 processes of uid {uid} remain after {EXIT_LOOKS} looks: 1000 (proc0); \
             kill-uid first"
        ))
    );
    assert!(rig.host.state().users.contains_key("kbf-lease-65-1"));
    uid_held(&rig);
}

/// Catches a bound off by one for the domains: an outcome that holds only at the last
/// allowed attempt is a success, for `user/<uid>` whose bootout fails until then, for a
/// `launchctl print` with no answer until then, and for a GUI session's `gui/<uid>`
/// whose bootout fails until then. Each first pass takes exactly [`KILL_ROUNDS`]
/// attempts, the second one.
#[test]
fn a_domain_outcome_that_holds_at_the_last_attempt_is_a_success() {
    let fails = || VecDeque::from(vec![true; KILL_ROUNDS - 1]);
    for case in ["bootout user", "no answer", "bootout gui"] {
        let (rig, uid) = one(&format!("owed-last-{}", case.replace(' ', "-")), "66.1");
        {
            let mut state = rig.host.state();
            match case {
                "bootout user" => {
                    state.domains.insert(format!("user/{uid}"));
                    state.fail_script.insert("bootout user", fails());
                }
                "no answer" => state.print_script.extend(vec![
                    PrintAnswer {
                        code: None,
                        stderr: String::new(),
                    };
                    KILL_ROUNDS - 1
                ]),
                _ => {
                    state.gui.insert(uid);
                    state.domains.insert(format!("gui/{uid}"));
                    state.fail_script.insert("bootout gui", fails());
                }
            }
        }
        assert_eq!(rig.helper.kill_uid("66.1"), Ok(()), "{case}");
        let calls = log(&rig);
        assert_eq!(
            count(&calls, &format!("kill_all {uid}")),
            KILL_ROUNDS + 1,
            "{case}"
        );
        assert!(rig.host.state().domains.is_empty(), "{case}");
        assert!(!owed(&rig, uid), "{case}");
    }
}

/// Catches a teardown that goes on past a process table it could not read: at the look
/// for a session before the first kill, and at a look after a bootout in a later
/// attempt. Either stops `kill-uid` at once (it is not retried: the caller repeats the
/// teardown) with the uid named, before the crontab is swept, and the uid stays held.
#[test]
fn a_process_table_that_cannot_be_read_stops_kill_uid() {
    for (n, script) in [vec![true], vec![false, false, false, true]]
        .into_iter()
        .enumerate()
    {
        let lease = format!("67.{}", n + 1);
        let (rig, uid) = one(&format!("owed-listing-{n}"), &lease);
        let tabs = rig.dir.join("tabs");
        let tab = tabs.join(format!("kbf-lease-67-{}", n + 1));
        std::fs::create_dir_all(&tabs).unwrap();
        std::fs::write(&tab, "* * * * * job").unwrap();
        {
            let mut state = rig.host.state();
            state.procs.insert(uid, 1);
            state.stubborn.insert(uid, 2);
            state
                .fail_script
                .insert("live_processes", VecDeque::from(script));
        }
        assert_eq!(
            rig.helper.kill_uid(&lease),
            Err(format!(
                "listing the processes of uid {uid}: live_processes failed"
            ))
        );
        let kills = count(&log(&rig), &format!("kill_all {uid}"));
        assert_eq!(kills, [0, 3][n]);
        assert!(tab.exists(), "the crontab was swept");
        assert!(
            rig.host
                .state()
                .users
                .contains_key(&format!("kbf-lease-67-{}", n + 1))
        );
        uid_held(&rig);
    }
}

/// Catches a GUI teardown record that cannot be kept taken as no obligation: a link where
/// the record should be is refused (never followed) before anything is killed, by
/// `kill-uid` and `user-delete`; a record that cannot be written (a directory there)
/// refuses `kill-uid` before the kill; and one that cannot be removed refuses the delete
/// before the user's record goes. The uid stays held throughout.
#[test]
fn a_gui_teardown_record_that_cannot_be_kept_refuses_the_teardown() {
    let (rig, uid) = one("owed-record", "68.1");
    let record = rig.dir.join(format!("gui-teardown.{uid}"));
    let refusal = |got: Result<(), String>| {
        let got = got.unwrap_err();
        let want = format!(
            "the GUI teardown record of uid {uid} ({}): ",
            record.display()
        );
        assert!(got.starts_with(&want), "{got}");
    };
    std::os::unix::fs::symlink(rig.dir.join("elsewhere"), &record).unwrap();
    refusal(rig.helper.kill_uid("68.1"));
    refusal(rig.helper.user_delete("68.1").map(drop));
    assert!(!log(&rig).iter().any(|c| c.starts_with("kill_all")));
    // Refused too when the intent record names the lease (a session is decided without
    // the record): the link is not replaced.
    let intent = rig.dir.join("session-intent");
    write_intent(&intent, Intent::Lease(LeaseId::new(68, 1))).unwrap();
    refusal(rig.helper.kill_uid("68.1"));
    assert!(record.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(!rig.dir.join("elsewhere").exists());
    assert!(!log(&rig).iter().any(|c| c.starts_with("kill_all")));
    crate::autologin::remove(&intent).unwrap();
    std::fs::remove_file(&record).unwrap();

    // A directory: read as owed, but neither written nor removed.
    std::fs::create_dir_all(record.join("held")).unwrap();
    refusal(rig.helper.kill_uid("68.1"));
    assert!(!log(&rig).iter().any(|c| c.starts_with("kill_all")));
    refusal(rig.helper.user_delete("68.1").map(drop));
    let calls = log(&rig);
    assert!(calls.contains(&format!("bootout gui/{uid}")), "{calls:#?}");
    assert!(!calls.iter().any(|c| c.starts_with("delete_user")));
    assert!(rig.host.state().users.contains_key("kbf-lease-68-1"));
    uid_held(&rig);
}

/// The names of the entries of a rig's state directory.
fn entries(rig: &Rig) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(rig.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

/// Catches a release that cannot take over the state a previous release left (which has
/// no GUI teardown record at all): a ledger written in the previous release's format,
/// holding a live lease, is opened by this one, whose boot reset reads the absent record
/// as nothing owed, never asks about `gui/<uid>`, and deletes the lease.
#[test]
fn a_previous_releases_state_directory_is_read_as_owing_nothing() {
    let dir = std::sync::Arc::new(crate::testing::scratch("helper-owed-previous"));
    let uid = me();
    std::fs::write(dir.join("ledger"), format!("created 70.1 {uid}\n")).unwrap();
    let rig = rig_with("owed-previous", UidRange::new(uid, uid).unwrap(), Some(dir));
    let reset = rig.helper.boot_reset(&[]).unwrap();
    assert_eq!(reset.deleted, [LeaseId::new(70, 1)]);
    let calls = log(&rig);
    assert!(!calls.iter().any(|c| c.contains("gui/")), "{calls:#?}");
    assert!(!owed(&rig, uid));
    assert_eq!(rig.helper.user_create("70.2", None), Ok(uid));
}

/// Catches a GUI teardown record that a previous release could trip on: it reads only the
/// files it names in the state directory (the ledger, the intent record, the scan's
/// baseline and window), never lists it. With a record left by a failed GUI teardown, the
/// ledger holds exactly the records of the previous release's format (and opens), and the
/// state directory differs from the one a failed teardown without a session leaves only
/// by the empty record.
#[test]
fn the_gui_teardown_record_changes_no_file_a_previous_release_reads() {
    let (gui, uid) = one("owed-new-data-gui", "71.1");
    stuck_gui(&gui, uid);
    assert!(gui.helper.kill_uid("71.1").is_err());
    let (plain, _) = one("owed-new-data-plain", "71.1");
    plain.host.state().procs.insert(uid, 1);
    plain.host.state().stubborn.insert(uid, usize::MAX);
    assert!(plain.helper.kill_uid("71.1").is_err());

    let record = format!("gui-teardown.{uid}");
    let mut with = entries(&gui);
    assert!(with.remove(&record), "{with:?}");
    assert_eq!(with, entries(&plain));
    assert_eq!(std::fs::read(gui.dir.join(&record)).unwrap(), b"");
    let ledger = gui.dir.join("ledger");
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        format!("created 71.1 {uid}\n")
    );
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        std::fs::read(plain.dir.join("ledger")).unwrap()
    );
    assert!(!gui.dir.join("session-intent").exists());
    crate::ledger::Ledger::open(&ledger).unwrap();
}

/// Catches a stale record (a previous release, rolled back to, completed a teardown and
/// left the record, and the uid went to a new lease) that blocks or misleads the new
/// lease's teardown: `gui/<uid>` is booted out and judged once (it does not print: any
/// answer but 0), the teardown completes and discharges the record, and the next one
/// does not ask about `gui/<uid>`.
#[test]
fn a_stale_gui_teardown_record_costs_one_look_and_is_discharged() {
    let (rig, uid) = one("owed-stale", "72.1");
    std::fs::write(rig.dir.join(format!("gui-teardown.{uid}")), b"").unwrap();
    assert_eq!(rig.helper.kill_uid("72.1"), Ok(()));
    assert_eq!(count(&log(&rig), &format!("bootout gui/{uid}")), 2);
    assert!(!owed(&rig, uid));
    assert_eq!(rig.helper.user_delete("72.1"), Ok(true));
    assert!(!log(&rig).iter().any(|c| c.contains("gui/")));
}
