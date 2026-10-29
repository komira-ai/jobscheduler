//! What the agent does with a whole-node lease when it runs jobs (it ignores the lease:
//! the job worker does not implement `Quiesce`), and what it does with a `Start` when it
//! runs none (it ignores that, and counts it). Both over the whole agent: the flags, the
//! scripted server and a machine of the test's own.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use js_agent::{Handle, Sleeper, Wake};
use serde_json::{Value, json};

use super::drain::container_flags;
use super::fake::FakeMachine;
use super::start_answer;
use crate::bounds::WakeBudget;
use crate::support::{
    Jobs, Lowest, Scratch, Script, ScriptConnector, beat_ok, code, flags, linux_host, reply,
    run_on, welcome,
};

/// A sleeper that settles the worker at every wait and asks for the stop at the
/// `stop_at`th (1 is the first), and at every wait after it.
struct Settle<'a> {
    seen: &'a RefCell<Option<Handle>>,
    calls: usize,
    stop_at: usize,
    budget: WakeBudget,
}

impl Sleeper for Settle<'_> {
    fn sleep(&mut self, _duration: Duration) -> Wake {
        self.budget.spend();
        if let Some(handle) = self.seen.borrow().as_ref() {
            handle.settle();
        }
        self.calls += 1;
        match self.calls >= self.stop_at {
            true => Wake::Stop,
            false => Wake::Elapsed,
        }
    }
}

/// A reply that carries the commands of `base` and names the whole-node lease `lease`.
fn naming(base: Value, lease: &str) -> crate::support::Reply {
    let mut body = base;
    body["exclusive"] = json!(lease);
    reply(200, body)
}

/// Asserts what an agent that runs jobs does when the server names a whole-node lease: the
/// lease is ignored, as the library documentation says. The start the same reply carries
/// is refused as `failed_to_start` (`no-capacity`) because the worker's lifecycle never
/// finds the host ready for a lease nothing prepares it for, and the start in a reply that
/// names no lease runs; every heartbeat stays in the phase `serving` and says neither
/// `exclusive_ready` nor `exclusive_refused`, the log has no line about preparing the host,
/// and the drain that follows a stop request is the ordinary one. It catches a session
/// that half follows the lease (a phase `preparing` that nothing ends, or a ready report
/// for a host that was not made quiet) and a run started on a host the server holds whole.
#[test]
fn an_agent_with_jobs_ignores_a_whole_node_lease() {
    let dir = Scratch::new("jobs-exclusive");
    let owned = container_flags(&dir);
    let extra: Vec<&str> = owned.iter().map(String::as_str).collect();
    let args = flags(&extra);
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let replies = vec![
        welcome(1),
        naming(start_answer(&["r1"]), "lease-1"),
        naming(json!({ "commands": [] }), "lease-1"),
        reply(200, start_answer(&["r2"])),
        beat_ok(),
        beat_ok(),
    ];
    let (script, sent) = Script::new(replies);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let mut sleeper = Settle {
        seen: &seen,
        calls: 0,
        stop_at: 5,
        budget: WakeBudget::new(),
    };
    let jobs: Jobs<'_> = (machine, Some(&hook));
    let ran = run_on(
        &args,
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), code(0), "{}", ran.err);

    assert_eq!(
        world.created(),
        ["r2"],
        "no container for the refused start"
    );
    let sent = sent.borrow();
    let beats: Vec<&Value> = sent.iter().skip(1).map(|(_, body)| body).collect();
    let phases: Vec<&str> = beats.iter().map(|b| b["phase"].as_str().unwrap()).collect();
    assert_eq!(
        phases,
        ["serving", "serving", "serving", "serving", "draining"]
    );
    for beat in &beats {
        assert!(beat.get("exclusive_ready").is_none(), "{beat}");
        assert!(beat.get("exclusive_refused").is_none(), "{beat}");
    }
    // The lifecycle still holds the host for the lease the reply names: the start in that
    // reply is refused (and reported), nothing is created for it, and the start in the
    // reply that no longer names the lease runs.
    assert_eq!(
        beats[1]["results"],
        json!([{
            "run_id": "r1",
            "lease_id": "lease-r1",
            "outcome": "failed_to_start",
            "detail": "no-capacity",
        }])
    );
    assert_eq!(
        beats[3]["held"],
        json!([
            { "run_id": "r1", "state": "exited_unacked" },
            { "run_id": "r2", "state": "running" },
        ])
    );
    assert!(!ran.err.contains("preparing"), "{}", ran.err);
    assert!(!ran.err.contains("refusing"), "{}", ran.err);
}

/// Asserts the agent without `--container-bin` is the agent without jobs: a `Start` the
/// server sends is counted in the log as an ignored command, no worker is built (the
/// hook is never told of one and the machine is never asked for a container), and the
/// heartbeats list nothing. It catches a job half that starts without its flag.
#[test]
fn an_agent_without_container_bin_ignores_a_start_and_builds_no_worker() {
    let machine = Arc::new(FakeMachine::new());
    let world = machine.world.clone();
    let replies = vec![welcome(1), reply(200, start_answer(&["r1"])), beat_ok()];
    let (script, sent) = Script::new(replies);
    let seen: RefCell<Option<Handle>> = RefCell::new(None);
    let hook = |handle: Handle| *seen.borrow_mut() = Some(handle);
    let mut sleeper = Settle {
        seen: &seen,
        calls: 0,
        stop_at: 3,
        budget: WakeBudget::new(),
    };
    let jobs: Jobs<'_> = (machine, Some(&hook));
    let ran = run_on(
        &flags(&[]),
        &linux_host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
        jobs,
    );
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    assert!(
        ran.err
            .contains("ignoring 1 command(s): this agent runs no jobs"),
        "{}",
        ran.err
    );
    assert!(seen.borrow().is_none(), "no worker was built");
    assert!(world.created().is_empty());
    assert!(world.log().is_empty());
    let sent = sent.borrow();
    assert!(
        sent.iter()
            .skip(1)
            .all(|(_, body)| body["held"] == json!([]))
    );
}
