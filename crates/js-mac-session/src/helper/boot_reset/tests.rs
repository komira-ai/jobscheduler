use crate::autologin::CommandOutput;
use crate::helper::tests::{Rig, add_legacy, fds, log, me, rig, rig_with};
use crate::helper::{Outcome, Reset};
use crate::lease::{Slot, UidRange};
use crate::proto::{Reply, Request};

use js_types::LeaseId;

fn domain(rig: &Rig) -> String {
    rig.dir.join("loginwindow").display().to_string()
}

fn position(log: &[String], prefix: &str) -> usize {
    log.iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} in {log:#?}"))
}

/// A helper restarted over `rig`'s state directory (a boot), its reset not yet run.
fn rebooted(rig: &Rig, name: &str) -> Rig {
    let range = UidRange::new(me(), me() + 3).unwrap();
    let booted = rig_with(name, range, Some(rig.dir.clone()));
    booted.host.state().users = rig.host.state().users.clone();
    booted
}

/// Asserts that the boot reset clears auto-login (`defaults delete` of `autoLoginUser`,
/// and `/etc/kcpassword` removed) before it ends anything, then ends every live lease, a
/// lease the server still holds included, its pool user reset in place (its GUI session
/// torn down first) and never deleted. Catches: a reset that leaves a Mac logging a user
/// in at its start, one that keeps a held lease's user over a restart (no lease keeps a
/// session now), one that resets a user before its GUI session is torn down, or one that
/// replaces a pool user it could reset.
#[test]
fn the_boot_reset_clears_auto_login_and_ends_every_lease() {
    let rig = rig("reset-every", 4);
    rig.helper.user_create("1.1").unwrap();
    rig.helper.user_create("1.2").unwrap();
    // Power is lost; at boot the lease user is logged in again by the login window.
    std::fs::write(rig.dir.join("kcpassword"), "x").unwrap();
    let booted = rebooted(&rig, "reset-every");
    booted.host.state().procs.insert(me(), 3);
    booted.host.state().gui.insert(me());
    booted.host.state().domains.insert(format!("gui/{}", me()));
    let reset = booted.helper.boot_reset(&["1.1".to_owned()]).unwrap();
    assert_eq!(
        reset,
        Reset {
            deleted: vec![LeaseId::new(1, 1), LeaseId::new(1, 2)],
        }
    );
    let calls = log(&booted);
    let domain = domain(&booted);
    let cleared = position(
        &calls,
        &format!("command /usr/bin/defaults delete {domain} autoLoginUser"),
    );
    let killed = position(&calls, &format!("bootout gui/{}", me()));
    let home = booted.dir.join("Users/kbf-lease-pool-0");
    let reset = position(&calls, &format!("make_home {}", home.display()));
    assert!(cleared < killed && killed < reset, "{calls:#?}");
    assert!(
        calls.iter().all(|call| !call.starts_with("delete_user")),
        "reset, not replaced: {calls:#?}"
    );
    assert_eq!(booted.host.state().users, rig.host.state().users);
    let free = (0..4).map(Slot::new).collect::<Vec<_>>();
    assert_eq!(booted.helper.pool_status().free, free);
    assert!(!booted.dir.join("kcpassword").exists());
}

/// Asserts that a boot reset whose auto-login clearing fails (`autoLoginUser` still set
/// after a failed `defaults delete`) fails, names why, ends no lease and lets no other verb
/// in; and that a held lease id that does not parse refuses the reset before anything is
/// cleared. Catches: a reset that serves leases on a Mac that still logs a user in at its
/// start, or one that takes a malformed held id.
#[test]
fn the_boot_reset_fails_when_auto_login_stays_or_a_held_id_is_malformed() {
    let rig = rig("reset-stays", 4);
    rig.helper.user_create("1.1").unwrap();
    let booted = rebooted(&rig, "reset-stays");
    let error = booted.helper.boot_reset(&["1.01".to_owned()]).unwrap_err();
    assert!(error.contains("not <term>.<seq>"), "{error}");
    assert!(log(&booted).is_empty(), "nothing done for a malformed id");
    let domain = domain(&booted);
    let answer = |success: bool, stdout: &str| CommandOutput {
        success,
        stdout: stdout.to_owned(),
        stderr: "said no".to_owned(),
    };
    {
        let mut state = booted.host.state();
        state.answers.insert(
            format!("/usr/bin/defaults delete {domain} autoLoginUser"),
            answer(false, ""),
        );
        state.answers.insert(
            format!("/usr/bin/defaults read {domain} autoLoginUser"),
            answer(true, "kbf-lease-pool-0\n"),
        );
    }
    let error = booted.helper.boot_reset(&[]).unwrap_err();
    assert!(
        error.contains("defaults delete") && error.contains("said no"),
        "{error}"
    );
    assert!(!booted.helper.reset_done());
    let calls = log(&booted);
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("kill_all") && !call.starts_with("make_home")),
        "{calls:#?}"
    );
    assert_eq!(
        booted.helper.pool_status().leased,
        [(Slot::new(0), LeaseId::new(1, 1))]
    );
}

/// Catches: a boot reset that fails part-way yet lets the other verbs in, or one that
/// can run twice (a second one would end the leases started since). The user that
/// cannot be ended is a lease's from before the pool, whose kill fails.
#[test]
fn the_boot_reset_runs_once_and_must_succeed() {
    let rig = rig("reset-once", 4);
    add_legacy(&rig, "1.1");
    let booted = rebooted(&rig, "reset-once");
    booted.host.state().stubborn.insert(me(), 100);
    booted.host.state().procs.insert(me(), 1);
    let error = booted.helper.boot_reset(&[]).unwrap_err();
    assert!(error.contains("lease 1.1:"), "{error}");
    assert!(!booted.helper.reset_done());
    booted.host.state().stubborn.clear();
    booted.helper.boot_reset(&[]).unwrap();
    assert!(booted.helper.reset_done());
    let again = booted.helper.boot_reset(&[]).unwrap_err();
    assert!(again.contains("runs once"), "{again}");
}

/// Catches: a verb served before the boot reset (the daemon could start a lease on a
/// Mac still logged in as a dead lease user), a reply to the reset that names a kept
/// lease, or a held lease that survives a restart.
#[test]
fn nothing_but_the_boot_reset_is_served_before_it() {
    let rig = rig("gate", 4);
    let booted = rebooted(&rig, "gate");
    let reply = |outcome: Outcome| match outcome {
        Outcome::Reply(reply) => reply,
        other => panic!("not a reply: {other:?}"),
    };
    let lease = || "2.1".to_owned();
    let create = Request::UserCreate {
        lease: lease(),
        grant: None,
    };
    for request in [
        create.clone(),
        Request::KillUid { lease: lease() },
        Request::UserDelete { lease: lease() },
        Request::Scan,
        Request::PoolCreate,
        Request::PoolStatus,
    ] {
        let refused = reply(booted.helper.handle(request.clone(), Vec::new()));
        assert_eq!(
            refused,
            Reply::Refused {
                reason: "the boot reset has not run since the helper started".to_owned()
            },
            "{request:?}"
        );
    }
    let run = Request::Run {
        lease: lease(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: Vec::new(),
        limits: None,
        deadline_ms: None,
    };
    assert!(matches!(
        booted.helper.handle(run, fds(&booted.dir)),
        Outcome::Reply(Reply::Refused { reason }) if reason.contains("boot reset has not run")
    ));
    assert_eq!(booted.host.state().users, rig.host.state().users);
    assert!(log(&booted).is_empty());
    let reset = Request::BootReset { held: Vec::new() };
    assert_eq!(
        reply(booted.helper.handle(reset, Vec::new())),
        Reply::Reset {
            kept: None,
            deleted: Vec::new()
        }
    );
    assert_eq!(
        reply(booted.helper.handle(create, Vec::new())),
        Reply::Created { uid: me() }
    );
    // A boot: the lease is held by the server, and still ended, and the reply keeps none.
    let booted_again = rebooted(&booted, "gate");
    let held = Request::BootReset {
        held: vec![lease()],
    };
    assert_eq!(
        reply(booted_again.helper.handle(held, Vec::new())),
        Reply::Reset {
            kept: None,
            deleted: vec![lease()]
        }
    );
}
