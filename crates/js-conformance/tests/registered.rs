//! Runs the conformance suite against the provider of self-registered hosts.

use std::collections::BTreeMap;

use js_agent_wire::{Heartbeat, Phase};
use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Call, Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_registry::{Credential, PoolConfig, RegisteredProvider, Registry};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

mod common;
use common::hello;

/// The provider the lease table would run on, with the agents of the hosts beside it: a
/// whole-node start commits a host and answers Unknown until the host's agent reports it
/// ready, so a start that answers Unknown is followed by that report and the start is
/// repeated, as the table's retry of a pending start would do. The report is the only
/// thing added; a start the registry answers any other way is passed on as it is.
struct WithAgents {
    inner: RegisteredProvider,
    registry: Registry,
    /// The epoch and session each host was welcomed with.
    sessions: BTreeMap<HostId, (u64, String)>,
}

impl WithAgents {
    /// The agent of the host `lease` is booked on reports the host ready for it.
    fn report_ready(&self, lease: &LeaseId) {
        let booked = self.inner.booked();
        let held = booked.iter().find(|(_, holder)| *holder == lease);
        let (host, _) = held.expect("a start the registry committed booked a host");
        let (epoch, session) = self.sessions[host].clone();
        let message = Heartbeat {
            host_id: host.to_string(),
            epoch,
            session,
            phase: Phase::Serving,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: Some(lease.to_string()),
            exclusive_refused: None,
        };
        self.registry
            .heartbeat(&message, &Credential::of_token("t"))
            .expect("the agent of a registered host is heard");
    }
}

impl Provider for WithAgents {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn flavour(&self) -> Flavour {
        self.inner.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.inner.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        let lease = spec.lease();
        match self.inner.start(spec.clone()) {
            StartAck::Unknown => {
                self.report_ready(&lease);
                self.inner.start(spec)
            }
            other => other,
        }
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.inner.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.inner.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.inner.capacity()
    }
}

/// A registry over the standard hosts of the suite, both registered and live, and the
/// provider the lease table would run on.
struct Registered {
    provider: WithAgents,
}

fn registered() -> Registered {
    let hosts = js_conformance::standard_hosts();
    let (_, shape) = hosts[0].clone();
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, ResourceClass::gpu_host(), shape, hosts.len());
    let registry = Registry::new(config, ManualClock::new(Timestamp::from_millis(1)));
    let mut sessions = BTreeMap::new();
    for (id, shape) in &hosts {
        let admitted = registry.hello(&hello(id.as_str(), shape), &Credential::of_token("t"));
        let welcome = admitted.expect("the standard hosts meet the pool's shape");
        sessions.insert(id.clone(), (welcome.epoch, welcome.session));
    }
    Registered {
        provider: WithAgents {
            inner: registry.provider(),
            registry,
            sessions,
        },
    }
}

impl Subject for Registered {
    fn provider(&mut self) -> &mut dyn Provider {
        &mut self.provider
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.provider.inner.booked()
    }

    /// A registry answers from memory and is never made to answer Unknown by a call.
    fn script_unknown(&mut self, _: Call, _: bool) -> bool {
        false
    }

    fn list_unleasable_class(&mut self, class: ResourceClass) -> bool {
        let kept = self.provider.inner.clone();
        self.provider.inner = kept.with_classes(vec![ResourceClass::gpu_host(), class]);
        true
    }
}

/// Asserts that the registered provider passes every check except the two it cannot take
/// part in (it never answers Unknown to a call), which are reported as not applicable and
/// not skipped silently. A breach of the provider contract in the registry (a start that
/// is not idempotent, a stopped lease that restarts, a read-back that disagrees with the
/// booking table) fails here.
#[test]
fn the_registered_provider_conforms() {
    let report = run(registered);
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(
        report.not_applicable(),
        vec![Check::UnknownDoesNotFree, Check::StartUnknownThenRetry]
    );
    let passed = report
        .results
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Passed)
        .count();
    assert_eq!(passed, Check::ALL.len() - 2);
}
