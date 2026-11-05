//! What the leases on each host book there, as a listing of the hosts reads it.

mod support;

use js_provider::Pool;
use support::*;

/// Asserts that a lease booked on a host is listed on that host, with the resources it
/// books there (for a class that books the whole host, the host's whole shape), and that a
/// host with no lease, registered or not, is not listed. It catches a listing that drops
/// the resources of a lease, names it on another host, or lists empty hosts.
#[test]
fn each_host_lists_the_leases_booked_on_it() {
    let mut fx = fixture(2);
    assert!(fx.registry.held().is_empty());
    fx.join("a", "t");
    assert!(fx.registry.held().is_empty());
    assert_eq!(started_on(&fx.provider.start(spec(1))), Some("a"));
    fx.join("b", "u");
    let held = fx.registry.held();
    assert_eq!(held.keys().collect::<Vec<_>>(), [&host("a")]);
    let on_a = &held[&host("a")];
    assert_eq!(on_a.len(), 1);
    assert_eq!((on_a[0].lease, on_a[0].whole), (lease(1), true));
    let booked = &on_a[0].resources;
    let whole = pool_shape();
    let whole = whole.resources();
    assert_eq!(
        (booked.cpu_millis(), booked.gpus()),
        (whole.cpu_millis(), whole.gpus())
    );
}
