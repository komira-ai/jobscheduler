//! Plan mode, proved by a cloud that records every call: a pool that is only planning holds
//! the read half of its provider, and through it no `acquire` or `release` is ever made.

mod common;

use common::{cloud, launch, request};
use js_elastic::{ElasticProvider, HostRef, ReadHandle};
use js_elastic_fake::{Billing, Call};

/// What code that only plans may do. It takes the read half, so it cannot be written to
/// make a machine: there is no `acquire` on the type to call.
fn plan(handle: &ReadHandle) -> (usize, usize, usize) {
    let offers = handle.catalog().unwrap().len();
    let held = handle.list().unwrap().len();
    let checks = handle.checks().len();
    handle.host_prep(&handle.catalog().unwrap()[0]);
    handle.payload_limit();
    handle.unresolved();
    handle.is_gone(&HostRef("i".to_owned()));
    (offers, held, checks)
}

/// Asserts that every read a plan makes through the cloud's read handle is recorded as a
/// read, that none of them is an `acquire` or a `release`, that the machines the cloud holds
/// are the ones it held before, and that the same cloud, asked through its full handle,
/// does record a purchase (so the zero above is a measurement and not a blind counter). It
/// catches a read that buys, and a recorder that cannot see a purchase.
#[test]
fn a_plan_made_through_the_read_handle_makes_zero_create_calls() {
    let cloud = cloud(Billing::Lambda);
    launch(&cloud, "existing");
    let before = cloud.count(Call::Acquire);
    assert_eq!(
        before, 1,
        "the recorder sees a purchase through the full handle"
    );

    let handle = cloud.read_handle();
    assert_eq!(plan(&handle), (1, 1, 0));
    assert_eq!(plan(&handle.clone()), (1, 1, 0));

    assert_eq!(cloud.count(Call::Acquire), before);
    assert_eq!(cloud.count(Call::Release), 0);
    assert_eq!(cloud.launched(), 1);
    let reads = [
        Call::Catalog,
        Call::List,
        Call::Checks,
        Call::Unresolved,
        Call::IsGone,
    ];
    assert!(
        cloud
            .calls()
            .iter()
            .skip(1)
            .all(|call| reads.contains(call)),
        "{:?}",
        cloud.calls()
    );
    cloud.acquire(&request("another"));
    assert_eq!(cloud.count(Call::Acquire), before + 1);
}
