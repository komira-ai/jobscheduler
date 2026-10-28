//! Runs the conformance suite against the provider of self-registered hosts.

use std::collections::BTreeMap;

use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Call, Provider};
use js_registry::{Credential, PoolConfig, RegisteredProvider, Registry};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

mod common;
use common::hello;

/// A registry over the standard hosts of the suite, both registered and live, and the
/// provider the lease table would run on.
struct Registered {
    provider: RegisteredProvider,
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

/// The registered provider of a `whole-node` pool over the standard hosts: its leases are
/// committed and wait for the agents' ready reports.
struct RegisteredWhole(Registered);

fn registered_whole() -> RegisteredWhole {
    let hosts = js_conformance::standard_hosts();
    let (_, shape) = hosts[0].clone();
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, ResourceClass::whole_node(), shape, hosts.len());
    let registry = Registry::new(config, ManualClock::new(Timestamp::from_millis(1)));
    for (id, shape) in &hosts {
        let admitted = registry.hello(&hello(id.as_str(), shape), &Credential::of_token("t"));
        admitted.expect("the standard hosts meet the pool's shape");
    }
    RegisteredWhole(Registered {
        provider: registry.provider(),
    })
}

impl Subject for RegisteredWhole {
    fn provider(&mut self) -> &mut dyn Provider {
        self.0.provider()
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.0.booked()
    }

    fn script_unknown(&mut self, call: Call, landed: bool) -> bool {
        self.0.script_unknown(call, landed)
    }

    fn class(&self) -> ResourceClass {
        ResourceClass::whole_node()
    }

    fn list_unleasable_class(&mut self, class: ResourceClass) -> bool {
        let kept = self.0.provider.clone();
        self.0.provider = kept.with_classes(vec![ResourceClass::whole_node(), class]);
        true
    }
}

/// Asserts that the whole-node registered provider passes every check but the two that
/// need an Unknown answer and the two that expect a start to be answered `Started` and
/// read Present at once, which a lease waiting for its agent's report cannot be. Those
/// four are reported as not applicable. A booking made before the host is ready that
/// breaks idempotence, a stop that leaves the host booked, or a late start that books a
/// host fails here.
#[test]
fn the_whole_node_registered_provider_conforms() {
    let report = run(registered_whole);
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(
        report.not_applicable(),
        vec![
            Check::StartThenPresent,
            Check::UnknownDoesNotFree,
            Check::StartIsIdempotent,
            Check::StartUnknownThenRetry
        ]
    );
}
