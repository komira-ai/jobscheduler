//! Wrappers that give the lease table one concrete type, so the server is not generic
//! over its pool and clock.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_provider::{HostCapacity, Observed, Pool, StartAck, StartSpec, StopAck};
use js_types::{Clock, Timestamp};
use js_types::{LeaseId, ProviderId, ResourceClass};

/// The pool the lease table runs on, behind a lock that the host listing shares.
type Locked = Arc<Mutex<Box<dyn Pool + Send>>>;

/// Takes the pool's lock. Every call through it is one synchronous call of the pool, so a
/// panic inside one leaves no half-made change the next call could see; a poisoned lock is
/// taken as it is.
fn lock(pool: &Locked) -> MutexGuard<'_, Box<dyn Pool + Send>> {
    pool.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A pool behind a box, which the lease table owns. [`BoxedPool::hosts`] hands out a
/// handle that reads the same pool's hosts.
pub(crate) struct BoxedPool(Locked);

impl BoxedPool {
    /// The table's pool `pool`.
    pub(crate) fn new(pool: Box<dyn Pool + Send>) -> Self {
        Self(Arc::new(Mutex::new(pool)))
    }

    /// A handle that lists this pool's hosts without the lease table's lock.
    pub(crate) fn hosts(&self) -> PoolHosts {
        PoolHosts(Arc::clone(&self.0))
    }
}

impl Pool for BoxedPool {
    fn id(&self) -> ProviderId {
        lock(&self.0).id()
    }

    fn classes(&self) -> Vec<ResourceClass> {
        lock(&self.0).classes()
    }

    fn start(&mut self, spec: StartSpec) -> StartAck {
        lock(&self.0).start(spec)
    }

    fn stop(&mut self, lease: &LeaseId) -> StopAck {
        lock(&self.0).stop(lease)
    }

    fn observe(&mut self, lease: &LeaseId) -> Observed {
        lock(&self.0).observe(lease)
    }

    fn capacity(&self) -> Vec<HostCapacity> {
        lock(&self.0).capacity()
    }

    fn waits(&self, lease: &LeaseId) -> bool {
        lock(&self.0).waits(lease)
    }
}

/// The hosts of the pool the lease table runs on, read beside the table: the host table
/// of the static pool, and of the registry when agents register hosts. The host listing
/// reads it, so listing hosts takes neither the table's lock nor a call of the table.
/// Clones read the same pool.
#[derive(Clone)]
pub(crate) struct PoolHosts(Locked);

impl PoolHosts {
    /// Every host of the pool, with its shape and the leases booked on it: what
    /// [`Pool::capacity`] answers. It holds the pool's lock for that one call.
    pub(crate) fn capacity(&self) -> Vec<HostCapacity> {
        lock(&self.0).capacity()
    }
}

/// A clock the server and its caller can both hold: clones read the same source.
///
/// The server reads time through this and nowhere else, so a test gives it a
/// [`js_types::ManualClock`] and the server's notion of time moves only when the test
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

    use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
    use js_types::ManualClock;

    use super::*;

    /// Asserts that the boxed provider answers every call of the contract as the
    /// provider inside it does, and that a shared clock reads the clock it wraps.
    #[test]
    fn wrappers_delegate() {
        let id = ProviderId::new("p").unwrap();
        let pool = StaticPool::from_toml(id.clone(), EXAMPLE_CONFIG).unwrap();
        let mut boxed = BoxedPool::new(Box::new(pool));
        assert_eq!(boxed.id(), id);
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
            probe_host: None,
        };
        let spec = StartSpec::new(LeaseId::new(1, 2), request, &boxed.classes()).unwrap();
        assert!(matches!(boxed.start(spec), StartAck::Started { .. }));

        let manual = ManualClock::new(Timestamp::from_millis(5));
        let shared = SharedClock::new(manual.clone());
        manual.set(Timestamp::from_millis(9));
        assert_eq!(shared.clone().now(), Timestamp::from_millis(9));
    }

    /// Asserts that the hosts handle reads the pool the table owns: a host is free before
    /// a start through the boxed pool, holds the lease after it, is free again after the
    /// stop, and a clone of the handle reads the same.
    #[test]
    fn the_hosts_handle_reads_the_tables_pool() {
        let id = ProviderId::new("p").unwrap();
        let pool = StaticPool::from_toml(id, EXAMPLE_CONFIG).unwrap();
        let mut boxed = BoxedPool::new(Box::new(pool));
        let hosts = boxed.hosts();
        let leased = |hosts: &PoolHosts| -> Vec<Vec<LeaseId>> {
            hosts
                .capacity()
                .into_iter()
                .map(|host| host.leases)
                .collect()
        };
        assert_eq!(leased(&hosts), [vec![], vec![]]);
        let resources = js_types::Resources::new(1, 1, 1, Some("nvidia-a100".into())).unwrap();
        let request = js_types::Request {
            class: js_types::ClassId::new("gpu-host").unwrap(),
            resources,
            capabilities: BTreeMap::new(),
            max_wall: std::time::Duration::from_secs(1),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        let lease = LeaseId::new(1, 1);
        let spec = StartSpec::new(lease, request, &boxed.classes()).unwrap();
        assert!(matches!(boxed.start(spec), StartAck::Started { .. }));
        assert_eq!(leased(&hosts), [vec![lease], vec![]]);
        assert_eq!(leased(&hosts.clone()), [vec![lease], vec![]]);
        assert!(matches!(boxed.stop(&lease), StopAck::Stopped { .. }));
        assert_eq!(leased(&hosts), [vec![], vec![]]);
    }
}
