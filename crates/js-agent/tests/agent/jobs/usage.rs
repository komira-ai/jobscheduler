//! The usage of the leases through the job half: the worker samples the cgroup files on its
//! own thread, once per heartbeat interval, and the session carries what it measured. The
//! container runtime and the cgroup files are in memory and the clock is moved by the test;
//! every wait on the worker is for an event (an order done), never for a time.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use js_agent::usage::Outbox;
use js_agent::{Delivery, Drain, HEARTBEAT_PATH, Session};
use js_agent_wire::{Command, HeartbeatReply};
use serde_json::{Value, json};

use super::fake::FakeMachine;
use super::{start_with_usage, starts};
use crate::lifecycle::job;
use crate::support::{Reply, Scratch, Script, Sent, beat_ok, hello, lowest, reply, welcome_with};

const SLICE: &str = "/sys/fs/cgroup/jobs.slice";
const MEMORY: u64 = 100 << 20;

/// The drain the sessions of these tests carry; none of them drains.
const DRAIN: Drain = Drain {
    wait: Duration::from_secs(10),
    settle: Duration::from_secs(30),
};

/// The node's and `r1`'s cgroup files after `k` windows of 30 seconds in which each used one
/// whole core.
fn counters(machine: &FakeMachine, k: u64) {
    let usage = format!("usage_usec {}\n", k * 30_000_000);
    let dirs = [SLICE.to_owned(), format!("{SLICE}/docker-r1.scope")];
    for dir in dirs {
        machine.files.set(&format!("{dir}/cpu.stat"), &usage);
        machine
            .files
            .set(&format!("{dir}/memory.current"), &format!("{MEMORY}\n"));
        machine.files.set(
            &format!("{dir}/memory.stat"),
            "inactive_file 0\nactive_file 0\n",
        );
    }
    machine
        .files
        .set(&format!("{SLICE}/cpu.max"), "max 100000\n");
}

/// The answer that starts `r1` with a wall-time limit of an hour, so that it is still
/// running when the test is over.
fn long_start() -> Value {
    serde_json::to_value(long_reply()).unwrap()
}

/// The same answer as a value of the wire type.
fn long_reply() -> HeartbeatReply {
    let mut spec = job("r1");
    spec.limits.wall_ms = 3_600_000;
    HeartbeatReply {
        commands: vec![Command::Start(Box::new(spec))],
        ..HeartbeatReply::default()
    }
}

/// The load of each heartbeat sent so far, `Null` for a beat without one.
fn loads(sent: &Sent) -> Vec<Value> {
    let sent = sent.borrow();
    let beats = sent.iter().filter(|(path, _)| path == HEARTBEAT_PATH);
    beats.map(|(_, body)| body["load"].clone()).collect()
}

/// A minute of `lease-r1` at one core, the memory these tests use.
fn minute(index: u16) -> Value {
    json!({
        "lease_id": "lease-r1",
        "minute": index,
        "cpu_mean_millis": 1000,
        "cpu_peak_window_millis": 1000,
        "ws_max_bytes": MEMORY,
    })
}

/// Asserts the whole path with a worker and a session: the beats carry the load the worker
/// sampled, with the node's counters, the clock of the lease and, from the second sample
/// on, the lease as covered; `seq` rises by one for each sample; a beat that fails to send
/// gives its minute back, the next beat carries it again, and the one after that does not
/// (it carries only the minute closed since). It catches a sample that is never published,
/// a sequence that skips, minutes lost with a failed send and minutes sent for ever.
#[test]
fn a_failed_send_requeues_and_the_next_beat_carries_the_minute_once() {
    let dir = Scratch::new("jobs-usage-requeue");
    let machine = FakeMachine::new();
    machine.world.set_cgroup_base(SLICE);
    counters(&machine, 0);
    let outbox = Arc::new(Outbox::new());
    let worker = start_with_usage(&dir, &machine, &outbox, Some(SLICE));
    let down: Reply = Err("the server is down".to_owned());
    let replies = vec![
        welcome_with(1, 30_000, 90_000),
        reply(200, long_start()),
        beat_ok(),
        beat_ok(),
        down,
        beat_ok(),
        beat_ok(),
    ];
    let (mut script, sent) = Script::new(replies);
    let mut session = Session::new(hello("h1"), &mut script, lowest())
        .with_jobs(&worker, DRAIN)
        .with_usage(outbox.clone());
    let settle = || assert!(worker.handle().settle());

    session.tick();
    assert_eq!(outbox.window_usec(), 30_000_000);
    session.tick();
    settle();
    session.tick();
    for k in 1..=4 {
        machine.clock.advance(30_000);
        counters(&machine, k);
        settle();
        session.tick();
    }

    let loads = loads(&sent);
    assert_eq!(loads.len(), 6);
    assert_eq!(loads[0], Value::Null, "nothing was sampled yet");
    assert_eq!(
        loads[1],
        json!({
            "seq": 1,
            "mono_usec": 0,
            "cpu_usage_usec": 0,
            "working_set_bytes": MEMORY,
            "active_file_bytes": 0,
            "actions_cpu_limited": false,
            "clocks": [{ "lease_id": "lease-r1", "started_mono_usec": 0 }],
        })
    );
    assert_eq!(loads[2]["seq"], 2);
    assert_eq!(loads[2]["mono_usec"], 30_000_000);
    assert_eq!(loads[2]["cpu_usage_usec"], 30_000_000);
    assert_eq!(loads[2]["covered"], json!(["lease-r1"]));
    assert_eq!(loads[2].get("minutes"), None);
    let carried: Vec<(Value, Value, Value)> = loads[3..]
        .iter()
        .map(|load| {
            (
                load["seq"].clone(),
                load["mono_usec"].clone(),
                load["minutes"].clone(),
            )
        })
        .collect();
    assert_eq!(
        carried,
        [
            (json!(3), json!(60_000_000), json!([minute(0)])),
            (json!(4), json!(90_000_000), json!([minute(0)])),
            (json!(5), json!(120_000_000), json!([minute(1)])),
        ]
    );
}

/// Asserts that the worker samples only when the heartbeat interval has passed on its
/// clock (one millisecond short publishes nothing, the full window publishes a reading) and
/// that every cgroup file is read on the worker's thread, never on the one that heartbeats.
/// It catches a worker that samples at every step, and a session that reads cgroup files
/// itself (a slow file system would then stop the heartbeats).
#[test]
fn the_worker_samples_once_per_window_on_its_own_thread() {
    let dir = Scratch::new("jobs-usage-window");
    let machine = FakeMachine::new();
    machine.world.set_cgroup_base(SLICE);
    counters(&machine, 0);
    let outbox = Arc::new(Outbox::new());
    let worker = start_with_usage(&dir, &machine, &outbox, Some(SLICE));
    assert!(worker.handle().settle());
    assert!(outbox.take().is_none(), "no window yet: no sample");

    outbox.set_window(Duration::from_secs(5));
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(outbox.take().map(|load| load.seq), Some(1));
    assert!(worker.handle().settle());
    assert!(outbox.take().is_none(), "the window has not passed");
    machine.clock.advance(4_999);
    assert!(worker.handle().settle());
    assert!(outbox.take().is_none(), "one millisecond short");
    machine.clock.advance(1);
    assert!(worker.handle().settle());
    assert_eq!(outbox.take().map(|load| load.seq), Some(2));

    let readers = machine.files.readers();
    assert!(!readers.is_empty());
    assert!(
        readers
            .iter()
            .all(|reader| *reader != thread::current().id())
    );
}

/// Asserts that without an actions control group nothing is sampled and the worker says why
/// once: no file is read, the outbox stays empty, and the line names the flag. It catches a
/// load made of files nobody pointed the agent at, and a silent absence.
#[test]
fn without_an_actions_cgroup_nothing_is_sampled_and_the_worker_says_why() {
    let dir = Scratch::new("jobs-usage-none");
    let machine = FakeMachine::new();
    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(5));
    let worker = start_with_usage(&dir, &machine, &outbox, None);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert!(outbox.take().is_none());
    assert!(machine.files.readers().is_empty());
    let lines = worker.take_lines();
    let said = "usage: --actions-cgroup is not given: the heartbeat carries no load";
    assert_eq!(
        lines.iter().filter(|line| line.as_str() == said).count(),
        1,
        "{lines:?}"
    );
}

/// Asserts that a lease whose control group the runtime could not give at its start is
/// looked up again at the following windows: the worker says once that it is not known,
/// does not say it again, and once the runtime can say, the lease is sampled (its baseline
/// at that window, covered at the next). It catches a lease that stays unsampled for its
/// whole life after one failed lookup, and a line in every window while it is missing.
#[test]
fn a_missing_control_group_is_looked_up_again_at_the_next_windows() {
    let dir = Scratch::new("jobs-usage-late-cgroup");
    let machine = FakeMachine::new();
    counters(&machine, 0);
    let outbox = Arc::new(Outbox::new());
    outbox.set_window(Duration::from_secs(30));
    let worker = start_with_usage(&dir, &machine, &outbox, Some(SLICE));
    let said = |worker: &js_agent::Worker| {
        let lines = worker.take_lines();
        lines
            .iter()
            .filter(|line| line.contains("control group"))
            .count()
    };

    assert_eq!(worker.try_deliver(long_reply()), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(said(&worker), 1, "told once that it is not known");
    let first = outbox.take().unwrap();
    assert_eq!(first.seq, 1);
    assert_eq!(first.clocks.len(), 1, "the lease is named, not sampled");

    machine.clock.advance(30_000);
    assert!(worker.handle().settle());
    assert_eq!(said(&worker), 0, "not told again while it is still missing");
    assert!(outbox.take().is_some());

    machine.world.set_cgroup_base(SLICE);
    for k in 1..=2 {
        machine.clock.advance(30_000);
        counters(&machine, k);
        assert!(worker.handle().settle());
        let load = outbox.take().unwrap();
        let covered = load.covered.len();
        assert_eq!(covered, usize::from(k == 2), "window {k}");
    }
    assert_eq!(said(&worker), 0, "nothing more once it is found");
}
