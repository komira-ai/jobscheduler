//! A pool's share class ([`PoolConfig::with_share_class`]): a class that books part of a
//! host, served beside the pool's own class by the registry's provider.

mod support;

use js_provider::Pool;
use js_registry::{ConfigError, PoolConfig};
use js_types::{Admission, ClassId, Dims, ProviderId, ResourceClass};
use support::*;

fn book(name: &str) -> ResourceClass {
    ResourceClass::new(ClassId::new(name).unwrap(), Admission::Book(Dims::new()), 0)
}

fn pool() -> PoolConfig {
    PoolConfig::new(
        ProviderId::new("builders").unwrap(),
        class(),
        pool_shape(),
        4,
    )
}

/// Catches: a share class that books whole hosts, or that is the pool's own class,
/// accepted; and a pool that keeps none when one is given.
#[test]
fn a_share_class_books_part_of_a_host_and_is_not_the_pools_own() {
    assert!(pool().share_class().is_none());
    let shared = pool().with_share_class(book("share")).unwrap();
    assert_eq!(shared.share_class(), Some(&book("share")));
    let whole = ResourceClass::whole_node();
    assert_eq!(pool().with_share_class(whole), Err(ConfigError::ShareClass));
    let own = book(class().id().as_str());
    assert_eq!(pool().with_share_class(own), Err(ConfigError::ShareClass));
}

/// Catches: the share class set but not served: the registry's provider serves the pool's
/// class and then the share class; without one, the pool's class alone.
#[test]
fn the_registrys_provider_serves_the_share_class_beside_the_pools() {
    let plain = fixture_of(pool());
    assert_eq!(plain.registry.provider().classes(), vec![class()]);
    let shared = fixture_of(pool().with_share_class(book("share")).unwrap());
    let served = shared.registry.provider().classes();
    assert_eq!(served, vec![class(), book("share")]);
}
