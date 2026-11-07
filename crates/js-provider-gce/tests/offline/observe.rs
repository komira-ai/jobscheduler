//! What a read of one machine by reference says, and what it tells the provider's records.

use js_elastic::{Acquired, Capacity, CapacityProvider, Hello, HostRef, HostStatus, Observed};
use js_provider_gce::{ApiError, InstanceId, InstanceStatus, Method, parse_host_ref};

use crate::rig::{SCHEDULER, ask, host, label, live, rig, row, stranger};

/// Asserts that every status an instance can be in reads as its status: provisioning,
/// staging and repairing as booting, running as active, stopping and suspending as
/// terminating, terminated, stopped and suspended as terminated, each present; that a
/// status this release does not know is the scheduler's unknown, present, with the text;
/// and that a machine the cloud does not know is absent. It catches a status mapped to the
/// wrong word, and an unknown status read as gone.
#[test]
fn observe_maps_instance_states() {
    let rig = rig(live(1));
    let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask("a1")) else {
        panic!("not accepted");
    };
    let machine = rig.account.instances().remove(0).id;
    let states = [
        (InstanceStatus::Provisioning, HostStatus::Booting),
        (InstanceStatus::Staging, HostStatus::Booting),
        (InstanceStatus::Repairing, HostStatus::Booting),
        (InstanceStatus::Running, HostStatus::Active),
        (InstanceStatus::Stopping, HostStatus::Terminating),
        (InstanceStatus::Suspending, HostStatus::Terminating),
        (InstanceStatus::Terminated, HostStatus::Terminated),
        (InstanceStatus::Stopped, HostStatus::Terminated),
        (InstanceStatus::Suspended, HostStatus::Terminated),
        (
            InstanceStatus::Other("HIBERNATING".to_owned()),
            HostStatus::Unknown("HIBERNATING".to_owned()),
        ),
    ];
    for (status, expected) in states {
        rig.account.set_status(&machine, status.clone());
        assert_eq!(
            rig.provider.observe(&held.id),
            Observed::Present(expected),
            "{status:?}"
        );
    }
    rig.account.remove_instance(&machine);
    assert_eq!(rig.provider.observe(&held.id), Observed::Absent);
    assert!(rig.provider.is_gone(&held.id));
}

/// Asserts that only an answer of `notFound` makes a machine absent, however the answer
/// came, and that a machine the cloud holds in any status is not gone. It catches a read
/// that takes the status of a stopped machine for its absence.
#[test]
fn only_a_not_found_answer_is_absent() {
    let rig = rig(live(1));
    let kept = host(&stranger(1));
    rig.account.add_instance(row(
        &stranger(1),
        InstanceStatus::Terminated,
        "plain-vm",
        Vec::new(),
    ));
    assert_eq!(
        rig.provider.observe(&kept),
        Observed::Present(HostStatus::Terminated)
    );
    assert!(!rig.provider.is_gone(&kept));
    rig.account
        .fail_next(Method::GetInstance, ApiError::answered("notFound"));
    assert_eq!(rig.provider.observe(&kept), Observed::Absent);
    assert!(rig.provider.is_gone(&host(&stranger(2))));
}

/// Asserts that a read which fails in any way but a not-found answer is `Unknown`, never
/// absent: a throttle, a denial, a refusal, a timeout, a missing credential, a transport
/// error, an answer that cannot be read and an incomplete one; and that text which is no
/// reference of the cloud is `Unknown` with nothing asked of the cloud. It catches a failure
/// read as "gone".
#[test]
fn a_read_that_fails_is_unknown_never_absent() {
    let rig = rig(live(1));
    let kept = host(&stranger(1));
    rig.account.add_instance(row(
        &stranger(1),
        InstanceStatus::Running,
        "plain-vm",
        Vec::new(),
    ));
    for error in [
        ApiError::answered("rateLimitExceeded"),
        ApiError::answered("forbidden"),
        ApiError::answered("backendError"),
        ApiError::Timeout,
        ApiError::NoCredential,
        ApiError::Transport("reset".to_owned()),
        ApiError::Unreadable("no body".to_owned()),
        ApiError::Incomplete,
    ] {
        rig.account.fail_next(Method::GetInstance, error.clone());
        assert_eq!(rig.provider.observe(&kept), Observed::Unknown, "{error}");
        rig.account.fail_next(Method::GetInstance, error.clone());
        assert!(!rig.provider.is_gone(&kept), "{error}");
    }
    let reads = rig.account.calls().len();
    for text in [
        "not an id",
        "region-one-a",
        "region-one-a/012",
        "Bad/12",
        "",
    ] {
        assert_eq!(
            rig.provider.observe(&HostRef(text.to_owned())),
            Observed::Unknown,
            "{text:?}"
        );
    }
    assert_eq!(rig.account.calls().len(), reads);
}

/// Asserts that a reference of this cloud which names an instance of another zone is read in
/// that zone and so is absent, not read as the instance of the same id in the configured
/// zone. It catches a read that drops the zone of the reference.
#[test]
fn a_reference_is_read_in_its_own_zone() {
    let rig = rig(live(1));
    rig.account.add_instance(row(
        &stranger(1),
        InstanceStatus::Running,
        "plain-vm",
        Vec::new(),
    ));
    let elsewhere = HostRef(
        host(&stranger(1))
            .as_str()
            .replace("region-one-a", "region-one-b"),
    );
    assert!(parse_host_ref(&elsewhere).is_some());
    assert_eq!(rig.provider.observe(&elsewhere), Observed::Absent);
    assert_eq!(
        rig.provider.observe(&host(&stranger(1))),
        Observed::Present(HostStatus::Active)
    );
}

/// Asserts what a read tells the records: a terminated machine is present and terminated and
/// no longer supplied, yet a hello by its name and instance id is still matched to it; one
/// the cloud says is gone is forgotten, so a hello by its name is matched to nothing; and an
/// instance with some of our marks, once listed, has the state a read finds it in counted
/// (a terminated one no longer counts toward the cap). It catches a read that leaves a
/// stale record.
#[test]
fn a_read_by_reference_updates_what_the_provider_holds() {
    let rig = rig(live(3));
    let Acquired::Accepted { held, .. } = rig.provider.acquire(&ask("a1")) else {
        panic!("not accepted");
    };
    let machine = rig.account.instances().remove(0).id;
    let hello = Hello {
        name: held.name.clone().unwrap(),
        instance_id: Some(held.id.to_string()),
        identity: None,
    };
    rig.account.set_status(&machine, InstanceStatus::Terminated);
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
        InstanceStatus::Running,
        "half-vm",
        vec![label("js-scheduler", SCHEDULER)],
    ));
    rig.provider.list().unwrap();
    assert_eq!(rig.provider.capacity().can_add, Some(2));
    rig.account.set_status(
        &InstanceId::new(&suspect).unwrap(),
        InstanceStatus::Terminated,
    );
    assert_eq!(
        rig.provider.observe(&host(&suspect)),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(
        rig.provider.capacity(),
        Capacity {
            supplied: 0,
            can_add: Some(3)
        }
    );
}
