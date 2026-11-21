//! The fake egress port the stand-in tests share: it records what it was asked, in order,
//! and answers as the test sets it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use js_agent::crun::{EgressPort, Wired};
use js_agent::egress::EgressError;
use js_agent_wire::NetworkCeiling;
use js_types::{JobNetwork, PortMap};

use crate::node::Node;

/// What the port was asked, and how it answers.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    /// `check <sidecars>` (the leases the ledger held when the check was asked),
    /// `attach <run> <pid>`, `adopt <run> <level> <ports>`, `settle`, `release <run>`
    /// and `leave <run>`, in order.
    pub(crate) calls: Mutex<Vec<String>>,
    /// The directory of the runtime's sidecars.
    pub(crate) ledger: PathBuf,
    /// The level of each attach, in order.
    pub(crate) levels: Mutex<Vec<JobNetwork>>,
    /// The declared ports of each attach, in order.
    pub(crate) ports: Mutex<Vec<Vec<PortMap>>>,
    /// The level and ports of each adopt, in order.
    pub(crate) adopted: Mutex<Vec<(JobNetwork, Vec<PortMap>)>>,
    pub(crate) check_fails: AtomicBool,
    /// How often the port was asked to settle.
    pub(crate) settles: AtomicUsize,
    pub(crate) adopt_fails: AtomicBool,
    pub(crate) settle_fails: AtomicBool,
    pub(crate) attach_fails: AtomicBool,
    pub(crate) release_fails: AtomicBool,
}

impl Seen {
    pub(crate) fn new(node: &Node) -> Arc<Self> {
        Arc::new(Self {
            ledger: node.scratch.join("oci-labels"),
            ..Self::default()
        })
    }

    fn note(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }

    /// The calls so far, and empties the record.
    pub(crate) fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }
}

/// A port that records in `seen`, except for its settle (every open settles once, which
/// the tests of the other calls leave out of their record; `seen.settles` counts it).
pub(crate) fn port(seen: &Arc<Seen>) -> Option<Arc<dyn EgressPort>> {
    Some(Arc::new(Port(Arc::clone(seen), false)))
}

/// A port that records in `seen`, its settle too.
pub(crate) fn port_logging_settle(seen: &Arc<Seen>) -> Option<Arc<dyn EgressPort>> {
    Some(Arc::new(Port(Arc::clone(seen), true)))
}

/// The fake port; whether its settle is in the call record.
#[derive(Debug)]
struct Port(Arc<Seen>, bool);

impl EgressPort for Port {
    fn ceiling(&self) -> NetworkCeiling {
        NetworkCeiling {
            level: JobNetwork::AllowList,
            ports: Vec::new(),
        }
    }

    fn check(&self) -> Result<(), EgressError> {
        let sidecars = std::fs::read_dir(&self.0.ledger).map_or(0, Iterator::count);
        self.0.note(format!("check {sidecars}"));
        match self.0.check_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Rules),
            false => Ok(()),
        }
    }

    fn attach(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        self.0.levels.lock().expect("levels").push(level);
        self.0.ports.lock().expect("ports").push(ports.to_vec());
        self.0.note(format!("attach {run} {pid}"));
        match self.0.attach_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Exhausted),
            false => Ok(Box::new(Wire {
                seen: Arc::clone(&self.0),
                run: run.to_owned(),
            })),
        }
    }

    fn adopt(
        &self,
        run: &str,
        level: JobNetwork,
        ports: &[PortMap],
    ) -> Result<Box<dyn Wired>, EgressError> {
        let each: Vec<String> = ports
            .iter()
            .map(|port| format!("{}:{}", port.host, port.container))
            .collect();
        let each = each.join(",");
        let call = format!("adopt {run} {} {each}", level.name());
        self.0
            .adopted
            .lock()
            .expect("adopted")
            .push((level, ports.to_vec()));
        self.0.note(call);
        match self.0.adopt_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Unadoptable {
                run: run.to_owned(),
                why: "its record is not wired".to_owned(),
            }),
            false => Ok(Box::new(Wire {
                seen: Arc::clone(&self.0),
                run: run.to_owned(),
            })),
        }
    }

    fn settle(&self) -> Result<(), EgressError> {
        self.0.settles.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            self.0.note("settle".to_owned());
        }
        match self.0.settle_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Rules),
            false => Ok(()),
        }
    }
}

/// One run's fake wiring.
struct Wire {
    seen: Arc<Seen>,
    run: String,
}

impl Wired for Wire {
    fn release(self: Box<Self>) -> Result<(), EgressError> {
        self.seen.note(format!("release {}", self.run));
        match self.seen.release_fails.load(Ordering::SeqCst) {
            true => Err(EgressError::Command {
                what: "deleting the veth".to_owned(),
                why: "Operation not permitted".to_owned(),
            }),
            false => Ok(()),
        }
    }

    fn leave(self: Box<Self>) {
        self.seen.note(format!("leave {}", self.run));
    }
}
