//! Wrappers that give the lease table one concrete type, so the server is not generic
//! over its provider and clock.

use std::sync::Arc;

use js_lease::{Clock, Timestamp};
use js_provider::{Flavour, HostCapacity, Observed, Provider, StartAck, StartSpec, StopAck};
use js_types::{LeaseId, ProviderId, ResourceClass};

/// A provider behind a box.
pub(crate) struct BoxedProvider(pub(crate) Box<dyn Provider + Send>);

impl Provider for BoxedProvider {
    fn id(&self) -> ProviderId {
        self.0.id()
    }

    fn flavour(&self) -> Flavour {
        self.0.flavour()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        self.0.classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        self.0.start(spec)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        self.0.stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        self.0.observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        self.0.capacity()
    }
}

/// A clock the server and its caller can both hold: clones read the same source.
///
/// The server reads time through this and nowhere else, so a test gives it a
/// [`js_lease::ManualClock`] and the server's notion of time moves only when the test
/// says so.
#[derive(Clone)]
pub struct SharedClock(Arc<dyn Clock + Send + Sync>);

impl SharedClock {
    /// Shares `clock`.
    pub fn new(clock: impl Clock + Send + Sync + 'static) -> Self {
        Self(Arc::new(clock))
    }
}

impl Clock for SharedClock {
    fn now(&self) -> Timestamp {
        self.0.now()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use js_lease::ManualClock;
    use js_pool_static::{EXAMPLE_CONFIG, StaticPool};

    use super::*;

    /// Asserts that the boxed provider answers every call of the contract as the
    /// provider inside it does, and that a shared clock reads the clock it wraps.
    #[test]
    fn wrappers_delegate() {
        let id = ProviderId::new("p").unwrap();
        let pool = StaticPool::from_toml(id.clone(), EXAMPLE_CONFIG).unwrap();
        let mut boxed = BoxedProvider(Box::new(pool));
        assert_eq!(boxed.id(), id);
        assert_eq!(boxed.flavour(), Flavour::Delegated);
        assert_eq!(boxed.classes(), vec![ResourceClass::gpu_host()]);
        assert_eq!(boxed.capacity().len(), 2);
        let lease = LeaseId::new(1, 1);
        assert_eq!(boxed.observe(&lease), Observed::Absent);
        assert!(matches!(boxed.stop(&lease), StopAck::Stopped { .. }));
        let resources = js_types::Resources::new(1, 1, 1, Some("nvidia-a100".into())).unwrap();
        let request = js_types::Request {
            class: js_types::ClassId::new("gpu-host").unwrap(),
            resources,
            capabilities: BTreeMap::new(),
            max_wall: std::time::Duration::from_secs(1),
            labels: BTreeMap::new(),
            validation_run: None,
        };
        let spec = StartSpec::new(LeaseId::new(1, 2), request, &boxed.classes()).unwrap();
        assert!(matches!(boxed.start(spec), StartAck::Started { .. }));

        let manual = ManualClock::new(Timestamp::from_millis(5));
        let shared = SharedClock::new(manual.clone());
        manual.set(Timestamp::from_millis(9));
        assert_eq!(shared.clone().now(), Timestamp::from_millis(9));
    }
}
