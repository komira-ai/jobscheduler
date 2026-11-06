//! What a release does: it terminates an instance only when it is ours and reports what a
//! read of it found afterwards.

use js_elastic::{
    Acquired, CapacityProvider, Claim, Held, HostRef, HostStatus, Observed, Released,
};
use js_provider_ec2::{Action, ApiError, InstanceState};

use crate::rig::{Rig, SCHEDULER, ask, host, live, marks, rig, row, stranger, tag};

/// Launches the machine of `acquire` on the rig.
fn launch(rig: &Rig, acquire: &str) -> Held {
    match rig.provider.acquire(&ask(acquire)) {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

/// Asserts what a release reads back: an instance of ours that is up is terminated and
/// reported as terminating, as a later read of it agrees, with one terminate sent; the
/// supply no longer counts it. It catches a release that reports from the terminate call
/// alone.
#[test]
fn release_reports_what_it_read_back() {
    let rig = rig(live(2));
    let held = launch(&rig, "a1");
    assert_eq!(rig.provider.capacity().supplied, 1);
    assert_eq!(
        rig.provider.release(&held.id),
        Released::Terminating(HostStatus::Terminating)
    );
    assert_eq!(rig.account.terminates(), 1);
    assert_eq!(
        rig.provider.observe(&held.id),
        Observed::Present(HostStatus::Terminating)
    );
    assert_eq!(rig.provider.capacity().supplied, 0);
    assert_eq!(
        rig.account.instances()[0].state,
        InstanceState::ShuttingDown
    );
}

/// Asserts that an instance the cloud says it does not know is released as gone with no
/// terminate sent, and that one the cloud stops knowing right after the terminate is gone
/// too, the machine being forgotten: it is no longer supplied and a hello by its name is
/// matched to nothing. It catches "gone" read from anything but a not-found answer.
#[test]
fn a_release_is_gone_only_on_a_not_found_answer() {
    let rig = rig(live(3));
    let before = launch(&rig, "a1");
    rig.account
        .remove_instance(&js_provider_ec2::InstanceId::new(before.id.as_str()).unwrap());
    assert_eq!(rig.provider.release(&before.id), Released::Gone);
    assert_eq!(rig.account.terminates(), 0);
    assert_eq!(rig.provider.capacity().supplied, 0);

    let after = launch(&rig, "a2");
    rig.account.forget_after_terminate();
    assert_eq!(rig.provider.release(&after.id), Released::Gone);
    assert_eq!(rig.account.terminates(), 1);
    assert_eq!(rig.provider.capacity().supplied, 0);
    let hello = js_elastic::Hello {
        name: after.name.clone().unwrap(),
        instance_id: None,
        identity: None,
    };
    assert_eq!(rig.provider.verify_instance(&hello), None);
}

/// Asserts that a release that gets no answer, at the first read, at the terminate or at the
/// read after it, is `Unknown`, that the machine stays supplied, and that a terminate whose
/// answer was lost, which took effect, is `Unknown` too and is seen as terminating by the next
/// read. It catches a failure read as "gone" or as terminated.
#[test]
fn a_release_that_gets_no_answer_is_unknown_and_keeps_the_machine() {
    let rig = rig(live(4));
    let held = launch(&rig, "a1");
    let throttle = ApiError::answered("RequestLimitExceeded");
    rig.account.fail_next(Action::DescribeInstances, throttle);
    assert_eq!(rig.provider.release(&held.id), Released::Unknown);
    assert_eq!(rig.account.terminates(), 0);

    rig.account
        .fail_next(Action::TerminateInstances, ApiError::Timeout);
    assert_eq!(rig.provider.release(&held.id), Released::Unknown);
    assert_eq!(rig.account.terminates(), 0);
    assert_eq!(rig.account.instances()[0].state, InstanceState::Pending);

    rig.account
        .fail_read_after_next_terminate(ApiError::Timeout);
    assert_eq!(rig.provider.release(&held.id), Released::Unknown);
    assert_eq!(rig.account.terminates(), 1);
    assert_eq!(rig.provider.capacity().supplied, 1);

    let lost = launch(&rig, "a2");
    rig.account
        .lose_after_next(Action::TerminateInstances, ApiError::Timeout);
    assert_eq!(rig.provider.release(&lost.id), Released::Unknown);
    assert_eq!(rig.account.terminates(), 2);
    assert_eq!(
        rig.provider.observe(&lost.id),
        Observed::Present(HostStatus::Terminating)
    );
}

/// Asserts that a release of an instance that is not entirely ours is refused with the claim
/// its marks make and nothing is terminated: one with no marks is foreign, one that carries
/// the scheduler tag and no acquire tag is a suspect; and that text which is no instance id
/// is foreign with nothing asked of the cloud. It catches a terminate of an instance the
/// provider cannot prove it launched.
#[test]
fn a_release_of_what_is_not_ours_sends_no_terminate() {
    let rig = rig(live(1));
    let loose = stranger(1);
    let half = stranger(2);
    rig.account
        .add_instance(row(&loose, InstanceState::Running, Vec::new()));
    rig.account.add_instance(row(
        &half,
        InstanceState::Running,
        vec![tag("js-scheduler", SCHEDULER)],
    ));
    assert_eq!(
        rig.provider.release(&host(&loose)),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(
        rig.provider.release(&host(&half)),
        Released::Refused(Claim::Suspect)
    );
    let reads = rig.account.actions().len();
    assert_eq!(
        rig.provider.release(&HostRef("not an id".to_owned())),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(rig.account.actions().len(), reads);
    assert_eq!(rig.account.terminates(), 0);
    let other = marks("sched-two", "a9");
    rig.account
        .add_instance(row(&stranger(3), InstanceState::Running, other));
    assert_eq!(
        rig.provider.release(&host(&stranger(3))),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(rig.account.terminates(), 0);
}
