//! A runtime with an egress over the leases an earlier process left, with the port faked
//! (the stand-in `crun`, as in `egress`): the network a start records in the container's
//! labels, how a restarted runtime adopts or removes a run by what they record, when the port
//! settles, and what a dropped runtime does with the wiring it holds.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use js_agent::crun::{CrunError, CrunRuntime, EgressPort, Reason};
use js_agent::egress::EgressError;
use js_agent::lifecycle::{RunHandle, RunSpec, Runtime, label};
use js_types::{JobNetwork, PortMap};

use crate::egress::{impatient, nothing_left, open, wired};
use crate::egress_port::{Seen, port, port_logging_settle};
use crate::node::{NoImages, Node, PID};

/// Sets the label `key` of the sidecar of the lease `name` to `value`, or removes it for
/// `None`.
fn set_label(node: &Node, name: &str, key: &str, value: Option<&str>) {
    let path = node.sidecar(name);
    let text = std::fs::read_to_string(&path).expect("the sidecar");
    let mut json: serde_json::Value = serde_json::from_str(&text).expect("json");
    match value {
        Some(value) => json["labels"][key] = serde_json::json!(value),
        None => {
            json["labels"].as_object_mut().expect("labels").remove(key);
        }
    }
    std::fs::write(path, json.to_string()).expect("write the sidecar");
}

/// Starts the wired run `run_id` (a `*-wired` run: it stands until it is killed) on a
/// runtime that has `port`, with its init in its cgroup so that a restart can adopt it, and
/// drops the runtime: the run is left to the next one. The name of the lease.
fn left_running(node: &Node, port: Option<Arc<dyn EgressPort>>, spec: &RunSpec) -> String {
    let mut runtime = open(node, port, impatient());
    let id = runtime.create_and_start(spec).expect("started");
    node.place(&id.0, PID);
    drop(runtime);
    id.0
}

/// The run `run_id` as a job start makes it (the labels of a booking, so that a restart can
/// adopt it), admitted at the allow-list.
fn job_wired(node: &Node, run_id: &str) -> RunSpec {
    RunSpec {
        network: JobNetwork::AllowList,
        ..node.job(run_id, 600_000)
    }
}

/// A runtime on `node` over the leases an earlier one left, with `port` and its settle in
/// the record.
fn reopened(node: &Node, seen: &Arc<Seen>) -> CrunRuntime {
    open(node, port_logging_settle(seen), impatient())
}

/// Asserts the lease `name` was removed with the reason `want` and nothing of it is left.
fn removed_saying(node: &Node, runtime: &CrunRuntime, name: &str, want: &str) {
    let report = runtime.swept();
    assert!(report.adopted.is_empty(), "{report:?}");
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert_eq!(
        report.removed[0].reason,
        Reason::Inconsistent(want.to_owned())
    );
    let run_id = name.rsplit('-').next().expect("a run id");
    assert!(nothing_left(node, run_id), "{name} is left");
}

/// Asserts a start writes the network it was admitted at into the container's labels:
/// `jobscheduler.dev/network` is the level's name for an allow-list, an open and a none run,
/// and `jobscheduler.dev/network-ports` lists the declared ports as `host:container` in
/// order, only when there are some. Catches a restart that cannot tell a run's level, and a
/// ports label written for a run that declared none.
#[test]
fn a_start_records_its_network_level_and_ports_in_the_labels() {
    let node = Node::new("egress-labels");
    let seen = Seen::new(&node);
    let mut runtime = open(&node, port(&seen), impatient());
    let ports = vec![
        PortMap {
            host: 8080,
            container: 80,
        },
        PortMap {
            host: 9090,
            container: 90,
        },
    ];
    let specs = [
        ("r1-exit", JobNetwork::AllowList, ports.clone()),
        ("r2-exit", JobNetwork::Open, Vec::new()),
        ("r3-exit", JobNetwork::None, Vec::new()),
    ];
    let mut ids = Vec::new();
    for (run, network, ports) in specs {
        let spec = RunSpec {
            network,
            ports,
            ..node.spec(run)
        };
        ids.push(runtime.stage(&spec).expect("staged"));
    }
    let found = runtime.list_labelled().expect("lists");
    let of = |id: &js_agent::lifecycle::RunHandle| {
        let one = found.iter().find(|one| one.id == *id).expect("listed");
        let get = |key: &str| one.labels.get(key).cloned();
        (get(label::NETWORK), get(label::NETWORK_PORTS))
    };
    let some = |text: &str| Some(text.to_owned());
    assert_eq!(of(&ids[0]), (some("allow-list"), some("8080:80,9090:90")));
    assert_eq!(of(&ids[1]), (some("open"), None));
    assert_eq!(of(&ids[2]), (some("none"), None));
}

/// Asserts a wired run left by a runtime that was dropped is adopted by the next runtime
/// through the port: the port is asked to adopt each run with the lease's name, the pid
/// `crun state` names, and the level and ports the labels record (an allow-list run with
/// no ports, an open run with ports), then settles once, after the adoptions; and the
/// adopted run's removal releases its adopted wiring. The first runtime leaves each wiring
/// when it is dropped and releases none. Catches a drop that takes the wiring down, an
/// adoption that wires again, a settle before the adoptions (it would remove them), and an
/// adopted wiring that is never released.
#[test]
fn a_wired_run_is_adopted_with_its_recorded_network_then_the_port_settles_once() {
    let node = Node::new("egress-adopt");
    let seen = Seen::new(&node);
    let ports = vec![PortMap {
        host: 8080,
        container: 80,
    }];
    let first = job_wired(&node, "r1-wired");
    let second = RunSpec {
        network: JobNetwork::Open,
        ports: ports.clone(),
        ..node.job("r2-wired", 600_000)
    };
    let mut runtime = open(&node, port_logging_settle(&seen), impatient());
    let ids: Vec<_> = [&first, &second]
        .into_iter()
        .map(|spec| {
            let id = runtime.create_and_start(spec).expect("started");
            node.place(&id.0, PID);
            id
        })
        .collect();
    drop(runtime);
    let (one, two) = (&ids[0].0, &ids[1].0);
    assert_eq!(
        seen.taken(),
        [
            "settle".to_owned(),
            format!("attach {one} {PID}"),
            format!("attach {two} {PID}"),
            format!("leave {one}"),
            format!("leave {two}"),
        ]
    );

    let mut runtime = reopened(&node, &seen);
    assert_eq!(runtime.swept().adopted.len(), 2, "{:?}", runtime.swept());
    assert!(runtime.swept().removed.is_empty(), "{:?}", runtime.swept());
    assert_eq!(
        seen.taken(),
        [
            format!("adopt {one} allow-list "),
            format!("adopt {two} open 8080:80"),
            "settle".to_owned(),
        ]
    );
    let attached = seen.ports.lock().expect("ports").clone();
    assert_eq!(
        attached,
        [Vec::new(), ports.clone()],
        "what the first runtime attached"
    );
    let adopted = seen.adopted.lock().expect("adopted").clone();
    assert_eq!(
        adopted,
        [
            (JobNetwork::AllowList, Vec::new()),
            (JobNetwork::Open, ports)
        ]
    );
    runtime.remove(&ids[0]).expect("removed");
    assert_eq!(seen.taken(), [format!("release {one}")]);
    drop(runtime);
    assert_eq!(seen.taken(), [format!("leave {two}")]);
    assert_eq!(
        seen.settles.load(Ordering::SeqCst),
        2,
        "one settle for each open"
    );
}

/// Asserts a dropped runtime calls leave, never release, for the wired runs it still
/// holds, and nothing for the rest: a runtime with no runs calls nothing; one whose wired
/// run was removed (released) and whose other run is `none` calls nothing more. Catches a
/// drop that releases the wiring of a run that goes on, and a leave of a wiring already
/// released.
#[test]
fn a_dropped_runtime_leaves_only_the_wiring_it_still_holds() {
    let node = Node::new("egress-drop");
    let seen = Seen::new(&node);
    let runtime = open(&node, port_logging_settle(&seen), impatient());
    drop(runtime);
    assert_eq!(
        seen.taken(),
        ["settle"],
        "a runtime with no runs calls nothing"
    );

    let mut runtime = open(&node, port_logging_settle(&seen), impatient());
    runtime
        .create_and_start(&node.spec("r1-wired"))
        .expect("started");
    let id = runtime
        .create_and_start(&wired(&node, "r2-wired"))
        .expect("started");
    runtime.remove(&id).expect("removed");
    drop(runtime);
    assert_eq!(
        seen.taken(),
        [
            "settle".to_owned(),
            format!("attach {} {PID}", id.0),
            format!("release {}", id.0)
        ]
    );
}

/// Asserts a run that ended while no agent ran is adopted under an egress with no call to
/// adopt its wiring (it has none left), and the port settles. Catches an adoption of an
/// ended run that asks the port for a wiring that is gone.
#[test]
fn a_run_with_an_exit_record_is_adopted_without_adopting_its_wiring() {
    let node = Node::new("egress-adopt-ended");
    let seen = Seen::new(&node);
    let mut first = open(&node, port(&seen), impatient());
    let id = first
        .create_and_start(&job_wired(&node, "r1-sayexit"))
        .expect("started");
    first.wait_ended(&id);
    drop(first);
    seen.taken();

    let second = reopened(&node, &seen);
    assert_eq!(
        second.swept().adopted,
        ["r1-sayexit"],
        "{:?}",
        second.swept()
    );
    assert!(second.swept().removed.is_empty(), "{:?}", second.swept());
    assert_eq!(seen.taken(), ["settle"]);
}

/// Asserts a `none` run is adopted under an egress with no call to adopt (it has no wiring),
/// while the port settles. Catches a none-level run handed to the port, which has no
/// interface for it.
#[test]
fn a_none_run_is_adopted_under_an_egress_without_asking_the_port() {
    let node = Node::new("egress-adopt-none");
    let seen = Seen::new(&node);
    left_running(&node, None, &node.job("r1-wired", 600_000));
    let second = reopened(&node, &seen);
    assert_eq!(second.swept().adopted, ["r1-wired"], "{:?}", second.swept());
    assert_eq!(seen.taken(), ["settle"]);
}

/// Asserts a run whose sidecar records no network label is removed under an egress with the
/// reason that nothing records its wiring, and is adopted by a runtime with no egress.
/// Catches an adoption of a run whose wiring cannot be told, and a runtime without an egress
/// refusing the runs of an agent that never recorded the label.
#[test]
fn a_run_with_no_network_label_is_removed_under_an_egress_and_adopted_without_one() {
    let node = Node::new("egress-adopt-unlabelled");
    let seen = Seen::new(&node);
    let name = left_running(&node, None, &node.job("r1-wired", 600_000));
    set_label(&node, &name, label::NETWORK, None);
    let second = reopened(&node, &seen);
    let why =
        "its network: no jobscheduler.dev/network label records it, so its wiring cannot be told";
    removed_saying(&node, &second, &name, why);
    assert_eq!(seen.taken(), ["settle"]);

    let name = left_running(&node, None, &node.job("r2-wired", 600_000));
    set_label(&node, &name, label::NETWORK, None);
    let third = node.open_impatient();
    assert_eq!(third.swept().adopted, ["r2-wired"], "{:?}", third.swept());
}

/// Asserts a wired run is removed by a runtime with no egress, with the reason that it was
/// wired at its level and the ceiling is none now. Catches a run adopted with a network the
/// agent can no longer manage.
#[test]
fn a_wired_run_is_removed_by_a_runtime_with_no_egress() {
    let node = Node::new("egress-adopt-no-ceiling");
    let seen = Seen::new(&node);
    let name = left_running(&node, port(&seen), &job_wired(&node, "r1-wired"));
    let second = node.open_impatient();
    let why = "its network: it was wired at allow-list and this agent's ceiling is none";
    removed_saying(&node, &second, &name, why);
}

/// Asserts a port that refuses to adopt removes the run with the port's reason, after the
/// adoption was asked with the recorded level, and the port still settles. Catches a refused
/// adoption that keeps a run with no wiring.
#[test]
fn a_run_whose_wiring_the_port_refuses_is_removed_with_its_reason() {
    let node = Node::new("egress-adopt-denied");
    let seen = Seen::new(&node);
    let name = left_running(&node, port(&seen), &job_wired(&node, "r1-wired"));
    seen.taken();
    seen.adopt_fails.store(true, Ordering::SeqCst);
    let second = reopened(&node, &seen);
    let why = format!(
        "its network: egress: the wiring of run {name} cannot be taken over: its record is not wired"
    );
    removed_saying(&node, &second, &name, &why);
    assert_eq!(
        seen.taken(),
        [format!("adopt {name} allow-list "), "settle".to_owned()]
    );
}

/// Asserts a network label the agent does not write removes the run as inconsistent, naming
/// the label, with the port asked nothing: an unknown level, and a ports list that is
/// empty, has no colon, holds a zero or out-of-range port, or ends with a comma. Catches a
/// label that is read as none and leaves a wired run unwired.
#[test]
fn a_run_with_a_network_label_the_agent_does_not_write_is_removed() {
    let cases = [
        (label::NETWORK, "bogus"),
        (label::NETWORK_PORTS, "x"),
        (label::NETWORK_PORTS, ""),
        (label::NETWORK_PORTS, "80"),
        (label::NETWORK_PORTS, "0:80"),
        (label::NETWORK_PORTS, "80:0"),
        (label::NETWORK_PORTS, "70000:1"),
        (label::NETWORK_PORTS, "80:80,"),
    ];
    for (index, (key, bad)) in cases.into_iter().enumerate() {
        let node = Node::new(&format!("egress-adopt-label-{index}"));
        let seen = Seen::new(&node);
        let name = left_running(&node, port(&seen), &job_wired(&node, "r1-wired"));
        seen.taken();
        set_label(&node, &name, key, Some(bad));
        let second = reopened(&node, &seen);
        let why = format!("the label {key} is not what the agent writes");
        removed_saying(&node, &second, &name, &why);
        assert_eq!(seen.taken(), ["settle"], "{key}={bad}");
    }
}

/// Asserts a wired run whose container `crun state` reads with no pid, and one whose state
/// `crun` cannot read, are both adopted through the port like any wired run, asked with the
/// recorded level and ports, and neither is marked lost. Catches a run adopted from the
/// kernel's view of its container instead of its record, and a run whose state could not be
/// read marked lost or removed (its result dropped).
#[test]
fn a_wired_run_whose_state_is_not_read_is_adopted_through_the_port_like_any_other() {
    let node = Node::new("egress-adopt-pid");
    let seen = Seen::new(&node);
    let ending = left_running(&node, None, &node.job("r1-badpid", 600_000));
    let deaf = left_running(&node, None, &node.job("r2-deaf", 600_000));
    for name in [&ending, &deaf] {
        set_label(&node, name, label::NETWORK, Some("allow-list"));
    }
    let second = reopened(&node, &seen);
    let report = second.swept();
    assert_eq!(report.adopted, ["r1-badpid", "r2-deaf"], "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(
        seen.taken(),
        [
            format!("adopt {ending} allow-list "),
            format!("adopt {deaf} allow-list "),
            "settle".to_owned(),
        ]
    );
    assert_eq!(second.host_port_lost(&RunHandle(ending)), None);
    assert_eq!(second.host_port_lost(&RunHandle(deaf)), None);
}

/// Asserts a port whose settle fails fails the open with the port's error. Catches an open that carries on with leftovers of an earlier process.
#[test]
fn a_settle_that_fails_fails_the_open() {
    let node = Node::new("egress-settle-fails");
    let seen = Seen::new(&node);
    seen.settle_fails.store(true, Ordering::SeqCst);
    let mut settings = node.settings("/root");
    settings.egress = port_logging_settle(&seen);
    let why = CrunRuntime::open_with(settings, Arc::new(NoImages), impatient())
        .expect_err("settle failed");
    assert!(
        matches!(why, CrunError::Egress(EgressError::Rules)),
        "{why:?}"
    );
    assert_eq!(why.to_string(), EgressError::Rules.to_string());
    assert_eq!(seen.taken(), ["settle"]);
}
