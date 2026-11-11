//! A job start's host ports through the lifecycle: held to the runtime's allowed ports at
//! admission (`input-not-allowed`, before anything is made), carried to the runtime as
//! given, and, for a run adopted after a restart that could not be offered them again,
//! stopped with `input_failure: host-port-lost`, kept across a further restart.

use std::fs;

use js_agent::lifecycle::Event;
use js_agent_wire::{HostPort, Outcome, PortProtocol, PortRange, refusal};
use serde_json::Value;

use super::per_job::{job_start, listing, start};
use super::{Harness, names};

fn host_port(host_port: u16, container_port: Option<u16>) -> HostPort {
    HostPort {
        host_port,
        container_port,
        protocol: PortProtocol::Tcp,
    }
}

fn ranges(entries: &[&str]) -> Vec<PortRange> {
    let parsed = entries.iter().map(|entry| PortRange::parse(entry));
    parsed.collect::<Option<_>>().expect("ranges")
}

/// Proves a job start that names a host port the runtime does not allow is refused
/// `input-not-allowed`, naming the port and the allowed ones, with nothing made and a
/// `failed_to_start` result whose detail is the code; that a runtime that allows none (the
/// default of every runtime but `crun`) refuses any; and that a start whose host ports are
/// all allowed is created with them as given, the job's own ports not bounded. Catches a
/// start run with its host ports dropped, an unlisted host port admitted, and the job port
/// held to the operator's list.
#[test]
fn a_host_port_the_runtime_does_not_allow_is_refused_before_anything_is_made() {
    let mut h = Harness::new("host-ports-admit");
    let mut asked = start("j1");
    asked.host_ports = vec![host_port(5432, None)];
    let events = h.apply(vec![job_start(asked)]);
    assert_eq!(
        events,
        [Event::Refused {
            run_id: "j1".to_owned(),
            code: refusal::INPUT_NOT_ALLOWED,
            why: "host port 5432 is not one this agent offers (--host-port: none)".to_owned(),
        }]
    );
    h.fake.set_host_ports(&ranges(&["5432", "9000-9010"]));
    let mut outside = start("j2");
    outside.host_ports = vec![host_port(9000, None), host_port(9011, Some(19011))];
    assert_eq!(
        h.apply(vec![job_start(outside)]),
        [Event::Refused {
            run_id: "j2".to_owned(),
            code: refusal::INPUT_NOT_ALLOWED,
            why: "host port 9011 is not one this agent offers (--host-port: 5432,9000-9010)"
                .to_owned(),
        }]
    );
    assert!(h.fake.created().is_empty(), "a refused start was created");
    for job_id in ["j1", "j2"] {
        let result = listing(&h, job_id).result.expect("a result");
        assert_eq!(result.outcome, Outcome::FailedToStart, "{job_id}");
        assert_eq!(result.detail.as_deref(), Some(refusal::INPUT_NOT_ALLOWED));
        assert_eq!(result.input_failure, None, "{job_id}");
    }
    let mut inside = start("j3");
    let given = vec![host_port(5432, Some(15432)), host_port(9010, None)];
    inside.host_ports = given.clone();
    assert_eq!(names(&h.apply(vec![job_start(inside)])), ["started j3"]);
    let created = h.fake.created();
    assert_eq!(
        created[0].host_ports, given,
        "the host ports reach the runtime"
    );
}

/// Proves that a job the runtime adopted after a restart but could not offer its host ports
/// again is noted `host-port-lost` once and stopped at the next step with the stop grace,
/// and ends `killed` with `input_failure: host-port-lost`; that the result keeps it after a
/// further restart, read back from the state root, and that a file without it (an earlier
/// release's) reads back with none; and that a job adopted beside it whose ports were
/// offered runs on. Catches a lost host port that leaves the job running without
/// it, a result that says the job's own failure, and an input failure lost on a restart.
#[test]
fn an_adopted_job_that_lost_its_host_port_is_stopped_and_says_so() {
    let mut h = Harness::new("host-ports-lost");
    h.fake.set_host_ports(&ranges(&["5432"]));
    for job_id in ["j1", "j2"] {
        let mut asked = start(job_id);
        asked.host_ports = vec![host_port(5432, None)];
        assert_eq!(
            names(&h.apply(vec![job_start(asked)])),
            [format!("started {job_id}")]
        );
    }
    h.fake
        .lose_host_ports("j1", "port 5432 is taken in the job's namespace");
    let events = names(&h.restart().expect("restarted"));
    assert!(
        events.contains(&"finding j1 host-port-lost".to_owned()),
        "{events:?}"
    );
    assert!(
        !events.contains(&"finding j2 host-port-lost".to_owned()),
        "{events:?}"
    );
    let events = names(&h.life.step());
    assert!(events.contains(&"result j1".to_owned()), "{events:?}");
    assert_eq!(h.fake.stops(), [("c1".to_owned(), super::GRACE)]);
    let lost = listing(&h, "j1").result.expect("a result");
    assert_eq!((lost.outcome, lost.exit_code), (Outcome::Killed, None));
    assert_eq!(lost.input_failure.as_deref(), Some(refusal::HOST_PORT_LOST));
    assert_eq!(lost.detail, None);
    assert_eq!(listing(&h, "j2").result, None, "the other job runs on");

    let file = h.dir.path("state").join("held").join("j1.json");
    let mut stored: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["input_failure"], refusal::HOST_PORT_LOST);

    h.restart().expect("restarted again");
    let kept = listing(&h, "j1").result.expect("the result is held");
    assert_eq!(kept.input_failure.as_deref(), Some(refusal::HOST_PORT_LOST));
    assert_eq!(kept.outcome, Outcome::Killed);

    // The file of an earlier release has no input failure, and is read back with none.
    stored.as_object_mut().unwrap().remove("input_failure");
    fs::write(&file, stored.to_string()).unwrap();
    h.restart().expect("restarted on the earlier file");
    let earlier = listing(&h, "j1").result.expect("the result is held");
    assert_eq!(
        (earlier.outcome, earlier.input_failure),
        (Outcome::Killed, None)
    );
}

/// Proves that an adopted job that lost its host port and exited by itself before the stop
/// reached it keeps its own outcome and exit code, and its result says `host-port-lost` when
/// it failed (as a job whose port is gone does) and nothing when it succeeded, the held file
/// naming none then. Catches a host's failure reported as the job's own, and an input
/// failure laid on a job that succeeded.
#[test]
fn a_job_that_ended_before_the_stop_keeps_its_own_result() {
    let mut h = Harness::new("host-ports-own-end");
    h.fake.set_host_ports(&ranges(&["5432"]));
    for job_id in ["j1", "j2"] {
        let mut asked = start(job_id);
        asked.host_ports = vec![host_port(5432, None)];
        h.apply(vec![job_start(asked)]);
        h.fake
            .lose_host_ports(job_id, "port 5432 is taken in the job's namespace");
    }
    h.fake.exit("j1", 0);
    h.fake.exit("j2", 3);
    h.restart().expect("restarted");
    h.life.step();
    assert!(h.fake.stops().is_empty(), "nothing was left to stop");
    let failed = listing(&h, "j2").result.expect("a result");
    assert_eq!(
        (failed.outcome, failed.exit_code),
        (Outcome::Failed, Some(3))
    );
    assert_eq!(
        failed.input_failure.as_deref(),
        Some(refusal::HOST_PORT_LOST)
    );
    let own = listing(&h, "j1").result.expect("a result");
    assert_eq!((own.outcome, own.exit_code), (Outcome::Succeeded, Some(0)));
    assert_eq!(own.input_failure, None);
    let file = h.dir.path("state").join("held").join("j1.json");
    let stored: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert!(stored.get("input_failure").is_none(), "{stored}");
}
