//! An agent of release `v0.1.208` against this registry. That release queued runs; this one
//! does not, so an agent that still keys its work by run says hello and heartbeats with held
//! runs and results, and is answered with a cancel for each held run and an unknown-lease
//! acknowledgement for each result. The agent's bodies are the files that release's code
//! wrote (`js-agent-wire/tests/fixtures/v0.1.208`), so they cannot follow a change to the
//! wire types.

mod support;

use js_agent_wire::{Ack, Command, Heartbeat, HeartbeatReply, HeldState, Hello, ResultAck};
use js_registry::PoolConfig;
use js_types::{ProviderId, Resources, Shape};
use support::*;

const HELLO_PER_JOB: &str =
    include_str!("../../js-agent-wire/tests/fixtures/v0.1.208/hello_per_job.json");
const HELLO_NATIVE: &str =
    include_str!("../../js-agent-wire/tests/fixtures/v0.1.208/hello_native.json");
const HEARTBEAT: &str = include_str!("../../js-agent-wire/tests/fixtures/v0.1.208/heartbeat.json");

/// The pool `builders` of the fixtures' hosts: four cores, 8 GiB and 100 GiB of disk (the
/// container agent's scratch default must fit it) of `os` on `arch`.
fn pool(os: &str, arch: &str) -> PoolConfig {
    let resources = Resources::new(4000, 8 << 30, 0, None)
        .unwrap()
        .with_disk_bytes(100 << 30);
    let shape = Shape::new(resources, map(&[("os", os), ("arch", arch)]));
    PoolConfig::new(ProviderId::new("builders").unwrap(), class(), shape, 2)
}

fn hello_of(text: &str) -> Hello {
    serde_json::from_str(text).unwrap()
}

/// The v0.1.208 heartbeat, carrying the epoch and session of the welcome the registry gave
/// the host: those two the registry issues, the rest is the file's.
fn heartbeat_of(welcome: &js_agent_wire::Welcome) -> Heartbeat {
    let mut beat: Heartbeat = serde_json::from_str(HEARTBEAT).unwrap();
    beat.epoch = welcome.epoch;
    beat.session = welcome.session.clone();
    beat
}

/// Asserts that the v0.1.208 heartbeat, fed to the registry after the v0.1.208 container
/// agent's hello, is answered with a `Cancel` for each of its two held runs in the order
/// held (one still running, one exited and unacknowledged), an `UnknownLease` acknowledgement
/// for each of its two results (the lease ids it names are not known), and nothing else: no
/// start, no stop, no job command or acknowledgement, no taint acknowledgement. It also
/// asserts that the reply survives the wire (encoded and decoded it is equal and passes
/// the reply's checks). It catches a registry that leaves a run of the old release
/// running on its host, or leaves a result unanswered for the agent to send for ever.
#[test]
fn the_v0_1_208_heartbeat_is_answered_with_a_cancel_per_held_run_and_unknown_lease_per_result() {
    let fx = fixture_of(pool("linux", "x86_64"));
    let welcome = fx.hello(&hello_of(HELLO_PER_JOB), &cred("t")).unwrap();
    let beat = heartbeat_of(&welcome);
    assert_eq!(beat.held.len(), 2);
    assert_eq!(beat.results.len(), 2);

    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(
        reply.commands,
        [
            Command::Cancel {
                run_id: "run-1".into()
            },
            Command::Cancel {
                run_id: "run-2".into()
            },
        ]
    );
    assert_eq!(
        reply.result_acks,
        [
            ResultAck {
                run_id: "run-2".into(),
                ack: Ack::UnknownLease
            },
            ResultAck {
                run_id: "run-3".into(),
                ack: Ack::UnknownLease
            },
        ]
    );
    assert!(!reply.taint_ack);
    assert_eq!(reply.exclusive, None);
    assert!(reply.job_acks.is_empty());
    assert!(reply.retired_acks.is_empty());
    assert_eq!(reply.log_grant, None);

    let wire = serde_json::to_string(&reply).unwrap();
    let back: HeartbeatReply = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, reply);
    assert!(back.validate().refused.is_empty());
}

/// Asserts that the same v0.1.208 heartbeat is answered the same way again: a registry that
/// has answered it keeps no run or result of it, so the agent's next heartbeat, still
/// listing the runs because it has not yet dropped them, gets the same cancels and
/// acknowledgements, and the host stays one the registry hears and lists. It catches a
/// registry that answers only the first heartbeat, or that loses the host once it has
/// cancelled its runs.
#[test]
fn the_v0_1_208_heartbeat_is_answered_the_same_when_it_is_repeated() {
    let fx = fixture_of(pool("linux", "x86_64"));
    let welcome = fx.hello(&hello_of(HELLO_PER_JOB), &cred("t")).unwrap();
    let beat = heartbeat_of(&welcome);
    let first = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    let second = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(second, first);
    assert_eq!(second.commands.len(), 2);
    assert_eq!(fx.registry.hosts().len(), 1);
    assert_eq!(fx.registry.hosts()[0].id, host("builder-01"));
}

/// Asserts that a v0.1.208 native agent, which reports no `per-job` and lists the run it
/// still holds in its hello, is welcomed without a command, is listed as releasing (the
/// run is not adopted), and is answered on its next heartbeat that lists the run with a
/// `Cancel` for it; once it lists nothing the host no longer releases. It catches a
/// registry that adopts the run of a restarted agent of the old release, or refuses its hello.
#[test]
fn a_v0_1_208_native_hello_with_a_held_run_is_released_by_the_next_heartbeat() {
    let fx = fixture_of(pool("macos", "aarch64"));
    let said = hello_of(HELLO_NATIVE);
    assert_eq!(said.held.len(), 1);
    assert_eq!(said.held[0].state, HeldState::Running);
    let welcome = fx.hello(&said, &cred("t")).unwrap();
    assert!(fx.registry.hosts()[0].releasing);

    let mut beat = fx.heartbeat("mac-01", welcome.epoch);
    beat.held = said.held.clone();
    let reply = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert_eq!(
        reply.commands,
        [Command::Cancel {
            run_id: "run-1".into()
        }]
    );
    assert!(reply.result_acks.is_empty());
    assert!(fx.registry.hosts()[0].releasing);

    beat.held = Vec::new();
    let quiet = fx.registry.heartbeat(&beat, &cred("t")).unwrap();
    assert!(quiet.commands.is_empty());
    assert!(!fx.registry.hosts()[0].releasing);
}
