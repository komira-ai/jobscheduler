//! The two guards of the marks that the public calls cannot reach: a lease booked on a
//! marked host (the marks refuse to be set on a leased host, and a marked host is never
//! booked, so the state only arises if a placement path ever forgets the mark), so these
//! tests put it there through the registry's own state.

use std::collections::BTreeMap;

use js_agent_wire::{Hello, Isolation, JobSpec, Limits, Phase, ProtocolRange, WireShape};
use js_lease::{ManualClock, Timestamp};
use js_types::{Admission, ClassId, LeaseId, ProviderId, ResourceClass, Resources, Shape};

use super::*;
use crate::credential::Credential;
use crate::runs::RunRefusal;
use crate::session::SequentialSessions;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    let pairs = pairs.iter();
    pairs.map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn registry() -> Registry {
    let class = ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0);
    let resources = Resources::new(4000, 8 << 30, 0, None).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos")]));
    let name = ProviderId::new("builders").unwrap();
    let config = crate::PoolConfig::new(name, class, shape, 2);
    let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
    Registry::with_sessions(config, clock, SequentialSessions::new())
}

fn hello() -> Hello {
    Hello {
        protocol: ProtocolRange { min: 1, max: 1 },
        host_id: "a".into(),
        pool: "builders".into(),
        agent_version: "0.1.0".into(),
        instance: None,
        shape: WireShape {
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            gpus: 0,
            gpu_class: None,
            max_leases: None,
        },
        capabilities: map(&[("os", "macos")]),
        gpu_devices: Vec::new(),
        labels: BTreeMap::new(),
        phase: Phase::Serving,
        held: Vec::new(),
        accepts_host_credential: false,
    }
}

fn job() -> JobSpec {
    JobSpec {
        run_id: "r1".into(),
        lease_id: String::new(),
        image: "busybox:1".into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

/// A registry with host `a` admitted and lease 1 booked on it.
fn leased() -> (Registry, HostId, LeaseId) {
    let registry = registry();
    registry
        .hello(&hello(), &Credential::of_token("t"))
        .unwrap();
    let (host, lease) = (HostId::new("a").unwrap(), LeaseId::new(1, 1));
    registry.lock().book.seat(&host, lease).unwrap();
    (registry, host, lease)
}

fn set_leaving(registry: &Registry, host: &HostId, leaving: Option<Leaving>) {
    let mut state = registry.lock();
    state.records.get_mut(host).unwrap().leaving = leaving;
}

/// Asserts that a run is not queued on a host that is draining or releasing, even when the
/// registry's booking names the lease: the refusal is `no-capacity`, the one a host out of
/// the serving phase gives, and the same call is queued once the mark is gone. It catches
/// a run start that reads the lease table and not the mark, so a booking that outlived a
/// drain could still start work on a host the scaler is giving back.
#[test]
fn a_run_is_not_queued_on_a_marked_host() {
    let (registry, host, lease) = leased();
    for leaving in [Leaving::Draining, Leaving::Releasing] {
        set_leaving(&registry, &host, Some(leaving));
        let refused = registry.queue_run(&host, &lease, job());
        assert_eq!(refused.unwrap_err(), RunRefusal::NoCapacity, "{leaving:?}");
    }
    set_leaving(&registry, &host, None);
    assert!(registry.queue_run(&host, &lease, job()).is_ok());
}

/// Asserts that `remove` keeps a releasing host that still holds a lease: `Busy::Lease`,
/// the host stays listed and its booking stays, and the host goes once the lease is
/// stopped. It catches a remove that drops a booking along with its host.
#[test]
fn remove_keeps_a_releasing_host_that_holds_a_lease() {
    let (registry, host, lease) = leased();
    set_leaving(&registry, &host, Some(Leaving::Releasing));
    assert_eq!(registry.remove(&host), Err(Busy::Lease));
    assert_eq!(registry.hosts().len(), 1);
    assert_eq!(registry.host_of(&lease), Some(host.clone()));
    registry.lock().book.stop(&lease);
    assert_eq!(registry.remove(&host), Ok(()));
    assert!(registry.hosts().is_empty());
}
