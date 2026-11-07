//! Runs the conformance suite against the layer the lease server runs on when agents
//! register hosts: a static pool holding `h1` and a registry holding the registered
//! host `h2`.

use std::collections::BTreeMap;
use std::sync::Arc;

use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::LayeredProvider;
use js_pool_static::StaticPool;
use js_provider::{Call, Pool};
use js_registry::{Credential, PoolConfig, Registry};
use js_sched_placer::engine::Settings;
use js_sched_placer::place::SchedPlacer;
use js_types::{HostId, LeaseId, ProviderId, ResourceClass, Shape};

mod common;
use common::hello;

const STATIC_HOST: &str = "\
[[host]]
id = \"h1\"
gpu_class = \"conformance-gpu\"
[host.resources]
cpu_millis = 8000
memory_bytes = 1073741824
gpus = 1
";

struct Layered {
    provider: LayeredProvider,
}

/// The layer over the static pool of `h1` and the registry of `h2`, both serving the
/// shipped class.
fn layer() -> LayeredProvider {
    let classes = vec![ResourceClass::gpu_host()];
    let hosts = js_conformance::standard_hosts();
    let (_, shape) = hosts[1].clone();
    // The scheduler describes only hosts that name an architecture.
    let shape = Shape::new(
        shape.resources().clone(),
        BTreeMap::from([("arch".to_string(), "arm64".to_string())]),
    );
    let config = PoolConfig::new(
        ProviderId::new("pool").unwrap(),
        ResourceClass::gpu_host(),
        shape.clone(),
        1,
    );
    let clock = ManualClock::new(Timestamp::from_millis(1));
    let placer = SchedPlacer::new(Settings::default(), clock.clone());
    let registry = Registry::new(config, clock, Arc::new(placer));
    registry.reserve([hosts[0].0.clone()]);
    let admitted = registry.hello(
        &hello(hosts[1].0.as_str(), &shape),
        &Credential::of_token("t"),
    );
    admitted.expect("the registered host meets the pool's shape");
    let pool = StaticPool::from_toml(ProviderId::new("static").unwrap(), STATIC_HOST).unwrap();
    LayeredProvider::new(
        ProviderId::new("layered").unwrap(),
        vec![
            Box::new(pool.with_classes(classes.clone())),
            Box::new(registry.provider().with_classes(classes)),
        ],
    )
}

fn layered() -> Layered {
    Layered { provider: layer() }
}

impl Subject for Layered {
    fn provider(&mut self) -> &mut dyn Pool {
        &mut self.provider
    }

    /// The booked hosts, read from the capacity both children report.
    fn booked(&self) -> BTreeMap<HostId, LeaseId> {
        let hosts = self.provider.capacity();
        hosts
            .into_iter()
            .filter_map(|host| Some((host.host, host.leased_to?)))
            .collect()
    }

    /// Neither child answers Unknown to a call, so neither can be made to.
    fn script_unknown(&mut self, _: Call, _: bool) -> bool {
        false
    }

    /// The registry child leases every class that books part of a host, so no class is
    /// listed that the layer cannot lease.
    fn list_unleasable_class(&mut self, _: ResourceClass) -> bool {
        false
    }
}

/// Asserts that the layer passes every check except the three it cannot take part in (it
/// never answers Unknown to a call, and its registry leases every class that books part of
/// a host, so it has no unleasable class to refuse), which are reported as not applicable
/// and not skipped silently. A start that lands a lease on both children, a stop that one
/// child does not hear, or a capacity list that repeats or drops a host fails here.
#[test]
fn the_layered_provider_conforms() {
    let report = run(layered);
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
