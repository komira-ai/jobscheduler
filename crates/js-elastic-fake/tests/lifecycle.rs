//! The fake's answers to the new calls through lifecycles the conformance suite does not
//! walk: a machine dropped and its acquire retried, an acquire launched again after a
//! release, reclaim notices on several streams, the quota at and below what runs, offers
//! with no GPU or no capacity, and the order of scripted failures.

mod common;

use std::collections::BTreeSet;

use common::{PRICE, START, cloud, launch, offer, request};
use js_elastic::{
    AcquireId, Acquired, Capacity, CapacityProvider, Hello, HostName, HostRef, Observed,
    ProviderEvent, RefusalCode, Released, SchedulerId,
};
use js_elastic_fake::{Billing, Call, ElasticFake, FakeClock, Knobs, Step};

fn accepted(answer: &Acquired) -> HostRef {
    match answer {
        Acquired::Accepted { held, .. } => held.id.clone(),
        other => panic!("expected an accepted launch, got {other:?}"),
    }
}

fn with_quota(quota: usize) -> ElasticFake {
    let cloud = cloud(Billing::Lambda);
    cloud.set_knobs(Knobs {
        quota: Some(quota),
        ..cloud.knobs()
    });
    cloud
}

fn hello(name: &str, instance: Option<&HostRef>) -> Hello {
    Hello {
        name: name.to_owned(),
        instance_id: instance.map(ToString::to_string),
    }
}

/// Asserts that a retry of the acquire of a dropped machine does not answer the dropped
/// machine: it launches a new one, which a read by id finds and which runs. It catches an
/// acquire that answers a machine the cloud says does not exist.
#[test]
fn a_retry_after_a_drop_launches_a_machine_that_exists() {
    let cloud = cloud(Billing::Lambda);
    let dropped = launch(&cloud, "a1");
    cloud.drop_machine(&dropped);
    let retried = accepted(&cloud.acquire(&request("a1")));
    assert_ne!(retried, dropped);
    assert!(matches!(cloud.observe(&retried), Observed::Present(_)));
    assert_eq!(cloud.observe(&dropped), Observed::Absent);
    assert_eq!(cloud.running(), [retried]);
}

/// Asserts how a hello is matched when one name has several machines: after a release
/// and a new launch of the same acquire, a hello by name alone is the new machine and one
/// with the released machine's id is that machine; with two running machines of one name
/// a hello by name alone is neither. It catches a match that takes the first machine of
/// a name.
#[test]
fn a_hello_by_name_alone_needs_one_running_machine_of_that_name() {
    let cloud = cloud(Billing::Lambda);
    let first = launch(&cloud, "a1");
    assert!(matches!(cloud.release(&first), Released::Terminating(_)));
    let second = launch(&cloud, "a1");
    assert_ne!(first, second);
    let name = request("a1").name.to_string();
    assert_eq!(
        cloud.verify_instance(&hello(&name, None)),
        Some(second.clone())
    );
    assert_eq!(
        cloud.verify_instance(&hello(&name, Some(&first))),
        Some(first)
    );
    let mut twin = request("a2");
    twin.name = request("a1").name;
    let third = accepted(&cloud.acquire(&twin));
    assert_eq!(cloud.verify_instance(&hello(&name, None)), None);
    assert_eq!(
        cloud.verify_instance(&hello(&name, Some(&third))),
        Some(third)
    );
}

/// Asserts the reclaim notices: every stream open at a preemption gets it, in the order
/// of the preemptions; a stream taken later sees only later ones; reclaiming a machine
/// the caller did not launch sends nothing; and a stream ends only when the last clone of
/// the cloud is gone. It catches a notice sent to one stream only, out of order, or for
/// another scheduler's machine.
#[test]
fn reclaim_notices_reach_every_open_stream_in_order() {
    let cloud = cloud(Billing::Lambda);
    let first = launch(&cloud, "a1");
    let second = launch(&cloud, "a2");
    let foreign = cloud.add_foreign(None, offer(Some(PRICE)));
    let early = cloud.events().unwrap();
    let both = cloud.events().unwrap();
    cloud.preempt(&first);
    let late = cloud.events().unwrap();
    cloud.preempt(&foreign);
    cloud.preempt(&second);
    assert!(
        cloud.running().is_empty(),
        "the foreign machine was reclaimed"
    );
    let survivor = cloud.clone();
    drop(cloud);
    survivor.preempt(&HostRef("nobody".to_owned()));
    drop(survivor);
    let reclaimed = |id: &HostRef| ProviderEvent::Reclaimed(id.clone());
    let order = [reclaimed(&first), reclaimed(&second)];
    assert_eq!(early.collect::<Vec<_>>(), order);
    assert_eq!(both.collect::<Vec<_>>(), order);
    assert_eq!(late.collect::<Vec<_>>(), [reclaimed(&second)]);
}

/// Asserts the bound at its edges: a quota of zero adds nothing; a quota cut below what
/// runs adds nothing (it does not wrap); a lost acquire that made no machine holds a
/// place until its hold runs out; a preempted machine is not supplied. It catches a bound
/// that goes negative, or that forgets an acquire that may still land.
#[test]
fn the_bound_saturates_and_counts_an_acquire_that_may_land() {
    let none = with_quota(0);
    assert_eq!(
        none.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(0)
        }
    );
    let cloud = with_quota(3);
    let kept = launch(&cloud, "a1");
    let reclaimed = launch(&cloud, "a2");
    cloud.preempt(&reclaimed);
    cloud.script(Call::Acquire, &[Step::Lost { landed: false }]);
    assert_eq!(cloud.acquire(&request("a3")), Acquired::Unknown);
    assert_eq!(
        cloud.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(1)
        }
    );
    cloud.clock().advance(cloud.knobs().unknown_hold);
    assert_eq!(cloud.capacity().can_add, Some(2));
    cloud.set_knobs(Knobs {
        quota: Some(0),
        ..cloud.knobs()
    });
    assert_eq!(
        cloud.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(0)
        }
    );
    assert!(matches!(cloud.observe(&kept), Observed::Present(_)));
}

/// Asserts that the capabilities are those of the offers listed, available or not, and
/// that an offer with no GPU adds no class. It catches capabilities read from the offers
/// available now.
#[test]
fn the_capabilities_count_offers_with_no_capacity_and_no_gpu() {
    let mut cpu = offer(None);
    (cpu.id, cpu.gpus, cpu.gpu_class, cpu.available) = ("cpu".to_owned(), 0, None, false);
    cpu.arch = "aarch64".to_owned();
    let cloud = ElasticFake::new(FakeClock::new(START), Billing::Gce, vec![cpu.clone()]);
    let caps = cloud.capabilities();
    assert_eq!(caps.arch, BTreeSet::from(["aarch64".to_owned()]));
    assert!(caps.accelerators.is_empty());
    assert!(caps.covers(&cpu));
}

/// Asserts the order of scripted failures: the step set for the next call of any kind
/// comes before the steps queued for its kind, which follow in turn; an acquire answered
/// without reaching the cloud (one that is deferred, and one answered with a machine it
/// launched before) leaves that step for the next call that does. It catches a step taken
/// by the wrong call, which would fault a different operation than a test meant.
#[test]
fn the_step_for_the_next_call_comes_first_and_waits_for_a_call_that_reaches_the_cloud() {
    let cloud = cloud(Billing::Lambda);
    cloud.script(Call::List, &[Step::Throttled]);
    cloud.script_next(Step::Lost { landed: false });
    let errors: Vec<String> = (0..2)
        .map(|_| cloud.list().unwrap_err().to_string())
        .collect();
    assert_eq!(
        errors,
        ["the list timed out", "the list was throttled (429)"]
    );
    assert!(cloud.list().is_ok());

    launch(&cloud, "a1");
    cloud.script(Call::Acquire, &[Step::Lost { landed: false }]);
    assert_eq!(cloud.acquire(&request("lost")), Acquired::Unknown);
    cloud.script_next(Step::Refuse(RefusalCode::Quota));
    assert!(matches!(
        cloud.acquire(&request("lost")),
        Acquired::Deferred { .. }
    ));
    assert!(matches!(
        cloud.acquire(&request("a1")),
        Acquired::Accepted { .. }
    ));
    let refused = cloud.acquire(&request("a2"));
    assert!(
        matches!(&refused, Acquired::Refused(refusal) if refusal.code == RefusalCode::Quota),
        "{refused:?}"
    );
}

/// Asserts that a hello named with another scheduler's prefix is never matched, even to a
/// machine of the account launched under that name.
#[test]
fn a_hello_for_another_schedulers_machine_is_not_matched() {
    let cloud = cloud(Billing::Lambda);
    let other = SchedulerId::new("another").unwrap();
    let name = HostName::derive(&other, &AcquireId::new("x").unwrap()).to_string();
    let theirs = cloud.add_foreign(Some(&name), offer(Some(PRICE)));
    assert_eq!(cloud.verify_instance(&hello(&name, None)), None);
    assert_eq!(cloud.verify_instance(&hello(&name, Some(&theirs))), None);
}

/// Asserts that a lost acquire under the id of a machine released before it holds its
/// place until its hold runs out: the released machine is not the one it may make. It
/// catches a lost acquire matched to an earlier machine of its id, which would show room
/// the lost launch may still take.
#[test]
fn a_lost_replacement_acquire_keeps_its_place() {
    let cloud = with_quota(2);
    let first = launch(&cloud, "a1");
    assert!(matches!(cloud.release(&first), Released::Terminating(_)));
    cloud.script(Call::Acquire, &[Step::Lost { landed: false }]);
    assert_eq!(cloud.acquire(&request("a1")), Acquired::Unknown);
    assert_eq!(
        cloud.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(1)
        }
    );
    cloud.clock().advance(cloud.knobs().unknown_hold);
    assert_eq!(cloud.capacity().can_add, Some(2));
}

/// Asserts that a lost acquire that landed is the machine it made: once that machine is
/// released, or dropped, the acquire is not counted again beside it. It catches a lost
/// acquire that holds a second place once its own machine ends.
#[test]
fn a_lost_acquire_that_landed_is_not_counted_again_once_its_machine_ends() {
    for dropped in [false, true] {
        let cloud = with_quota(3);
        cloud.script(Call::Acquire, &[Step::Lost { landed: true }]);
        assert_eq!(cloud.acquire(&request("a1")), Acquired::Unknown);
        let landed = cloud.running()[0].clone();
        assert_eq!(
            cloud.capacity(),
            Capacity {
                supplied: 1,
                can_add: Some(2)
            }
        );
        match dropped {
            true => cloud.drop_machine(&landed),
            false => assert!(matches!(cloud.release(&landed), Released::Terminating(_))),
        }
        assert_eq!(
            cloud.capacity(),
            Capacity {
                supplied: 0,
                can_add: Some(3)
            },
            "dropped {dropped}"
        );
    }
}
