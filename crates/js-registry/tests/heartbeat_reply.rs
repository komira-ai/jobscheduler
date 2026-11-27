//! The reply to a heartbeat for an agent of the previous release that still keys its work by
//! run: the registry queues no run and answers a heartbeat that lists held runs and results
//! with no command and no result acknowledgement. The job half of the reply is the
//! transport's and is left empty here.

mod support;

use js_agent_wire::{Heartbeat, HeartbeatReply, HeldRun, HeldState, Outcome, RunResult, Taint};
use support::*;

fn held(run: &str) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state: HeldState::Running,
    }
}

fn result(run: &str, outcome: Outcome) -> RunResult {
    RunResult {
        run_id: run.into(),
        lease_id: "1-1".into(),
        outcome,
        exit_code: None,
        detail: None,
        log_tail: None,
    }
}

/// The reply to a heartbeat that sets nothing: no command, no acknowledgement and no grant.
fn empty_reply() -> HeartbeatReply {
    HeartbeatReply {
        commands: Vec::new(),
        result_acks: Vec::new(),
        taint_ack: false,
        taint_cleared_ack: false,
        host_taint: None,
        exclusive: None,
        job_acks: Vec::new(),
        retired_acks: Vec::new(),
        log_grant: None,
        beat_now: false,
    }
}

/// Asserts that a heartbeat that lists held runs, whatever their state, and results of every
/// outcome, one of them naming a lease the registry never saw, is answered with exactly the
/// empty reply: no command, no result acknowledgement, no taint acknowledgement, no
/// exclusive grant, no job or retired acknowledgement and no log grant. It catches a reply
/// that cancels a run the agent lists or acknowledges a result it carries.
#[test]
fn a_heartbeat_listing_held_runs_and_results_is_answered_with_an_empty_reply() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.held = vec![
        held("r1"),
        HeldRun {
            run_id: "r2".into(),
            state: HeldState::ExitedUnacked,
        },
        held("r3"),
    ];
    let mut elsewhere = result("r6", Outcome::Killed);
    elsewhere.lease_id = "9-9".into();
    beat.results = vec![
        result("r4", Outcome::Succeeded),
        result("r5", Outcome::Failed),
        elsewhere,
    ];
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(reply, empty_reply());
}

/// Asserts that more held runs than the wire's cap of commands still get the empty reply and
/// that the host stays listed. It catches a reply that names a run past the cap or refuses
/// the beat.
#[test]
fn a_long_held_list_is_answered_with_an_empty_reply() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.held = (0..=js_agent_wire::limits::MAX_COMMANDS)
        .map(|n| held(&format!("r{n}")))
        .collect();
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(reply, empty_reply());
    assert_eq!(fx.registry.hosts().len(), 1);
}

/// Asserts that a taint the heartbeat reports is acknowledged beside a listing of runs and
/// results, which adds no command and no acknowledgement, and that a heartbeat with no taint
/// is not acknowledged. It catches a reply that drops the taint acknowledgement when the
/// heartbeat lists runs.
#[test]
fn the_taint_acknowledgement_goes_alone_beside_a_listing() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat: Heartbeat = fx.heartbeat("a", epoch);
    beat.held = vec![held("r1")];
    beat.results = vec![result("r2", Outcome::Succeeded)];
    beat.taint = Some(Taint {
        reason: "residue".into(),
        run_id: None,
        check: None,
    });
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(reply.taint_ack);
    assert!(reply.commands.is_empty());
    assert!(reply.result_acks.is_empty());
    beat.taint = None;
    let next = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(!next.taint_ack);
}

/// Asserts that the runs a hello lists as held are never adopted: the welcome carries no
/// command, the host reads releasing, and the next heartbeat that still lists them is
/// answered with the empty reply and the host still reads releasing; once a heartbeat lists
/// none the host is no longer releasing. It catches a registry that adopts a run an agent
/// lists after a restart.
#[test]
fn the_runs_a_hello_lists_are_not_adopted_and_release_once_unlisted() {
    let fx = fixture(2);
    let mut said = hello("a");
    said.held = vec![held("r1"), held("r2")];
    let welcome = fx.hello(&said, &cred("t")).unwrap();
    assert!(fx.registry.hosts()[0].releasing);
    let mut beat = fx.heartbeat("a", welcome.epoch);
    beat.held = said.held.clone();
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(reply, empty_reply());
    assert!(fx.registry.hosts()[0].releasing);
    beat.held = Vec::new();
    let quiet = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(quiet.commands.is_empty());
    assert!(!fx.registry.hosts()[0].releasing);
}
