//! The in-memory project's launch side: what an insert, a read, a delete and a list do to
//! it, and the failures a test can script.

use js_elastic::{AcquireId, SchedulerId};
use js_provider_gce::{
    ApiError, Call, ErrorReason, FakeGce, GceApi, HostMaintenance, InsertRequest, InstanceKey,
    InstanceName, InstanceRef, InstanceStatus, ListFilter, MachineTypeName, Method, Operation,
    OperationRef, OperationStatus, Payload, Project, ProvisioningModel, RequestId, Scheduling,
};

use crate::rig::{
    CPU_TYPE, OTHER_ZONE, PROJECT, X86_IMAGE, ZONE, id, image, label, network, row, zone,
};

fn project() -> Project {
    Project::new(PROJECT).unwrap()
}

/// An insert of the instance `name` for the acquire `acquire`, with these labels.
fn request(name: &str, acquire: &str, labels: &[(&str, &str)]) -> InsertRequest {
    InsertRequest {
        project: project(),
        zone: zone(ZONE),
        name: InstanceName::new(name).unwrap(),
        machine_type: MachineTypeName(CPU_TYPE.to_owned()),
        scheduling: Scheduling {
            provisioning_model: ProvisioningModel::Standard,
            on_host_maintenance: HostMaintenance::Terminate,
            automatic_restart: false,
            termination_action: None,
            max_run_duration: None,
        },
        image: image(X86_IMAGE),
        disk_size_gb: None,
        accelerators: None,
        labels: labels
            .iter()
            .map(|(key, value)| label(key, value))
            .collect(),
        payload: Payload::new("boot".to_owned()),
        network: network(),
        request_id: RequestId::derive(
            &AcquireId::new(acquire).unwrap(),
            &SchedulerId::new("one").unwrap(),
        ),
    }
}

fn by(instance: InstanceKey) -> InstanceRef {
    InstanceRef {
        project: project(),
        zone: zone(ZONE),
        instance,
    }
}

fn by_name(name: &str) -> InstanceRef {
    by(InstanceKey::Name(InstanceName::new(name).unwrap()))
}

fn by_id(text: &str) -> InstanceRef {
    by(InstanceKey::Id(id(text)))
}

fn filter(labels: &[(&str, &str)], page_token: Option<String>) -> ListFilter {
    ListFilter {
        project: project(),
        labels: labels
            .iter()
            .map(|(key, value)| label(key, value))
            .collect(),
        page_token,
    }
}

/// A fake that holds the instances `vm-0` to `vm-{count - 1}`, all inserted by it.
fn holding(count: usize) -> FakeGce {
    let fake = FakeGce::new();
    for n in 0..count {
        let insert = request(&format!("vm-{n}"), &format!("a{n}"), &[]);
        fake.insert_instance(&insert).unwrap();
    }
    fake
}

/// Asserts that an insert creates one provisioning instance with the name, labels, machine
/// type and zone of the request and the first id, answers a pending operation that names
/// it, that a repeat of the request id answers that same operation and creates nothing,
/// and that once the request ids are forgotten the same id inserts again under a new id.
/// It catches an insert that is not idempotent on its request id, and a fake that never
/// forgets.
#[test]
fn an_insert_creates_one_instance_and_a_repeated_request_id_creates_none() {
    let fake = FakeGce::new();
    let first = request("vm-a", "a1", &[("k", "v")]);
    let operation = fake.insert_instance(&first).unwrap();
    assert_eq!(operation.status, OperationStatus::Pending);
    assert!(!operation.failed());
    assert_eq!(operation.reference.project, project());
    assert_eq!(operation.reference.zone, zone(ZONE));
    let rows = fake.instances();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id("1"));
    assert_eq!(rows[0].name, "vm-a");
    assert_eq!(rows[0].status, InstanceStatus::Provisioning);
    assert_eq!(rows[0].labels, [label("k", "v")]);
    assert_eq!(rows[0].machine_type, MachineTypeName(CPU_TYPE.to_owned()));
    assert_eq!(rows[0].zone, zone(ZONE));
    assert_eq!(operation.target_id, Some(id("1")));
    assert_eq!(fake.insert_instance(&first), Ok(operation.clone()));
    assert_eq!(fake.inserted(), 1);
    assert_eq!(fake.instances().len(), 1);
    assert_eq!(fake.inserts(), [first]);
    fake.forget_request_ids();
    let again = fake.insert_instance(&request("vm-b", "a1", &[])).unwrap();
    assert_ne!(again.reference, operation.reference);
    assert_eq!(again.target_id, Some(id("2")));
    assert_eq!(fake.inserted(), 2);
    assert_eq!(fake.instances().len(), 2);
}

/// Asserts that an insert of a name a zone holds already fails with the reason
/// `alreadyExists` and creates nothing, while the same name in another zone is inserted.
/// It catches a fake that lets two instances share a name in one zone, or refuses a name
/// because another zone holds it.
#[test]
fn a_name_taken_in_the_zone_is_already_exists() {
    let fake = FakeGce::new();
    fake.insert_instance(&request("vm-a", "a1", &[])).unwrap();
    let error = fake
        .insert_instance(&request("vm-a", "a2", &[]))
        .unwrap_err();
    assert!(error.is_already_exists());
    assert_eq!(fake.inserted(), 1);
    let elsewhere = InsertRequest {
        zone: zone(OTHER_ZONE),
        ..request("vm-a", "a3", &[])
    };
    fake.insert_instance(&elsewhere).unwrap();
    assert_eq!(fake.inserted(), 2);
    assert_eq!(fake.instances()[1].zone, zone(OTHER_ZONE));
}

/// Asserts that a read answers the instance by its name and by its id, and answers
/// `notFound` for a name or id the fake does not hold and for a name that another zone
/// holds. It catches a read that ignores the zone or answers a stranger.
#[test]
fn a_read_answers_by_name_or_id_within_its_zone() {
    let fake = holding(1);
    let held = fake.instances().remove(0);
    assert_eq!(fake.get_instance(&by_name("vm-0")), Ok(held.clone()));
    assert_eq!(fake.get_instance(&by_id("1")), Ok(held));
    let elsewhere = InstanceRef {
        zone: zone(OTHER_ZONE),
        ..by_name("vm-0")
    };
    for missing in [elsewhere, by_name("vm-9"), by_id("77")] {
        assert!(fake.get_instance(&missing).unwrap_err().is_not_found());
    }
}

/// Asserts that a read of the operation an insert or a delete answered shows it done with
/// the same name and target, that a delete's operation is another one, and that an
/// operation the fake does not know is `notFound`. It catches an operation that stays
/// pending for ever or is shared by two calls.
#[test]
fn an_operation_reads_done_after_the_call_that_started_it() {
    let fake = holding(0);
    let inserted = fake.insert_instance(&request("vm-a", "a1", &[])).unwrap();
    let read = fake.get_operation(&inserted.reference).unwrap();
    assert_eq!(
        read,
        Operation {
            status: OperationStatus::Done,
            ..inserted.clone()
        }
    );
    assert!(!read.failed());
    let deleted = fake.delete_instance(&by_id("1")).unwrap();
    assert_eq!(deleted.status, OperationStatus::Pending);
    assert_eq!(deleted.target_id, Some(id("1")));
    assert_ne!(deleted.reference.name, inserted.reference.name);
    let read = fake.get_operation(&deleted.reference).unwrap();
    assert_eq!(read.status, OperationStatus::Done);
    let unknown = OperationRef {
        name: "operation-99".to_owned(),
        ..inserted.reference
    };
    assert!(fake.get_operation(&unknown).unwrap_err().is_not_found());
}

/// Asserts that a delete moves the instance to stopping and counts one call, by id or by
/// name; that an instance the zone does not hold (an unknown id, or one of another zone)
/// is `notFound` and changes and counts nothing; and that a delete scripted to fail before
/// it is sent is not counted. It catches a delete that counts what it did not do.
#[test]
fn a_delete_takes_effect_unless_it_fails_before_or_names_nothing() {
    let fake = holding(2);
    assert!(
        fake.delete_instance(&by_id("77"))
            .unwrap_err()
            .is_not_found()
    );
    let elsewhere = InstanceRef {
        zone: zone(OTHER_ZONE),
        ..by_id("1")
    };
    assert!(fake.delete_instance(&elsewhere).unwrap_err().is_not_found());
    assert_eq!(fake.deletes(), 0);
    fake.delete_instance(&by_id("1")).unwrap();
    assert_eq!(fake.deletes(), 1);
    assert_eq!(fake.instances()[0].status, InstanceStatus::Stopping);
    assert_eq!(fake.instances()[1].status, InstanceStatus::Provisioning);
    fake.delete_instance(&by_name("vm-1")).unwrap();
    assert_eq!(fake.deletes(), 2);
    assert_eq!(fake.instances()[1].status, InstanceStatus::Stopping);
    fake.fail_next(Method::DeleteInstance, ApiError::Timeout);
    assert_eq!(fake.delete_instance(&by_id("1")), Err(ApiError::Timeout));
    assert_eq!(fake.deletes(), 2);
}

/// Asserts that a list answers the instances that carry every label of the filter, of any
/// status (a terminated one included) and of any launcher, in the order they were added
/// (the ones it inserted and the ones added from outside), none when a label is missing or
/// has another value, and all when the filter has none; that the page names no next page
/// and no unreachable scope; and that only the fake's own inserts are counted as inserted.
/// It catches a filter that matches on one label only.
#[test]
fn a_list_answers_the_instances_with_every_label_of_the_filter() {
    let fake = FakeGce::new();
    fake.insert_instance(&request("vm-a", "a1", &[("k", "v"), ("x", "y")]))
        .unwrap();
    fake.insert_instance(&request("vm-b", "a2", &[("k", "v")]))
        .unwrap();
    fake.add_instance(row(
        "9000001",
        InstanceStatus::Running,
        "vm-c",
        vec![label("x", "y")],
    ));
    fake.set_status(&id("2"), InstanceStatus::Terminated);
    let names = |labels: &[(&str, &str)]| -> Vec<String> {
        fake.list_instances(&filter(labels, None))
            .unwrap()
            .items
            .iter()
            .map(|found| found.name.clone())
            .collect()
    };
    assert_eq!(names(&[("k", "v")]), ["vm-a", "vm-b"]);
    assert_eq!(names(&[("k", "v"), ("x", "y")]), ["vm-a"]);
    assert_eq!(names(&[("x", "y")]), ["vm-a", "vm-c"]);
    assert!(names(&[("k", "other")]).is_empty());
    assert_eq!(names(&[]).len(), 3);
    let page = fake.list_instances(&filter(&[], None)).unwrap();
    assert_eq!(page.next_page_token, None);
    assert!(!page.unreachable);
    assert_eq!(fake.inserted(), 2);
}

/// Asserts that a paginated list answers pages of the size asked for, each naming the token
/// of the next until the last, which names none; that following the tokens gives every
/// instance once in order; and that a size of zero is read as one. It catches a page that
/// repeats or skips an instance.
#[test]
fn a_paginated_list_is_followed_token_by_token() {
    let fake = holding(5);
    fake.paginate(2);
    let first = fake.list_instances(&filter(&[], None)).unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.next_page_token.as_deref(), Some("page-2"));
    let second = fake
        .list_instances(&filter(&[], first.next_page_token.clone()))
        .unwrap();
    assert_eq!(second.items.len(), 2);
    let third = fake
        .list_instances(&filter(&[], second.next_page_token.clone()))
        .unwrap();
    assert_eq!(third.items.len(), 1);
    assert_eq!(third.next_page_token, None);
    let walked: Vec<String> = [first, second, third]
        .into_iter()
        .flat_map(|page| page.items)
        .map(|found| found.name)
        .collect();
    assert_eq!(walked, ["vm-0", "vm-1", "vm-2", "vm-3", "vm-4"]);
    fake.paginate(0);
    let single = fake.list_instances(&filter(&[], None)).unwrap();
    assert_eq!(single.items.len(), 1);
    assert_eq!(single.next_page_token.as_deref(), Some("page-1"));
}

/// Asserts that a token the fake did not make is read as the first page, that a token past
/// the last instance answers an empty page that names no next one, and that a page beyond
/// the walk is not an error. It catches a fake that panics or loops on a token it does not
/// know.
#[test]
fn a_token_the_fake_did_not_make_is_the_first_page_and_one_past_the_end_is_empty() {
    let fake = holding(3);
    for junk in ["junk", "page-x", ""] {
        let page = fake
            .list_instances(&filter(&[], Some(junk.to_owned())))
            .unwrap();
        assert_eq!(page.items.len(), 3, "{junk:?}");
    }
    let past = fake
        .list_instances(&filter(&[], Some("page-99".to_owned())))
        .unwrap();
    assert!(past.items.is_empty());
    assert_eq!(past.next_page_token, None);
}

/// Asserts that a page failure fails the one call that asks for that page with an
/// incomplete answer and no short list, that the next call for it answers, that a page
/// beyond the walk fails nothing, and that an unreachable scope marks exactly the next page
/// it is scripted for. It catches a page failure that sticks, or one that is swallowed.
#[test]
fn a_failed_page_fails_one_call_and_an_unreachable_scope_marks_one_page() {
    let fake = holding(3);
    fake.paginate(1).fail_page(2);
    let first = fake.list_instances(&filter(&[], None)).unwrap();
    let token = first.next_page_token;
    assert_eq!(
        fake.list_instances(&filter(&[], token.clone())),
        Err(ApiError::Incomplete)
    );
    assert!(fake.list_instances(&filter(&[], token)).is_ok());
    fake.fail_page(9);
    assert!(fake.list_instances(&filter(&[], None)).is_ok());
    fake.answer_unreachable_once();
    assert!(fake.list_instances(&filter(&[], None)).unwrap().unreachable);
    assert!(!fake.list_instances(&filter(&[], None)).unwrap().unreachable);
}

/// Asserts that an answer lost after a call took effect leaves the effect and answers the
/// error, once, for each of the five launch calls and for a catalog call, and that several
/// queue in order: an insert that landed is found again by its request id and is not
/// inserted twice, a delete that landed is counted. It catches a lost answer that undoes
/// the call or sticks.
#[test]
fn a_lost_answer_leaves_the_effect_for_every_launch_call() {
    let fake = FakeGce::new();
    let insert = request("vm-a", "a1", &[]);
    fake.lose_after_next(Method::InsertInstance, ApiError::Timeout);
    assert_eq!(fake.insert_instance(&insert), Err(ApiError::Timeout));
    assert_eq!(fake.inserted(), 1);
    let found = fake.insert_instance(&insert).unwrap();
    assert_eq!(found.target_id, Some(id("1")));
    assert_eq!(fake.inserted(), 1);

    fake.lose_after_next(Method::GetInstance, ApiError::Timeout)
        .lose_after_next(Method::GetInstance, ApiError::Incomplete);
    assert_eq!(fake.get_instance(&by_id("1")), Err(ApiError::Timeout));
    assert_eq!(fake.get_instance(&by_id("1")), Err(ApiError::Incomplete));
    assert!(fake.get_instance(&by_id("1")).is_ok());

    fake.lose_after_next(Method::GetOperation, ApiError::Timeout);
    assert_eq!(fake.get_operation(&found.reference), Err(ApiError::Timeout));
    assert!(fake.get_operation(&found.reference).is_ok());

    fake.lose_after_next(Method::ListInstances, ApiError::Timeout);
    assert_eq!(
        fake.list_instances(&filter(&[], None)),
        Err(ApiError::Timeout)
    );
    assert!(fake.list_instances(&filter(&[], None)).is_ok());

    fake.lose_after_next(Method::DeleteInstance, ApiError::Timeout);
    assert_eq!(fake.delete_instance(&by_id("1")), Err(ApiError::Timeout));
    assert_eq!(fake.deletes(), 1);
    assert_eq!(fake.instances()[0].status, InstanceStatus::Stopping);

    fake.lose_after_next(Method::MachineTypes, ApiError::Timeout);
    let lost = fake.machine_types(&project(), &[]).unwrap_err();
    assert_eq!(lost, ApiError::Timeout);
    assert!(fake.machine_types(&project(), &[]).unwrap().is_empty());
}

/// Asserts that a failure scripted for the next call of a launch method fails that call
/// before it does anything, for each of the five, and is spent by it: nothing is inserted,
/// nothing is deleted, and the next call answers. It catches a scripted failure that has
/// the effect it should have prevented.
#[test]
fn a_failure_scripted_before_a_launch_call_leaves_nothing_behind() {
    let fake = FakeGce::new();
    let insert = request("vm-a", "a1", &[]);
    fake.fail_next(Method::InsertInstance, ApiError::Timeout);
    assert_eq!(fake.insert_instance(&insert), Err(ApiError::Timeout));
    assert_eq!((fake.inserted(), fake.instances().len()), (0, 0));
    let operation = fake.insert_instance(&insert).unwrap();
    assert_eq!(fake.inserted(), 1);

    fake.fail_next(Method::GetInstance, ApiError::Timeout);
    assert_eq!(fake.get_instance(&by_id("1")), Err(ApiError::Timeout));
    assert!(fake.get_instance(&by_id("1")).is_ok());

    fake.fail_next(Method::GetOperation, ApiError::Timeout);
    assert_eq!(
        fake.get_operation(&operation.reference),
        Err(ApiError::Timeout)
    );
    assert!(fake.get_operation(&operation.reference).is_ok());

    fake.fail_next(Method::ListInstances, ApiError::Timeout);
    assert_eq!(
        fake.list_instances(&filter(&[], None)),
        Err(ApiError::Timeout)
    );
    assert!(fake.list_instances(&filter(&[], None)).is_ok());

    fake.fail_next(Method::DeleteInstance, ApiError::Timeout);
    assert_eq!(fake.delete_instance(&by_id("1")), Err(ApiError::Timeout));
    assert_eq!(fake.deletes(), 0);
    assert_eq!(fake.instances()[0].status, InstanceStatus::Provisioning);
}

/// Asserts the order scripted failures are taken in: the next call of any kind first, then
/// the next call of the method, then the standing failure of the method, which stays; that
/// a standing failure of one method leaves the others alone; and that every call, failed
/// ones included, is recorded in order with its method, project and zones. It catches a
/// failure taken out of order and a call that goes unrecorded.
#[test]
fn scripted_failures_are_taken_in_order_and_every_call_is_recorded() {
    let fake = holding(1);
    fake.fail_always(Method::GetInstance, ApiError::Timeout)
        .fail_next(Method::GetInstance, ApiError::Incomplete)
        .fail_next_call(ApiError::Transport("reset".to_owned()));
    let read = || fake.get_instance(&by_id("1"));
    assert_eq!(read(), Err(ApiError::Transport("reset".to_owned())));
    assert_eq!(read(), Err(ApiError::Incomplete));
    assert_eq!(read(), Err(ApiError::Timeout));
    assert_eq!(read(), Err(ApiError::Timeout));
    assert!(fake.list_instances(&filter(&[], None)).is_ok());
    assert_eq!(
        fake.methods(),
        [
            Method::InsertInstance,
            Method::GetInstance,
            Method::GetInstance,
            Method::GetInstance,
            Method::GetInstance,
            Method::ListInstances,
        ]
    );
    let calls = fake.calls();
    assert_eq!(
        calls[0],
        Call {
            method: Method::InsertInstance,
            project: Some(project()),
            region: None,
            zones: vec![zone(ZONE)],
        }
    );
    assert_eq!(
        calls[5],
        Call {
            method: Method::ListInstances,
            project: Some(project()),
            region: None,
            zones: Vec::new(),
        }
    );
}

/// Asserts that the operation of an insert scripted to fail is answered pending with no
/// target, inserts no instance, shows done with the scripted error code when it is read,
/// is the answer to a repeat of the same request id (which uses up no further code), and
/// that several queue in order, one per insert that is not a repeat, after which inserts
/// go through. It catches an operation that fails without its code or inserts anyway.
#[test]
fn an_insert_can_start_an_operation_that_fails() {
    let fake = FakeGce::new();
    fake.fail_next_operation("ZONE_RESOURCE_POOL_EXHAUSTED")
        .fail_next_operation("QUOTA_EXCEEDED");
    let first = request("vm-a", "a1", &[]);
    let answer = fake.insert_instance(&first).unwrap();
    assert_eq!(answer.status, OperationStatus::Pending);
    assert_eq!(answer.target_id, None);
    assert!(answer.error_codes.is_empty());
    assert_eq!((fake.inserted(), fake.instances().len()), (0, 0));
    let read = fake.get_operation(&answer.reference).unwrap();
    assert!(read.failed());
    assert_eq!(
        read.error_codes,
        [ErrorReason::new("ZONE_RESOURCE_POOL_EXHAUSTED")]
    );
    assert_eq!(fake.insert_instance(&first), Ok(answer));

    let second = fake.insert_instance(&request("vm-b", "a2", &[])).unwrap();
    let read = fake.get_operation(&second.reference).unwrap();
    assert_eq!(read.error_codes, [ErrorReason::new("QUOTA_EXCEEDED")]);
    assert_eq!(fake.inserted(), 0);

    let third = fake.insert_instance(&request("vm-c", "a3", &[])).unwrap();
    assert_eq!(third.target_id, Some(id("1")));
    assert!(!fake.get_operation(&third.reference).unwrap().failed());
    assert_eq!(fake.inserted(), 1);
}

/// Asserts that a read scripted to fail after the next delete lets the reads before it
/// answer, fails the first read after a delete that took effect and only that one, takes
/// one scripted failure per delete in order, is not armed by a delete that failed before
/// it was sent, and is not armed by a delete with nothing queued. It catches a failure that
/// lands on the wrong read.
#[test]
fn a_read_after_the_next_delete_can_be_failed() {
    let fake = holding(2);
    fake.fail_read_after_next_delete(ApiError::Timeout)
        .fail_read_after_next_delete(ApiError::Incomplete);
    assert!(fake.get_instance(&by_id("1")).is_ok());
    fake.delete_instance(&by_id("1")).unwrap();
    assert_eq!(fake.get_instance(&by_id("2")), Err(ApiError::Timeout));
    assert!(fake.get_instance(&by_id("2")).is_ok());
    fake.delete_instance(&by_id("2")).unwrap();
    assert_eq!(fake.get_instance(&by_id("2")), Err(ApiError::Incomplete));
    assert!(fake.get_instance(&by_id("2")).is_ok());
    fake.delete_instance(&by_id("2")).unwrap();
    assert!(fake.get_instance(&by_id("2")).is_ok());

    fake.fail_read_after_next_delete(ApiError::Timeout);
    fake.fail_next(
        Method::DeleteInstance,
        ApiError::Transport("reset".to_owned()),
    );
    assert!(fake.delete_instance(&by_id("1")).is_err());
    assert!(fake.get_instance(&by_id("1")).is_ok());
    fake.delete_instance(&by_id("1")).unwrap();
    assert_eq!(fake.get_instance(&by_id("1")), Err(ApiError::Timeout));
}

/// Asserts that `set_status` changes the instance named and no other and does nothing for
/// an id the fake does not hold, that `remove_instance` makes the cloud forget the one
/// named so that a read of it is `notFound`, and that an instance added from outside is
/// held and listed but not counted as inserted. It catches a status set on every instance
/// or a removal of the wrong one.
#[test]
fn instances_can_be_set_removed_and_added_from_outside() {
    let fake = holding(2);
    fake.set_status(&id("1"), InstanceStatus::Running);
    fake.set_status(&id("77"), InstanceStatus::Stopped);
    let states: Vec<InstanceStatus> = fake
        .instances()
        .into_iter()
        .map(|found| found.status)
        .collect();
    assert_eq!(
        states,
        [InstanceStatus::Running, InstanceStatus::Provisioning]
    );
    fake.remove_instance(&id("1"));
    fake.remove_instance(&id("77"));
    assert!(fake.get_instance(&by_id("1")).unwrap_err().is_not_found());
    assert_eq!(fake.instances().len(), 1);
    fake.add_instance(row(
        "9000001",
        InstanceStatus::Terminated,
        "vm-x",
        Vec::new(),
    ));
    assert_eq!(fake.instances().len(), 2);
    assert_eq!(fake.instances()[1].name, "vm-x");
    assert_eq!(fake.inserted(), 2);
}
