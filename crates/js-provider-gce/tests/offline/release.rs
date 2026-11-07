//! What a release does: it deletes an instance only when it is ours and reports what a read
//! of it found afterwards.

use std::sync::Arc;

use js_elastic::{
    Acquired, CapacityProvider, Claim, Held, Hello, HostRef, HostStatus, Observed, Released,
};
use js_provider_gce::{
    AcceleratorTypeInfo, ApiError, GceApi, GceProvider, InsertRequest, Instance, InstanceRef,
    InstanceStatus, ListFilter, MachineTypeInfo, ManualClock, Method, Operation, OperationRef,
    Page, Project, QuotaMetric, Region, SkuPrice, Zone,
};

use crate::rig::{
    Account, Rig, SCHEDULER, ask, host, label, live, marks, name_of, rig, row, settings, stranger,
    trace_everything,
};

/// Launches the machine of `acquire` on the rig.
fn launch(rig: &Rig, acquire: &str) -> Held {
    match rig.provider.acquire(&ask(acquire)) {
        Acquired::Accepted { held, .. } => held,
        other => panic!("not accepted: {other:?}"),
    }
}

/// A project whose delete finishes at once: the instance is `TERMINATED` by the time it is
/// read back. Every other call goes to the account.
struct Finishing(Arc<Account>);

impl GceApi for Finishing {
    fn machine_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<MachineTypeInfo>, ApiError> {
        self.0.machine_types(project, zones)
    }

    fn accelerator_types(
        &self,
        project: &Project,
        zones: &[Zone],
    ) -> Result<Vec<AcceleratorTypeInfo>, ApiError> {
        self.0.accelerator_types(project, zones)
    }

    fn prices(&self, region: &Region) -> Result<Vec<SkuPrice>, ApiError> {
        self.0.prices(region)
    }

    fn region_quotas(
        &self,
        project: &Project,
        region: &Region,
    ) -> Result<Vec<QuotaMetric>, ApiError> {
        self.0.region_quotas(project, region)
    }

    fn insert_instance(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        self.0.insert_instance(request)
    }

    fn get_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.0.get_instance(instance)
    }

    fn delete_instance(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        let operation = self.0.delete_instance(instance)?;
        operation.target_id.iter().for_each(|target| {
            self.0.set_status(target, InstanceStatus::Terminated);
        });
        Ok(operation)
    }

    fn list_instances(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        self.0.list_instances(filter)
    }

    fn get_operation(&self, operation: &OperationRef) -> Result<Operation, ApiError> {
        self.0.get_operation(operation)
    }
}

/// A rig whose deletes finish at once, as [`Finishing`] says.
fn rig_finishing(max_machines: usize) -> Rig {
    trace_everything();
    let account = Arc::new(Account::new());
    let clock = Arc::new(ManualClock::new());
    let provider = GceProvider::new(
        Arc::new(Finishing(Arc::clone(&account))),
        clock.clone(),
        settings(),
        live(max_machines),
    );
    Rig {
        account,
        clock,
        provider,
    }
}

/// Asserts what a release reads back: an instance of ours that is up is deleted and reported
/// as terminating while the cloud shows it stopping, as a later read of it agrees, with one
/// delete sent and no more; one the cloud already shows terminated is reported as
/// terminated, not as gone; and the supply no longer counts either. It catches a release
/// that reports from the delete call alone, and one that reads a `TERMINATED` instance as
/// gone, which the cloud still holds.
#[test]
fn release_reports_what_it_read_back() {
    let rig = rig(live(2));
    let held = launch(&rig, "a1");
    assert_eq!(rig.provider.capacity().supplied, 1);
    assert_eq!(
        rig.provider.release(&held.id),
        Released::Terminating(HostStatus::Terminating)
    );
    assert_eq!(rig.account.deletes(), 1);
    assert_eq!(
        rig.provider.observe(&held.id),
        Observed::Present(HostStatus::Terminating)
    );
    assert_eq!(rig.provider.capacity().supplied, 0);
    assert_eq!(rig.account.instances()[0].status, InstanceStatus::Stopping);

    let finishing = rig_finishing(2);
    let done = launch(&finishing, "a1");
    assert_eq!(
        finishing.provider.release(&done.id),
        Released::Terminating(HostStatus::Terminated)
    );
    assert_eq!(finishing.account.deletes(), 1);
    assert_eq!(
        finishing.account.instances()[0].status,
        InstanceStatus::Terminated
    );
    assert_eq!(
        finishing.provider.observe(&done.id),
        Observed::Present(HostStatus::Terminated)
    );
    assert_eq!(finishing.provider.capacity().supplied, 0);
}

/// Asserts that an instance the cloud says it does not know is released as gone with no
/// delete sent, and that one the cloud stops knowing right after the delete is gone too,
/// the machine being forgotten: it is no longer supplied and a hello by its name is
/// matched to nothing. It catches "gone" read from anything but a `notFound` answer.
#[test]
fn a_release_is_gone_only_on_a_not_found_answer() {
    let rig = rig(live(3));
    let before = launch(&rig, "a1");
    let instance = rig.account.instances().remove(0);
    rig.account.remove_instance(&instance.id);
    assert_eq!(rig.provider.release(&before.id), Released::Gone);
    assert_eq!(rig.account.deletes(), 0);
    assert!(!rig.account.methods().contains(&Method::DeleteInstance));
    assert_eq!(rig.provider.capacity().supplied, 0);

    let after = launch(&rig, "a2");
    rig.account
        .fail_read_after_next_delete(ApiError::answered("notFound"));
    assert_eq!(rig.provider.release(&after.id), Released::Gone);
    assert_eq!(rig.account.deletes(), 1);
    assert_eq!(rig.provider.capacity().supplied, 0);
    let hello = Hello {
        name: after.name.clone().unwrap(),
        instance_id: None,
        identity: None,
    };
    assert_eq!(rig.provider.verify_instance(&hello), None);
}

/// Asserts that a release that gets no answer is `Unknown` and never `Gone`, at the first
/// read (throttled), at the delete (a timeout, and a server fault), and at the read after
/// the delete (throttled, timed out), that the machine stays supplied after each, and that
/// a delete whose answer was lost, which took effect, is `Unknown` too and is seen as
/// terminating by the next read. It catches a failure read as "gone" or as terminated.
#[test]
fn a_release_that_gets_no_answer_is_unknown_and_keeps_the_machine() {
    let rig = rig(live(4));
    let held = launch(&rig, "a1");
    rig.account
        .fail_next(Method::GetInstance, ApiError::answered("rateLimitExceeded"));
    assert_eq!(rig.provider.release(&held.id), Released::Unknown);
    assert_eq!(rig.account.deletes(), 0);

    for error in [ApiError::Timeout, ApiError::answered("backendError")] {
        rig.account.fail_next(Method::DeleteInstance, error);
        assert_eq!(rig.provider.release(&held.id), Released::Unknown);
        assert_eq!(rig.account.deletes(), 0);
        assert_eq!(
            rig.account.instances()[0].status,
            InstanceStatus::Provisioning
        );
        assert_eq!(rig.provider.capacity().supplied, 1);
    }

    rig.account
        .fail_read_after_next_delete(ApiError::answered("rateLimitExceeded"));
    assert_eq!(rig.provider.release(&held.id), Released::Unknown);
    assert_eq!(rig.account.deletes(), 1);
    assert_eq!(rig.provider.capacity().supplied, 1);

    let timed_out = launch(&rig, "a2");
    rig.account.fail_read_after_next_delete(ApiError::Timeout);
    assert_eq!(rig.provider.release(&timed_out.id), Released::Unknown);
    assert_eq!(rig.account.deletes(), 2);
    assert_eq!(rig.provider.capacity().supplied, 2);

    let lost = launch(&rig, "a3");
    rig.account
        .lose_after_next(Method::DeleteInstance, ApiError::Timeout);
    assert_eq!(rig.provider.release(&lost.id), Released::Unknown);
    assert_eq!(rig.account.deletes(), 3);
    assert_eq!(
        rig.provider.observe(&lost.id),
        Observed::Present(HostStatus::Terminating)
    );
}

/// Asserts that a release of an instance that is not entirely ours is refused with the
/// claim its marks make and nothing is deleted: one with no marks and one of another
/// scheduler are foreign, one that carries the scheduler label and no acquire label or
/// name is a suspect. It catches a delete of an instance the provider cannot prove it
/// launched.
#[test]
fn a_release_of_what_is_not_ours_sends_no_delete() {
    let rig = rig(live(1));
    let loose = stranger(1);
    let half = stranger(2);
    let other = stranger(3);
    rig.account
        .add_instance(row(&loose, InstanceStatus::Running, "plain-vm", Vec::new()));
    rig.account.add_instance(row(
        &half,
        InstanceStatus::Running,
        "half-vm",
        vec![label("js-scheduler", SCHEDULER)],
    ));
    rig.account.add_instance(row(
        &other,
        InstanceStatus::Running,
        &name_of("sched-two", "a9"),
        marks("sched-two", "a9"),
    ));
    assert_eq!(
        rig.provider.release(&host(&loose)),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(
        rig.provider.release(&host(&half)),
        Released::Refused(Claim::Suspect)
    );
    assert_eq!(
        rig.provider.release(&host(&other)),
        Released::Refused(Claim::Foreign)
    );
    assert_eq!(rig.account.deletes(), 0);
    assert!(!rig.account.methods().contains(&Method::DeleteInstance));
    assert_eq!(rig.account.instances()[1].status, InstanceStatus::Running);
}

/// Asserts that text which is not a reference of the form `<zone>/<id>` is released as
/// foreign with nothing asked of the cloud, not even a read: no slash, an id with a
/// leading zero, a bad zone, and nothing at all. It catches a release that sends a call
/// for a reference that is no instance of this cloud, or answers it as gone.
#[test]
fn a_reference_that_is_not_a_zone_and_an_id_is_refused_with_no_call() {
    let rig = rig(live(1));
    for text in [
        "not an id",
        "42",
        "region-one-a/042",
        "Bad/42",
        "region-one-a/",
        "",
    ] {
        assert_eq!(
            rig.provider.release(&HostRef(text.to_owned())),
            Released::Refused(Claim::Foreign),
            "{text:?}"
        );
    }
    assert!(rig.account.calls().is_empty());
}
