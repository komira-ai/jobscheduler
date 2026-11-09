//! The reply to a heartbeat for an agent of the previous release that still keys its work by
//! run: the registry queues no run, so it cancels every run the agent lists as held and
//! answers every result it reports as of an unknown lease. The job half of the reply is the
//! transport's and is left empty here.

mod support;

use js_agent_wire::{
    Ack, Command, Heartbeat, HeartbeatReply, HeldRun, HeldState, Outcome, RunResult, Taint, limits,
};
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

fn cancels(reply: &HeartbeatReply) -> Vec<String> {
    let cancel = |command: &Command| match command {
        Command::Cancel { run_id } => run_id.clone(),
        other => panic!("not a cancel: {other:?}"),
    };
    reply.commands.iter().map(cancel).collect()
}

/// Asserts that a heartbeat that lists held runs is answered with a cancel for each, in the
/// order listed, whatever their state, and nothing else: no start or stop, no exclusive
/// grant, no job acknowledgement, no retired acknowledgement and no log grant. It catches a
/// reply that leaves a run the registry does not know running on the agent.
#[test]
fn every_held_run_is_cancelled() {
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
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(cancels(&reply), ["r1", "r2", "r3"]);
    assert!(reply.result_acks.is_empty());
    assert!(!reply.taint_ack);
    assert_eq!(reply.exclusive, None);
    assert!(reply.job_acks.is_empty());
    assert!(reply.retired_acks.is_empty());
    assert_eq!(reply.log_grant, None);
}

/// Asserts that no more than the wire's cap of commands goes in one reply: a heartbeat that
/// lists one run more than `MAX_COMMANDS` is answered with a cancel for the first
/// `MAX_COMMANDS`, and the run past the cap is cancelled by the reply to the next heartbeat
/// that still lists it. It catches a reply the wire refuses.
#[test]
fn the_cancels_of_one_reply_are_capped_at_the_wire_limit() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    beat.held = (0..=limits::MAX_COMMANDS)
        .map(|n| held(&format!("r{n}")))
        .collect();
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    let sent = cancels(&reply);
    assert_eq!(sent.len(), limits::MAX_COMMANDS);
    assert_eq!(sent[0], "r0");
    assert_eq!(
        sent[limits::MAX_COMMANDS - 1],
        format!("r{}", limits::MAX_COMMANDS - 1)
    );
    beat.held = vec![held(&format!("r{}", limits::MAX_COMMANDS))];
    let next = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(cancels(&next), [format!("r{}", limits::MAX_COMMANDS)]);
}

/// Asserts that every result a heartbeat carries is acknowledged `UnknownLease`, whatever its
/// outcome and the lease it names, in the order sent, so the agent drops each result; and
/// that the reply carries no command for results alone. It catches a result left
/// unanswered, which the agent would send for ever.
#[test]
fn every_result_is_answered_unknown_lease() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    let mut beat = fx.heartbeat("a", epoch);
    let mut elsewhere = result("r3", Outcome::Killed);
    elsewhere.lease_id = "9-9".into();
    beat.results = vec![
        result("r1", Outcome::Succeeded),
        result("r2", Outcome::Failed),
        elsewhere,
    ];
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    let acks: Vec<(String, Ack)> = reply
        .result_acks
        .iter()
        .map(|ack| (ack.run_id.clone(), ack.ack))
        .collect();
    let unknown = |run: &str| (run.to_owned(), Ack::UnknownLease);
    assert_eq!(acks, [unknown("r1"), unknown("r2"), unknown("r3")]);
    assert!(reply.commands.is_empty());
}

/// Asserts that a taint the heartbeat reports is acknowledged beside the cancels and the
/// acknowledgements, and nothing of the reply is taken from the held list or the results: a
/// heartbeat with no taint is not acknowledged. It catches a reply that drops the taint
/// acknowledgement when it carries runs.
#[test]
fn the_taint_acknowledgement_goes_beside_the_cancels() {
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
    assert_eq!(cancels(&reply), ["r1"]);
    assert_eq!(reply.result_acks.len(), 1);
    beat.taint = None;
    let next = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(!next.taint_ack);
}

/// Asserts that the runs a hello lists as held are never adopted: the welcome carries no
/// command, the host reads releasing, and the next heartbeat that still lists them is
/// answered with a cancel for each; once a heartbeat lists none the host is no longer
/// releasing. It catches a registry that adopts a run an agent lists after a restart.
#[test]
fn the_runs_a_hello_lists_are_cancelled_by_the_next_heartbeat() {
    let fx = fixture(2);
    let mut said = hello("a");
    said.held = vec![held("r1"), held("r2")];
    let welcome = fx.hello(&said, &cred("t")).unwrap();
    assert!(fx.registry.hosts()[0].releasing);
    let mut beat = fx.heartbeat("a", welcome.epoch);
    beat.held = said.held.clone();
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(cancels(&reply), ["r1", "r2"]);
    assert!(fx.registry.hosts()[0].releasing);
    beat.held = Vec::new();
    let quiet = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(quiet.commands.is_empty());
    assert!(!fx.registry.hosts()[0].releasing);
}
