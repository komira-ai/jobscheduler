//! What a list returns, and what it tells the provider's records.

use js_elastic::{Acquired, Capacity, CapacityProvider, Held, HostStatus, ProviderError};
use js_provider_ec2::{Action, ApiError, InstanceState};

use crate::rig::{SCHEDULER, ask, host, live, marks, name_of, ours, rig, row, stranger, tag};

fn accepted(answer: Acquired) -> Held {
    match answer {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

/// Asserts that a list holds the instances this scheduler launched that are not terminated,
/// each with its acquire id, name, offer type, zone and status, in the order the cloud gave
/// them; and that instances of another scheduler, instances with no marks, instances with
/// only some of ours and terminated instances are not in it.
#[test]
fn a_list_holds_the_machines_this_scheduler_launched() {
    let rig = rig(live(5));
    let first = accepted(rig.provider.acquire(&ask("a1")));
    rig.account
        .add_instance(ours(&stranger(1), InstanceState::Running, "a2"));
    rig.account
        .add_instance(ours(&stranger(2), InstanceState::Terminated, "a3"));
    rig.account.add_instance(row(
        &stranger(3),
        InstanceState::Running,
        marks("sched-two", "a4"),
    ));
    rig.account.add_instance(row(
        &stranger(4),
        InstanceState::Running,
        vec![tag("js-scheduler", SCHEDULER)],
    ));
    let listed = rig.provider.list().unwrap();
    assert_eq!(
        listed,
        [
            Held {
                id: first.id.clone(),
                acquire: "a1".to_owned(),
                name: Some(name_of(SCHEDULER, "a1")),
                offer: "m9.large".to_owned(),
                zone: Some("zone-a1".to_owned()),
                status: HostStatus::Booting,
                ends_unix_seconds: None,
            },
            Held {
                id: host(&stranger(1)),
                acquire: "a2".to_owned(),
                name: Some(name_of(SCHEDULER, "a2")),
                offer: "m9.large".to_owned(),
                zone: Some("zone-a1".to_owned()),
                status: HostStatus::Active,
                ends_unix_seconds: None,
            },
        ]
    );
}

/// Asserts that a list whose walk fails on a page is an error and not a shorter list, with
/// the words of the failure, and that the provider's count does not change; and that so is a
/// list that fails outright.
#[test]
fn a_page_failure_in_list_is_an_error() {
    let rig = rig(live(5));
    accepted(rig.provider.acquire(&ask("a1")));
    accepted(rig.provider.acquire(&ask("a2")));
    rig.account.paginate(1).fail_page(2);
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new("the answer is not complete"))
    );
    assert_eq!(rig.provider.list().unwrap().len(), 2);
    rig.account.fail_next(
        Action::DescribeInstances,
        ApiError::Transport("reset".to_owned()),
    );
    assert_eq!(
        rig.provider.list(),
        Err(ProviderError::new("cannot talk to the cloud: reset"))
    );
    assert_eq!(rig.provider.capacity().supplied, 2);
}

/// Asserts that a list which does not show a machine this provider launched yet (a read
/// right after a write can lag it) does not drop it from the count, that a list which shows
/// it is believed from then on, and that a list that no longer shows it drops it. It catches
/// a count that falls when a read lags, which would let the cap be passed.
#[test]
fn a_list_that_lags_a_launch_does_not_drop_it() {
    let rig = rig(live(2));
    let held = accepted(rig.provider.acquire(&ask("a1")));
    let shown = rig.account.instances().remove(0);
    rig.account.remove_instance(&shown.id);
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 1,
            can_add: Some(1)
        }
    );
    rig.account.add_instance(shown.clone());
    assert_eq!(rig.provider.list().unwrap().len(), 1);
    rig.account.remove_instance(&shown.id);
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(rig.provider.capacity().supplied, 0);
    assert_eq!(held.id, shown.id.host_ref());
}

/// Asserts that a list believes the instances it shows over what the provider held: a
/// machine listed terminated is not supplied or counted, and one of ours listed again as a
/// suspect (retagged) is counted once, as a suspect.
#[test]
fn a_list_replaces_what_the_provider_held() {
    let rig = rig(live(3));
    let held = accepted(rig.provider.acquire(&ask("a1")));
    let machine = rig.account.instances().remove(0);
    rig.account
        .set_state(&machine.id, InstanceState::Terminated);
    rig.provider.list().unwrap();
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );

    let second = accepted(rig.provider.acquire(&ask("a2")));
    assert_eq!(rig.provider.capacity().supplied, 1);
    let twin = rig.account.instances().remove(1);
    rig.account.remove_instance(&twin.id);
    rig.account.add_instance(js_provider_ec2::Instance {
        tags: vec![tag("js-scheduler", SCHEDULER)],
        ..twin
    });
    assert!(rig.provider.list().unwrap().is_empty());
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(2)
        }
    );
    assert_ne!(held.id, second.id);
}

/// Asserts that a cloud which answers instances whatever tags were asked for does not make
/// a stranger's instance ours: the list holds only ours.
#[test]
fn a_list_ignores_instances_the_cloud_returns_that_are_not_ours() {
    let rig = rig(live(2));
    accepted(rig.provider.acquire(&ask("a1")));
    rig.account.ignore_tag_filters();
    rig.account
        .add_instance(row(&stranger(1), InstanceState::Running, Vec::new()));
    rig.account.add_instance(row(
        &stranger(2),
        InstanceState::Running,
        marks("sched-two", "a9"),
    ));
    let listed = rig.provider.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].acquire, "a1");
    assert_eq!(rig.provider.capacity().can_add, Some(1));
    assert!(matches!(
        rig.provider.acquire(&ask("a1")),
        Acquired::Accepted { .. }
    ));
}
