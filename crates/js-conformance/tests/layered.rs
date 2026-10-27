//! Runs the conformance suite against the layer the lease server runs on when agents
//! register hosts: a static pool holding `h1` and a registry holding the registered
//! host `h2`.

use std::collections::BTreeMap;

use js_conformance::{Check, Subject, Verdict, run};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::LayeredProvider;
use js_pool_static::StaticPool;
use js_provider::{Call, Provider};
use js_registry::{Credential, PoolConfig, Registry};
use js_types::{HostId, LeaseId, ProviderId, ResourceClass};

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
/// shipped class and `extra` when given.
fn layer(extra: Option<ResourceClass>) -> LayeredProvider {
    let classes = [Some(ResourceClass::gpu_host()), extra];
    let classes = classes.into_iter().flatten().collect::<Vec<_>>();
    let hosts = js_conformance::standard_hosts();
    let (_, shape) = hosts[1].clone();
    let config = PoolConfig::new(
        ProviderId::new("pool").unwrap(),
        ResourceClass::gpu_host(),
        shape.clone(),
        1,
    );
    let registry = Registry::new(config, ManualClock::new(Timestamp::from_millis(1)));
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
    Layered {
        provider: layer(None),
    }
}

impl Subject for Layered {
    fn provider(&mut self) -> &mut dyn Provider {
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

    fn list_unleasable_class(&mut self, class: ResourceClass) -> bool {
        self.provider = layer(Some(class));
        true
    }
}

/// Asserts that the layer passes every check except the two it cannot take part in (it
/// never answers Unknown to a call), which are reported as not applicable and not
/// skipped silently. A start that lands a lease on both children, a stop that one child
/// does not hear, or a capacity list that repeats or drops a host fails here.
#[test]
fn the_layered_provider_conforms() {
    let report = run(layered);
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
