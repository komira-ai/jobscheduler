//! Runs the conformance suite against the provider of self-registered hosts.

use std::collections::BTreeMap;

use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Call, Provider};
use js_registry::{Credential, PoolConfig, RegisteredProvider, Registry};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

/// A registry over the standard hosts of the suite, both registered and live, and the
/// provider the lease table would run on.
struct Registered {
    provider: RegisteredProvider,
}

fn hello(host: &str, shape: &Shape) -> Hello {
    let resources = shape.resources();
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: host.into(),
        pool: "pool".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: resources.cpu_millis(),
            memory_bytes: resources.memory_bytes(),
            gpus: resources.gpus(),
            gpu_class: resources.gpu_class().map(str::to_string),
            max_leases: None,
        },
        capabilities: shape.capabilities().clone(),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
    }
}

fn registered() -> Registered {
    let hosts = js_conformance::standard_hosts();
    let (_, shape) = hosts[0].clone();
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, ResourceClass::gpu_host(), shape, hosts.len());
    let registry = Registry::new(config, ManualClock::new(Timestamp::from_millis(1)));
    for (id, shape) in &hosts {
        let admitted = registry.hello(&hello(id.as_str(), shape), &Credential::of_token("t"));
        admitted.expect("the standard hosts meet the pool's shape");
    }
    Registered {
        provider: registry.provider(),
    }
}

impl Subject for Registered {
    fn provider(&mut self) -> &mut dyn Provider {
        &mut self.provider
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.provider.booked()
    }

    /// A registry answers from memory and is never made to answer Unknown by a call.
    fn script_unknown(&mut self, _: Call, _: bool) -> bool {
        false
    }

    fn list_unleasable_class(&mut self, class: ResourceClass) -> bool {
        let kept = self.provider.clone();
        self.provider = kept.with_classes(vec![ResourceClass::gpu_host(), class]);
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
