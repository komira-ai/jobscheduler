//! The launch side: insert, zone operations, get, delete and the list by ownership marks.

use super::*;

fn request_id() -> RequestId {
    RequestId::derive(
        &AcquireId::new("acquire-0001").unwrap(),
        &SchedulerId::new("example-scheduler").unwrap(),
    )
}

pub(crate) fn insert_request() -> InsertRequest {
    InsertRequest {
        project: project(),
        zone: zone(ZONE_A),
        name: InstanceName::new(INSTANCE_NAME).unwrap(),
        machine_type: MachineTypeName("n1-standard-4".to_owned()),
        scheduling: Scheduling {
            provisioning_model: ProvisioningModel::Standard,
            on_host_maintenance: HostMaintenance::Terminate,
            automatic_restart: false,
            termination_action: None,
            max_run_duration: None,
        },
        image: ImagePath::new("projects/example-project/global/images/js-agent-x86").unwrap(),
        disk_size_gb: None,
        accelerators: None,
        labels: vec![label("js-scheduler", "example-scheduler")],
        payload: Payload::new("REDACTED-BOOT-PAYLOAD".to_owned()),
        network: NetworkInterface {
            network: "global/networks/default".to_owned(),
            subnetwork: "regions/us-central1/subnetworks/default".to_owned(),
        },
        request_id: request_id(),
    }
}

fn label(key: &str, value: &str) -> Label {
    Label {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

pub(crate) fn by_id(text: &str) -> InstanceRef {
    InstanceRef {
        project: project(),
        zone: zone(ZONE_A),
        instance: InstanceKey::Id(id(text)),
    }
}

fn operation_of(name: &str, status: OperationStatus, codes: &[&str]) -> Operation {
    Operation {
        reference: OperationRef {
            project: project(),
            zone: zone(ZONE_A),
            name: name.to_owned(),
        },
        status,
        target_id: Some(id(INSTANCE_ID)),
        error_codes: codes.iter().map(|code| ErrorReason::new(code)).collect(),
    }
}

/// Asserts that `instances.insert` is one POST to the zone's instances with the request id
/// as `requestId`, and that the answer is read as the zone operation it is: the name, the
/// zone from its URL, the status `RUNNING` and the id of the instance it creates
/// (`targetId`, a string), with no error. It catches a target id read as a number and a
/// zone read from the wrong member.
#[test]
fn an_insert_answers_a_running_zone_operation_with_its_target() {
    let (client, script) = over(vec![answer(fixture!("instances_insert_operation.json"))]);
    let operation = client.insert_instance(&insert_request()).unwrap();
    assert_eq!(
        operation,
        operation_of(OPERATION, OperationStatus::Running, &[])
    );
    assert!(!operation.failed());
    let sent = api_calls(&script);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method.as_str(), "POST");
    assert_eq!(
        sent[0].url,
        format!(
            "{COMPUTE}/zones/{ZONE_A}/instances?requestId={}",
            request_id().as_str()
        )
    );
}

/// Asserts that `zoneOperations.get` reads each state of an insert's operation from the
/// request `.../zones/<zone>/operations/<name>`: still running; done with no error; done
/// with the error code `ZONE_RESOURCE_POOL_EXHAUSTED`; and done with `QUOTA_EXCEEDED`. A
/// failed operation reports `failed()` and its codes, and the launch mapping (the same
/// call the provider makes on each code) is a capacity refusal for the first and a quota
/// refusal for the second, with the code as the reason. It catches an error code read from
/// the message, a finished operation read as failed without an error, and a code mapped to
/// the wrong constant.
#[test]
fn zone_operations_are_read_running_done_and_failed() {
    let case = |text: &str| {
        let (client, script) = over(vec![answer(text)]);
        let reference = OperationRef {
            project: project(),
            zone: zone(ZONE_A),
            name: OPERATION.to_owned(),
        };
        let operation = client.get_operation(&reference).unwrap();
        let sent = api_calls(&script);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method.as_str(), "GET");
        assert_eq!(
            sent[0].url,
            format!("{COMPUTE}/zones/{ZONE_A}/operations/{OPERATION}")
        );
        operation
    };
    let running = case(fixture!("zone_operations_get_running.json"));
    assert_eq!(
        running,
        operation_of(OPERATION, OperationStatus::Running, &[])
    );
    assert!(!running.failed());

    let done = case(fixture!("zone_operations_get_done.json"));
    assert_eq!(done, operation_of(OPERATION, OperationStatus::Done, &[]));
    assert!(!done.failed());

    let refused = |code: RefusalCode, reason: &str| {
        Acquired::Refused(Refusal {
            code,
            reason: Reason::Api(reason.to_owned()),
        })
    };
    let exhausted = case(fixture!(
        "zone_operations_get_done_zone_resource_pool_exhausted.json"
    ));
    assert_eq!(
        exhausted,
        operation_of(
            OPERATION,
            OperationStatus::Done,
            &["ZONE_RESOURCE_POOL_EXHAUSTED"]
        )
    );
    assert!(exhausted.failed());
    assert_eq!(
        launch_answer(&ApiError::answered(exhausted.error_codes[0].as_str())),
        refused(RefusalCode::Capacity, "ZONE_RESOURCE_POOL_EXHAUSTED")
    );

    let over_quota = case(fixture!("zone_operations_get_done_quota_exceeded.json"));
    assert_eq!(
        over_quota,
        operation_of(OPERATION, OperationStatus::Done, &["QUOTA_EXCEEDED"])
    );
    assert!(over_quota.failed());
    assert_eq!(
        launch_answer(&ApiError::answered(over_quota.error_codes[0].as_str())),
        refused(RefusalCode::Quota, "QUOTA_EXCEEDED")
    );
}

/// Asserts that `instances.get` by id is one GET of the instance's URL and that the answer
/// is read into the instance: its id (a string), name, `PROVISIONING` status (booting to
/// the scheduler), the machine type's name from its URL, the zone's name from its URL, and
/// the three ownership labels (the scheduler's, the acquire's and a user label), and that
/// the scheduler's reference to it is `<zone>/<id>`. It catches a status or a label lost
/// and a URL kept where a name is wanted.
#[test]
fn an_instance_is_read_with_its_status_and_labels() {
    let (client, script) = over(vec![answer(fixture!("instances_get_provisioning.json"))]);
    let instance = client.get_instance(&by_id(INSTANCE_ID)).unwrap();
    assert_eq!(
        instance,
        Instance {
            id: id(INSTANCE_ID),
            name: INSTANCE_NAME.to_owned(),
            status: InstanceStatus::Provisioning,
            labels: vec![
                label("js-acquire-id", "acquire-0001"),
                label("js-label-validation-run", "example-run-1"),
                label("js-scheduler", "example-scheduler"),
            ],
            machine_type: MachineTypeName("n1-standard-4".to_owned()),
            zone: zone(ZONE_A),
        }
    );
    assert_eq!(instance.status.status(), HostStatus::Booting);
    assert_eq!(instance.label("js-scheduler"), Some("example-scheduler"));
    assert_eq!(
        instance.host_ref().as_str(),
        format!("{ZONE_A}/{INSTANCE_ID}")
    );
    let sent = api_calls(&script);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method.as_str(), "GET");
    assert_eq!(
        sent[0].url,
        format!("{COMPUTE}/zones/{ZONE_A}/instances/{INSTANCE_ID}")
    );
}

/// Asserts that `instances.delete` is one DELETE of the instance's URL with no body and
/// that its answer is read as the running delete operation of that instance. It catches a
/// delete sent as another method and an operation read without its target.
#[test]
fn a_delete_answers_a_running_zone_operation_with_its_target() {
    let (client, script) = over(vec![answer(fixture!("instances_delete_operation.json"))]);
    let operation = client.delete_instance(&by_id(INSTANCE_ID)).unwrap();
    assert_eq!(
        operation,
        operation_of(
            "operation-1700000000000-6000000000002-00000002-0000a002",
            OperationStatus::Running,
            &[]
        )
    );
    let sent = api_calls(&script);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method.as_str(), "DELETE");
    assert!(sent[0].body.is_empty());
    assert_eq!(
        sent[0].url,
        format!("{COMPUTE}/zones/{ZONE_A}/instances/{INSTANCE_ID}")
    );
}

fn marks() -> ListFilter {
    ListFilter {
        project: project(),
        labels: vec![
            label("js-scheduler", "example-scheduler"),
            label("js-label-validation-run", "example-run-1"),
        ],
        page_token: None,
    }
}

/// Asserts that the list of the scheduler's instances, asked for by its ownership marks,
/// is two requests that both carry the label filter (`labels.<key> = "<value>"` joined by
/// `AND`) and partial success, the second with the first page's token; that the first page
/// holds the PROVISIONING, STAGING and RUNNING instances and the second the STOPPING,
/// TERMINATED and SUSPENDED ones, each with its id, name, labels, machine type and zone;
/// that the scheduler reads them as booting, booting, active, terminating, terminated and
/// terminated; and that the warning-only scope of a zone with nothing on the page adds no
/// row and does not mark the page unreachable. It catches a page token dropped from the
/// second request, a status mapped to the wrong state, and a warning scope read as rows.
#[test]
fn the_list_by_ownership_marks_walks_two_pages_and_maps_every_status() {
    let (client, script) = over(vec![
        answer(fixture!("instances_aggregated_list_page1.json")),
        answer(fixture!("instances_aggregated_list_page2.json")),
    ]);
    let first = client.list_instances(&marks()).unwrap();
    assert_eq!(
        first.next_page_token.as_deref(),
        Some("EXAMPLE-PAGE-TOKEN-3")
    );
    assert!(!first.unreachable);
    let second = client
        .list_instances(&ListFilter {
            page_token: first.next_page_token.clone(),
            ..marks()
        })
        .unwrap();
    assert_eq!(second.next_page_token, None);
    assert!(!second.unreachable);
    assert_eq!(script.left(), 0);

    let owned = |n: u32, status: InstanceStatus, zone_name: &str, machine: &str| Instance {
        id: id(&format!("12345678901234567{n:02}")),
        name: format!("js-0123456789ab-fedcba98765{n}"),
        status,
        labels: vec![
            label("js-acquire-id", &format!("acquire-{n:04}")),
            label("js-label-validation-run", "example-run-1"),
            label("js-scheduler", "example-scheduler"),
        ],
        machine_type: MachineTypeName(machine.to_owned()),
        zone: zone(zone_name),
    };
    assert_eq!(
        first.items,
        [
            owned(1, InstanceStatus::Provisioning, ZONE_A, "n1-standard-4"),
            owned(2, InstanceStatus::Staging, ZONE_A, "n1-standard-4"),
            owned(3, InstanceStatus::Running, ZONE_A, "a2-highgpu-1g"),
        ]
    );
    assert_eq!(
        second.items,
        [
            owned(4, InstanceStatus::Stopping, ZONE_B, "n1-standard-4"),
            owned(5, InstanceStatus::Terminated, ZONE_B, "n1-standard-4"),
            owned(6, InstanceStatus::Suspended, ZONE_B, "n1-standard-4"),
        ]
    );
    let states: Vec<HostStatus> = first
        .items
        .iter()
        .chain(&second.items)
        .map(|instance| instance.status.status())
        .collect();
    assert_eq!(
        states,
        [
            HostStatus::Booting,
            HostStatus::Booting,
            HostStatus::Active,
            HostStatus::Terminating,
            HostStatus::Terminated,
            HostStatus::Terminated,
        ]
    );

    let filter = "labels.js-scheduler = \"example-scheduler\" AND \
                  labels.js-label-validation-run = \"example-run-1\"";
    let url = format!("{COMPUTE}/aggregated/instances");
    assert_eq!(
        calls(&script),
        [
            (
                url.clone(),
                query(&[("returnPartialSuccess", "true"), ("filter", filter)])
            ),
            (
                url,
                query(&[
                    ("returnPartialSuccess", "true"),
                    ("filter", filter),
                    ("pageToken", "EXAMPLE-PAGE-TOKEN-3"),
                ])
            ),
        ]
    );
}

/// Asserts that an aggregated list whose every scope holds only a `NO_RESULTS_ON_PAGE`
/// warning is an empty page with no next token that is not marked unreachable. It catches
/// an empty list read as a failure.
#[test]
fn a_list_whose_scopes_hold_only_warnings_is_an_empty_page() {
    let (client, _) = over(vec![answer(fixture!(
        "instances_aggregated_list_empty.json"
    ))]);
    let page = client.list_instances(&marks()).unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.next_page_token, None);
    assert!(!page.unreachable);
}
