//! What a read of one machine by id says, and what it tells the provider's records.

use js_elastic::{Acquired, CapacityProvider, HostRef, HostStatus, Observed};
use js_provider_ec2::{Action, ApiError, InstanceId, InstanceState};

use crate::rig::{SCHEDULER, ask, host, live, rig, row, stranger, tag};

/// Asserts that every state an instance can be in reads as its status: pending as booting,
/// running as active, shutting-down and stopping as terminating, terminated and stopped as
/// terminated, each present; and that a machine the cloud does not know is absent.
#[test]
fn observe_maps_instance_states() {
    let rig = rig(live(1));
    let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask("a1")) else {
        panic!("not accepted");
    };
    let machine = InstanceId::new(held.id.as_str()).unwrap();
    let states = [
        (InstanceState::Pending, HostStatus::Booting),
        (InstanceState::Running, HostStatus::Active),
        (InstanceState::ShuttingDown, HostStatus::Terminating),
        (InstanceState::Terminated, HostStatus::Terminated),
        (InstanceState::Stopping, HostStatus::Terminating),
        (InstanceState::Stopped, HostStatus::Terminated),
    ];
    for (state, status) in states {
        rig.account.set_state(&machine, state);
        assert_eq!(rig.provider.observe(&held.id), Observed::Present(status));
    }
    rig.account.remove_instance(&machine);
    assert_eq!(rig.provider.observe(&held.id), Observed::Absent);
    assert!(rig.provider.is_gone(&held.id));
}

/// Asserts that a read which fails in any way but a not-found answer is `Unknown`, never
/// absent: a throttle, a denial, a timeout and a transport error; and that text which is no
/// instance id is `Unknown` with nothing asked of the cloud.
#[test]
fn a_read_that_fails_is_unknown_never_absent() {
    let rig = rig(live(1));
    let kept = host(&stranger(1));
    rig.account
        .add_instance(row(&stranger(1), InstanceState::Running, Vec::new()));
    for error in [
        ApiError::answered("RequestLimitExceeded"),
        ApiError::answered("UnauthorizedOperation"),
        ApiError::Timeout,
        ApiError::Transport("reset".to_owned()),
        ApiError::Incomplete,
    ] {
        rig.account.fail_next(Action::DescribeInstances, error);
        assert_eq!(rig.provider.observe(&kept), Observed::Unknown);
        assert!(!rig.provider.is_gone(&kept));
    }
    let reads = rig.account.actions().len();
    assert_eq!(
        rig.provider.observe(&HostRef("not an id".to_owned())),
        Observed::Unknown
    );
    assert_eq!(rig.account.actions().len(), reads);
}

/// Asserts what a read tells the records: a terminated machine is present and terminated and
/// no longer supplied; one the cloud says is gone is forgotten, so a hello by its name is
/// matched to nothing; and an instance with some of our marks, once listed, has the state a
/// read finds it in counted (a terminated one no longer counts toward the cap).
#[test]
fn a_read_by_id_updates_what_the_provider_holds() {
    let rig = rig(live(3));
    let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask("a1")) else {
        panic!("not accepted");
    };
    let machine = InstanceId::new(held.id.as_str()).unwrap();
    let hello = js_elastic::Hello {
        name: held.name.clone().unwrap(),
        instance_id: Some(held.id.to_string()),
        identity: None,
    };
    rig.account.set_state(&machine, InstanceState::Terminated);
    assert_eq!(
        rig.provider.observe(&held.id),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(rig.provider.capacity().supplied, 0);
    assert_eq!(rig.provider.verify_instance(&hello), Some(held.id.clone()));
    rig.account.remove_instance(&machine);
    assert_eq!(rig.provider.observe(&held.id), Observed::Absent);
    assert_eq!(rig.provider.verify_instance(&hello), None);

    let suspect = stranger(4);
    rig.account.add_instance(row(
        &suspect,
        InstanceState::Running,
        vec![tag("js-scheduler", SCHEDULER)],
    ));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity().can_add, Some(2));
    rig.account.set_state(
        &InstanceId::new(&suspect).unwrap(),
        InstanceState::Terminated,
    );
    assert_eq!(
        rig.provider.observe(&host(&suspect)),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(rig.provider.capacity().can_add, Some(3));
}
