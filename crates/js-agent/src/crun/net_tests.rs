//! The network a run is asked for: the labels that record it in a sidecar, and what a run's
//! network takes over from an earlier process of the agent.

use std::collections::BTreeMap;

use js_agent_wire::NetworkCeiling;
use js_types::{JobNetwork, PortMap};

use super::net::{EgressPort, Net, Wired, network_labels, recorded_network};
use crate::egress::EgressError;

/// Asserts the labels a start writes for a network are read back as the same level and
/// ports (the level's name alone when no port is declared, `host:container` pairs joined by
/// commas otherwise), a sidecar with no network label records no network, and a level that
/// is not one, or a ports list that is empty, has an entry with no colon, a zero or too large
/// port, or a trailing comma, is refused naming the label. Catches a restart that reads a
/// label differently from how it was written, and a damaged label read as no network.
#[test]
fn the_network_labels_are_read_back_as_they_were_written() {
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
    for (level, ports) in [
        (JobNetwork::AllowList, ports.clone()),
        (JobNetwork::Open, Vec::new()),
        (JobNetwork::None, ports),
    ] {
        let labels: BTreeMap<String, String> = network_labels(level, &ports).into_iter().collect();
        assert_eq!(
            labels.len(),
            1 + usize::from(!ports.is_empty()),
            "{labels:?}"
        );
        assert_eq!(recorded_network(&labels), Ok(Some((level, ports))));
    }
    let written: BTreeMap<String, String> = network_labels(
        JobNetwork::Open,
        &[PortMap {
            host: 8080,
            container: 80,
        }],
    )
    .into_iter()
    .collect();
    assert_eq!(written["jobscheduler.dev/network"], "open");
    assert_eq!(written["jobscheduler.dev/network-ports"], "8080:80");

    assert_eq!(recorded_network(&BTreeMap::new()), Ok(None));
    let level = "jobscheduler.dev/network";
    let ports = "jobscheduler.dev/network-ports";
    let unread = |key: &str| Err(format!("the label {key} is not what the agent writes"));
    let labels = BTreeMap::from([(level.to_owned(), "bogus".to_owned())]);
    assert_eq!(recorded_network(&labels), unread(level));
    for bad in ["", "80", "x:80", "0:80", "80:0", "70000:80", "80:80,"] {
        let labels = BTreeMap::from([
            (level.to_owned(), "open".to_owned()),
            (ports.to_owned(), bad.to_owned()),
        ]);
        assert_eq!(recorded_network(&labels), unread(ports), "{bad:?}");
    }
}

/// An egress that records what it is asked to adopt and answers as told.
#[derive(Debug, Default)]
struct Adopting {
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    refuses: bool,
}

impl EgressPort for Adopting {
    fn ceiling(&self) -> NetworkCeiling {
        NetworkCeiling::default()
    }

    fn check(&self) -> Result<(), EgressError> {
        Ok(())
    }

    fn attach(
        &self,
        _run: &str,
        _pid: u32,
        _level: JobNetwork,
        _ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        Err(EgressError::Exhausted)
    }

    fn adopt(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        let call = format!("adopt {run} {pid} {} {}", level.name(), ports.len());
        self.calls.lock().expect("calls").push(call);
        match self.refuses {
            true => Err(EgressError::Exhausted),
            false => Ok(Box::new(Left(std::sync::Arc::clone(&self.calls)))),
        }
    }

    fn settle(&self) -> Result<(), EgressError> {
        Ok(())
    }
}

impl Adopting {
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }
}

/// A wiring that notes in a shared list that it was left.
struct Left(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Wired for Left {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        Ok(())
    }

    fn leave(self: Box<Self>) {
        self.0.lock().expect("calls").push("leave".to_owned());
    }
}

/// Asserts what a run's network takes over from the pid `crun state` named: a container whose
/// state `crun` read and that names no pid is taken over unwired; one whose state could not
/// be read is taken over unwired and marked lost, naming why; a pid is handed to the egress with the
/// run's name, level and ports; a refusal is the egress's own text and wires nothing. A
/// wiring that was taken over is left once. Catches an ending container failed for want of a
/// pid, and a failed state read taken for an ending container or refused outright.
#[test]
fn a_runs_network_takes_over_what_the_pid_names_and_leaves_it_once() {
    let port = PortMap {
        host: 8080,
        container: 80,
    };
    let egress = Adopting::default();
    let mut open = Net::asked(JobNetwork::Open, vec![port], Vec::new());
    let ending = Err("crun state: no pid in {\"status\":\"running\"}".to_owned());
    assert_eq!(open.adopt(&egress, "js-x-r2", &ending), Ok(()));
    let ending = Err("crun state: pid 99999999999 is not a pid".to_owned());
    assert_eq!(open.adopt(&egress, "js-x-r2", &ending), Ok(()));
    let unread = Err("crun state: exit status: 1: no container".to_owned());
    assert_eq!(open.lost(), None, "an ending container is not lost");
    assert_eq!(open.adopt(&egress, "js-x-r2", &unread), Ok(()));
    assert_eq!(
        open.lost().as_deref(),
        Some(
            "its network cannot be taken over: find the container's pid: crun state: exit status: 1: no container"
        )
    );
    open.leave();
    assert_eq!(egress.taken(), Vec::<String>::new());

    assert_eq!(open.adopt(&egress, "js-x-r2", &Ok(PID)), Ok(()));
    assert_eq!(egress.taken(), [format!("adopt js-x-r2 {PID} open 1")]);
    open.leave();
    open.leave();
    assert_eq!(egress.taken(), ["leave"], "left once");

    let refusing = Adopting {
        refuses: true,
        ..Adopting::default()
    };
    let mut allow = Net::asked(JobNetwork::AllowList, Vec::new(), Vec::new());
    assert_eq!(
        allow.adopt(&refusing, "js-x-r3", &Ok(PID)),
        Err(EgressError::Exhausted.to_string())
    );
    allow.leave();
    assert_eq!(
        refusing.taken(),
        [format!("adopt js-x-r3 {PID} allow-list 0")]
    );
}

/// The pid the tests of the network name.
const PID: u32 = 4321;
