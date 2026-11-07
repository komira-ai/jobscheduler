//! Runs the conformance suite against the provider of self-registered hosts.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Call, Pool};
use js_registry::{Credential, PoolConfig, RegisteredProvider, Registry};
use js_sched_placer::engine::Settings;
use js_sched_placer::place::SchedPlacer;
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

mod common;
use common::hello;

/// A registry over the standard hosts of the suite, both registered and live, and the
/// provider the lease table would run on.
struct Registered {
    provider: RegisteredProvider,
}

/// `hosts` with an architecture named: the scheduler describes only hosts that have one.
fn with_arch(hosts: Vec<(HostId, Shape)>) -> Vec<(HostId, Shape)> {
    let caps = BTreeMap::from([("arch".to_string(), "arm64".to_string())]);
    let named = |shape: Shape| Shape::new(shape.resources().clone(), caps.clone());
    hosts.into_iter().map(|(id, s)| (id, named(s))).collect()
}

/// A registry over `config` on the production placer, as the server builds it.
fn registry_of(config: PoolConfig) -> Registry {
    let clock = ManualClock::new(Timestamp::from_millis(1));
    let placer = SchedPlacer::new(Settings::default(), clock.clone());
    Registry::new(config, clock, Arc::new(placer))
}

fn registered() -> Registered {
    let hosts = js_conformance::standard_hosts();
    let hosts = with_arch(hosts);
    let (_, shape) = hosts[0].clone();
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, ResourceClass::gpu_host(), shape, hosts.len());
    let registry = registry_of(config);
    for (id, shape) in &hosts {
        let admitted = registry.hello(&hello(id.as_str(), shape), &Credential::of_token("t"));
        admitted.expect("the standard hosts meet the pool's shape");
    }
    Registered {
        provider: registry.provider(),
    }
}

impl Subject for Registered {
    fn provider(&mut self) -> &mut dyn Pool {
        &mut self.provider
    }

    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        self.provider.booked()
    }

    /// A registry answers from memory and is never made to answer Unknown by a call.
    fn script_unknown(&mut self, _: Call, _: bool) -> bool {
        false
    }

    /// A registry leases every class that books part of a host, so it has no class to
    /// list that it cannot lease.
    fn list_unleasable_class(&mut self, _: ResourceClass) -> bool {
        false
    }
}

/// Asserts that the registered provider passes every check except the three it cannot take
/// part in (it never answers Unknown to a call, and it leases every class that books part
/// of a host, so it has no unleasable class to refuse), which are reported as not
/// applicable and not skipped silently. A breach of the provider contract in the registry
/// (a start that is not idempotent, a stopped lease that restarts, a read-back that
/// disagrees with the booking table) fails here.
#[test]
fn the_registered_provider_conforms() {
    let report = run(registered);
    assert!(report.failed().is_empty(), "{report:?}");
    assert_eq!(
        report.not_applicable(),
        vec![
            Check::UnknownDoesNotFree,
            Check::StartUnknownThenRetry,
            Check::UnsupportedClassIsRefused
        ]
    );
    let passed = report
        .results
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Passed)
        .count();
    assert_eq!(passed, Check::ALL.len() - 3);
}

/// The registered provider of a `whole-node` pool over the standard hosts: its leases are
/// committed and wait for the agents' ready reports.
struct RegisteredWhole(Registered);

fn registered_whole() -> RegisteredWhole {
    let hosts = js_conformance::standard_hosts();
    let hosts = with_arch(hosts);
    let (_, shape) = hosts[0].clone();
    let name = ProviderId::new("pool").unwrap();
    let config = PoolConfig::new(name, ResourceClass::whole_node(), shape, hosts.len());
    let registry = registry_of(config);
    for (id, shape) in &hosts {
        let admitted = registry.hello(&hello(id.as_str(), shape), &Credential::of_token("t"));
        admitted.expect("the standard hosts meet the pool's shape");
    }
    RegisteredWhole(Registered {
        provider: registry.provider(),
    })
}

impl Subject for RegisteredWhole {
    fn provider(&mut self) -> &mut dyn Pool {
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

    /// As [`Registered`]: no class is listed that the registry cannot lease.
    fn list_unleasable_class(&mut self, _: ResourceClass) -> bool {
        false
    }
}

/// Asserts that the whole-node registered provider passes every check but the two that
/// need an Unknown answer, the two that expect a start to be answered `Started` and
/// read Present at once, which a lease waiting for its agent's report cannot be, and the
/// refusal of an unleasable class, which a registry has none of. Those five are reported
/// as not applicable. A booking made before the host is ready that
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
            Check::StartUnknownThenRetry,
            Check::UnsupportedClassIsRefused
        ]
    );
}
