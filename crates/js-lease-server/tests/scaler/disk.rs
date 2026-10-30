//! Scratch disk in the scaler's view: a request that books disk is covered by a host that
//! books enough, and by a machine whose offer has it, and by nothing else. Time is an
//! injected clock; nothing sleeps.
//!
//! In production only agent pools and static host files can book disk today, and the
//! registry refuses a request for disk in an elastic pool before the scaler sees it, so
//! these tests pin the scaler's own rule (defence in depth) for a state not yet reachable.

use std::collections::BTreeMap;
use std::time::Duration;

use js_registry::Waiting;
use js_types::Shape;

use js_scaler::Records;

use crate::fakes::{live, request, rig, view};

const GIB: u64 = 1 << 30;

/// A request like [`request`] that books `disk` bytes of scratch disk.
fn wants(id: &str, disk: u64) -> Waiting {
    let mut waiting = request(id);
    waiting.demand.resources = waiting.demand.resources.with_disk_bytes(disk);
    waiting
}

/// A free live host of the pool that the pool books `disk` bytes of scratch disk for.
fn host(name: &str, disk: u64) -> js_registry::HostView {
    let mut host = view(name);
    let booked = host.shape.resources().clone().with_disk_bytes(disk);
    host.shape = Shape::new(booked, BTreeMap::new());
    host
}

/// How many launches the log shows: the calls that acquire a machine.
fn launches(log: &[String]) -> usize {
    log.iter()
        .filter(|entry| entry.starts_with("acquire:"))
        .count()
}

/// Asserts that a request for more disk than every offer has launches nothing, on the
/// first tick and on the ones after it, while the same request for an amount an offer has
/// launches once. It catches a scaler that launches for a request no machine could serve
/// (a launch loop that ends only at the pool's cap), and a control that proves the
/// request is otherwise launchable.
#[tokio::test]
async fn a_request_for_more_disk_than_every_offer_has_launches_nothing() {
    let big = rig(Records::default(), live());
    big.world().waiting = vec![wants("big", 512 * GIB + 1)];
    for _ in 0..4 {
        big.scaler.tick().await;
        big.advance(Duration::from_secs(5));
    }
    assert_eq!(launches(&big.log()), 0, "{:?}", big.log());
    assert!(big.scaler.state().records.instances.is_empty());

    let fits = rig(Records::default(), live());
    fits.world().waiting = vec![wants("fits", 512 * GIB)];
    fits.scaler.tick().await;
    assert_eq!(launches(&fits.log()), 1, "{:?}", fits.log());
}

/// Asserts that a free host the pool books enough scratch disk for covers a request for
/// it (nothing is launched), and one that books less does not (a machine is launched).
/// It catches a host reported with unbounded disk, which covers every request and leaves
/// one the scheduler refuses waiting with nothing launched for it.
#[tokio::test]
async fn a_host_covers_a_request_only_if_it_books_the_disk() {
    let roomy = rig(Records::default(), live());
    roomy.world().hosts = vec![host("h-roomy", 20 * GIB)];
    roomy.world().waiting = vec![wants("r1", 10 * GIB)];
    roomy.scaler.tick().await;
    assert_eq!(launches(&roomy.log()), 0, "{:?}", roomy.log());

    let small = rig(Records::default(), live());
    small.world().hosts = vec![host("h-small", 5 * GIB)];
    small.world().waiting = vec![wants("r2", 10 * GIB)];
    small.scaler.tick().await;
    assert_eq!(launches(&small.log()), 1, "{:?}", small.log());

    let none = rig(Records::default(), live());
    none.world().hosts = vec![host("h-none", 0)];
    none.world().waiting = vec![wants("r3", 1)];
    none.scaler.tick().await;
    assert_eq!(launches(&none.log()), 1, "{:?}", none.log());
}
