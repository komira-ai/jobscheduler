//! What the static pool lists from its configuration, through its public interface.

use std::path::Path;

use js_pool_static::{ConfigError, EXAMPLE_CONFIG, StaticPool};
use js_types::{Admission, ClassId, HostId, ProviderId, ResourceClass};

fn pool(config: &str) -> StaticPool {
    StaticPool::from_toml(ProviderId::new("pool").unwrap(), config).unwrap()
}

fn example() -> StaticPool {
    pool(EXAMPLE_CONFIG)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// Asserts that the pool reports its id and the one shipped class.
#[test]
fn identity_and_classes() {
    let p = example();
    assert_eq!(p.id().as_str(), "pool");
    assert_eq!(p.classes(), vec![ResourceClass::gpu_host()]);
}

/// Asserts that `with_classes` replaces the classes the pool serves and nothing else.
#[test]
fn with_classes_replaces_the_served_classes() {
    let id = ClassId::new("whole-node").unwrap();
    let other = ResourceClass::new(id, Admission::Whole, 0);
    let p = example().with_classes(vec![other.clone()]);
    assert_eq!(p.classes(), vec![other]);
    assert_eq!(p.id().as_str(), "pool");
    assert_eq!(p.hosts().len(), 2);
}

/// Asserts that the listing of the shipped example names both hosts in id order, free of
/// any lease, each with the shape the file gives it.
#[test]
fn the_example_lists_its_hosts_free() {
    let listed = example().hosts();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].host, host("gpu-01"));
    assert_eq!(listed[1].host, host("gpu-02"));
    assert!(listed.iter().all(|host| host.leased_to.is_none()));
    assert!(listed.iter().all(|host| host.leases.is_empty()));
    assert_eq!(listed[1].shape.resources().gpus(), 8);
}

/// Asserts that the pool loads from a file path and that an unreadable path is
/// refused with the path named.
#[test]
fn config_file_loads_and_missing_file_is_refused() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("example.toml");
    let id = ProviderId::new("pool").unwrap();
    let loaded = StaticPool::from_config_file(id.clone(), &path).unwrap();
    assert_eq!(loaded.hosts().len(), 2);
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("absent.toml");
    let err = StaticPool::from_config_file(id, &missing).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }));
    assert!(err.to_string().contains("absent.toml"));
}
