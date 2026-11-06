//! The launch side: `instances.insert`, `instances.get`, `instances.delete`,
//! `instances.aggregatedList` and `zoneOperations.get`, the request each makes and the
//! records each answer is read into.

use std::time::Duration;

use js_elastic::{AcquireId, SchedulerId};
use js_provider_gce::{
    AcceleratorTypeName, ApiError, AttachedAccelerator, COMPUTE_BASE, ErrorReason, GceApi,
    HostMaintenance, ImagePath, InsertRequest, Instance, InstanceId, InstanceKey, InstanceName,
    InstanceRef, InstanceStatus, Label, ListFilter, MachineTypeName, NetworkInterface, Operation,
    OperationRef, OperationStatus, Page, Payload, Project, ProvisioningModel, RequestId,
    Scheduling, TerminationAction, Zone,
};
use serde_json::{Value, json};

use crate::script::{TOKEN, answer, api_calls, over};

pub const ZONE: &str = "region-one-a";

pub fn project() -> Project {
    Project::new("sample-project").unwrap()
}

pub fn zone(name: &str) -> Zone {
    Zone::new(name).unwrap()
}

fn instance_id(text: &str) -> InstanceId {
    InstanceId::new(text).unwrap()
}

fn label(key: &str, value: &str) -> Label {
    Label {
        key: key.to_owned(),
        value: value.to_owned(),
    }
}

fn request_id() -> RequestId {
    RequestId::derive(
        &AcquireId::new("acquire-1").unwrap(),
        &SchedulerId::new("scheduler-1").unwrap(),
    )
}

fn on_demand() -> Scheduling {
    Scheduling {
        provisioning_model: ProvisioningModel::Standard,
        on_host_maintenance: HostMaintenance::Terminate,
        automatic_restart: false,
        termination_action: None,
        max_run_duration: None,
    }
}

fn spot() -> Scheduling {
    Scheduling {
        provisioning_model: ProvisioningModel::Spot,
        on_host_maintenance: HostMaintenance::Terminate,
        automatic_restart: false,
        termination_action: Some(TerminationAction::Delete),
        max_run_duration: Some(Duration::from_secs(3600)),
    }
}

fn insert_request(scheduling: Scheduling, payload: &str) -> InsertRequest {
    InsertRequest {
        project: project(),
        zone: zone(ZONE),
        name: InstanceName::new("js-vm-one").unwrap(),
        machine_type: MachineTypeName("q9-standard-2".to_owned()),
        scheduling,
        image: ImagePath::new("projects/image-project/global/images/js-agent-x86").unwrap(),
        disk_size_gb: None,
        accelerators: None,
        labels: vec![
            label("js-scheduler", "sched-one"),
            label("js-acquire-id", "acquire-1"),
        ],
        payload: Payload::new(payload.to_owned()),
        network: NetworkInterface {
            network: "global/networks/default".to_owned(),
            subnetwork: "regions/region-one/subnetworks/default".to_owned(),
        },
        request_id: request_id(),
    }
}

fn zone_url(name: &str) -> String {
    format!("{COMPUTE_BASE}/projects/sample-project/zones/{name}")
}

fn operation_json(status: &str) -> Value {
    json!({
        "kind": "compute#operation",
        "name": "operation-one",
        "zone": zone_url(ZONE),
        "status": status,
    })
}

pub fn instance_json(id: &str, name: &str, status: &str, zone_name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "status": status,
        "machineType": format!("{}/machineTypes/q9-standard-2", zone_url(zone_name)),
        "zone": zone_url(zone_name),
        "labels": {"js-scheduler": "sched-one", "js-acquire-id": "acquire-1"},
    })
}

fn instance_ref(key: InstanceKey) -> InstanceRef {
    InstanceRef {
        project: project(),
        zone: zone(ZONE),
        instance: key,
    }
}

pub fn by_name() -> InstanceRef {
    instance_ref(InstanceKey::Name(InstanceName::new("js-vm-one").unwrap()))
}

fn operation_ref(name: &str) -> OperationRef {
    OperationRef {
        project: project(),
        zone: zone(ZONE),
        name: name.to_owned(),
    }
}

fn is_unreadable<T>(result: &Result<T, ApiError>) -> bool {
    matches!(result, Err(ApiError::Unreadable(_)))
}

/// Asserts that an on-demand `insert_instance` with a disk size and an attached
/// accelerator is one POST to the zone's instances with the request id as the `requestId`
/// query parameter, the bearer token, a JSON content type, and a body that names the
/// instance, the machine type path, a boot disk deleted with the instance and built from
/// the image at the stated size, one network interface with no access config, the
/// accelerator, the standard scheduling, the labels and the payload byte for byte as the
/// `user-data` metadata item. It catches a body that leaves a field off, gives the
/// instance an external address, sends the payload altered, or sends no request id.
#[test]
fn an_on_demand_insert_is_one_post_with_every_field_of_the_instance() {
    let mut request = insert_request(on_demand(), "#cloud-config\njoin: \"s3cret\" \u{e9}\\n\n");
    request.disk_size_gb = Some(200);
    request.accelerators = Some(AttachedAccelerator {
        type_name: AcceleratorTypeName("q9-gpu-one".to_owned()),
        count: 2,
    });
    let (client, script) = over(vec![answer(&operation_json("PENDING").to_string())]);
    client.insert_instance(&request).unwrap();
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method.as_str(), "POST");
    assert_eq!(
        call.url,
        format!(
            "{}/instances?requestId={}",
            zone_url(ZONE),
            request_id().as_str()
        )
    );
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(call.header("content-type"), Some("application/json"));
    let body: Value = serde_json::from_slice(&call.body).unwrap();
    assert_eq!(
        body,
        json!({
            "name": "js-vm-one",
            "machineType": "zones/region-one-a/machineTypes/q9-standard-2",
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "initializeParams": {
                    "sourceImage": "projects/image-project/global/images/js-agent-x86",
                    "diskSizeGb": "200",
                },
            }],
            "networkInterfaces": [{
                "network": "global/networks/default",
                "subnetwork": "regions/region-one/subnetworks/default",
            }],
            "guestAccelerators": [{
                "acceleratorType": "zones/region-one-a/acceleratorTypes/q9-gpu-one",
                "acceleratorCount": 2,
            }],
            "scheduling": {
                "provisioningModel": "STANDARD",
                "onHostMaintenance": "TERMINATE",
                "automaticRestart": false,
            },
            "labels": {"js-scheduler": "sched-one", "js-acquire-id": "acquire-1"},
            "metadata": {"items": [{
                "key": "user-data",
                "value": "#cloud-config\njoin: \"s3cret\" \u{e9}\\n\n",
            }]},
        })
    );
}

/// Asserts that a Spot `insert_instance` with no disk size and no accelerator states the
/// Spot provisioning model, the `DELETE` termination action and the maximum run duration
/// in whole seconds as a string, leaves `diskSizeGb` and `guestAccelerators` out, and that
/// an on-demand one leaves the termination action and the duration out. It catches a Spot
/// instance that lingers as TERMINATED after a preemption, and a limit sent as a number.
#[test]
fn a_spot_insert_states_the_termination_action_and_the_run_limit() {
    let (client, script) = over(vec![answer(&operation_json("PENDING").to_string())]);
    client
        .insert_instance(&insert_request(spot(), "payload"))
        .unwrap();
    let body: Value = serde_json::from_slice(&api_calls(&script)[0].body).unwrap();
    assert_eq!(
        body["scheduling"],
        json!({
            "provisioningModel": "SPOT",
            "onHostMaintenance": "TERMINATE",
            "automaticRestart": false,
            "instanceTerminationAction": "DELETE",
            "maxRunDuration": {"seconds": "3600"},
        })
    );
    assert_eq!(
        body["disks"][0]["initializeParams"],
        json!({"sourceImage": "projects/image-project/global/images/js-agent-x86"})
    );
    assert!(body.get("guestAccelerators").is_none());
}

/// Asserts that the operation an insert answers with is read into its reference (the
/// request's project, the zone from the operation's zone URL and its name), its status,
/// the target instance id and the codes of its errors in order; and that an operation with
/// no `targetId` and no `error` has neither. It catches an operation that loses its error
/// codes, and one that reads the zone URL whole.
#[test]
fn an_operation_is_read_with_its_target_and_its_error_codes() {
    let mut failed = operation_json("DONE");
    failed["targetId"] = json!("1234567890123456789");
    failed["error"] = json!({"errors": [
        {"code": "QUOTA_EXCEEDED", "message": "private text"},
        {"code": "ZONE_RESOURCE_POOL_EXHAUSTED"},
    ]});
    let (client, _) = over(vec![
        answer(&failed.to_string()),
        answer(&operation_json("RUNNING").to_string()),
    ]);
    let request = insert_request(on_demand(), "payload");
    let operation = client.insert_instance(&request).unwrap();
    assert_eq!(
        operation,
        Operation {
            reference: operation_ref("operation-one"),
            status: OperationStatus::Done,
            target_id: Some(instance_id("1234567890123456789")),
            error_codes: vec![
                ErrorReason::new("QUOTA_EXCEEDED"),
                ErrorReason::new("ZONE_RESOURCE_POOL_EXHAUSTED"),
            ],
        }
    );
    assert!(operation.failed());
    let running = client.insert_instance(&request).unwrap();
    assert_eq!(running.status, OperationStatus::Running);
    assert_eq!(running.target_id, None);
    assert!(running.error_codes.is_empty());
    assert!(!running.failed());
}

/// Asserts that an operation answer that lacks its name, zone or status, whose zone is not
/// a zone name, whose status is not one the cloud documents, whose `targetId` is not an
/// instance id (text or not), whose `error` lists no errors or an error with no code, and
/// an answer that is not JSON are each `Unreadable` for an insert, a delete and an
/// operation read alike, with the missing field named. It catches an answer read as a
/// success that is not one.
#[test]
fn an_operation_answer_that_cannot_be_read_is_unreadable() {
    let with = |key: &str, value: Value| {
        let mut body = operation_json("DONE");
        body[key] = value;
        body.to_string()
    };
    let without = |key: &str| {
        let mut body = operation_json("DONE");
        body.as_object_mut().unwrap().remove(key);
        body.to_string()
    };
    let bodies = [
        without("name"),
        without("zone"),
        without("status"),
        with("zone", json!("nowhere")),
        with("status", json!("WEIRD")),
        with("targetId", json!("abc")),
        with("targetId", json!(5)),
        with("error", json!({})),
        with("error", json!({"errors": [{"message": "no code"}]})),
        "not json".to_owned(),
    ];
    let request = insert_request(on_demand(), "payload");
    for body in bodies {
        let (client, _) = over(vec![answer(&body), answer(&body), answer(&body)]);
        assert!(is_unreadable(&client.insert_instance(&request)), "{body}");
        assert!(is_unreadable(&client.delete_instance(&by_name())), "{body}");
        assert!(
            is_unreadable(&client.get_operation(&operation_ref("operation-one"))),
            "{body}"
        );
    }
    let (client, _) = over(vec![answer(&without("name"))]);
    assert_eq!(
        client.insert_instance(&request),
        Err(ApiError::Unreadable("name is missing".to_owned()))
    );
}

/// Asserts that `get_instance` is one GET of the instance by name or by id with the bearer
/// token and no body, and reads the id, name, status, labels (sorted by key), the machine
/// type's name from its URL and the zone's name from its URL; and that an instance with no
/// `labels` has none and one with a status this release does not know keeps it as
/// `Other`. It catches a URL that is kept whole and a missing label list that fails.
#[test]
fn get_instance_is_one_get_by_name_or_id_and_reads_the_record() {
    let mut unlabeled = instance_json("1234567890123456790", "js-vm-two", "FUTURE_STATUS", ZONE);
    unlabeled.as_object_mut().unwrap().remove("labels");
    let (client, script) = over(vec![
        answer(&instance_json("1234567890123456789", "js-vm-one", "RUNNING", ZONE).to_string()),
        answer(&unlabeled.to_string()),
    ]);
    let found = client.get_instance(&by_name()).unwrap();
    assert_eq!(
        found,
        Instance {
            id: instance_id("1234567890123456789"),
            name: "js-vm-one".to_owned(),
            status: InstanceStatus::Running,
            labels: vec![
                label("js-acquire-id", "acquire-1"),
                label("js-scheduler", "sched-one"),
            ],
            machine_type: MachineTypeName("q9-standard-2".to_owned()),
            zone: zone(ZONE),
        }
    );
    let by_id = instance_ref(InstanceKey::Id(instance_id("1234567890123456790")));
    let other = client.get_instance(&by_id).unwrap();
    assert_eq!(
        other.status,
        InstanceStatus::Other("FUTURE_STATUS".to_owned())
    );
    assert!(other.labels.is_empty());
    let calls = api_calls(&script);
    assert_eq!(calls[0].method.as_str(), "GET");
    assert_eq!(
        calls[0].url,
        format!("{}/instances/js-vm-one", zone_url(ZONE))
    );
    assert_eq!(
        calls[1].url,
        format!("{}/instances/1234567890123456790", zone_url(ZONE))
    );
    for call in &calls {
        assert!(call.body.is_empty());
        assert_eq!(call.header("content-type"), None);
        assert_eq!(
            call.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
}

/// Asserts that an instance answer that lacks its id, name, status, machine type or zone
/// (the field named), whose id is not an id, whose zone is not a zone name, whose labels
/// are not an object or hold a value that is not text, and one that is not JSON are each
/// `Unreadable`. It catches an instance read from a half answer.
#[test]
fn an_instance_answer_that_cannot_be_read_is_unreadable() {
    let base = || instance_json("1234567890123456789", "js-vm-one", "RUNNING", ZONE);
    let mut bodies = Vec::new();
    for key in ["id", "name", "status", "machineType", "zone"] {
        let mut body = base();
        body.as_object_mut().unwrap().remove(key);
        bodies.push((body.to_string(), Some(format!("{key} is missing"))));
    }
    for (key, value) in [
        ("id", json!("012")),
        ("zone", json!("nowhere")),
        ("labels", json!("flat")),
        ("labels", json!({"key": 5})),
    ] {
        let mut body = base();
        body[key] = value;
        bodies.push((body.to_string(), None));
    }
    bodies.push(("not json".to_owned(), None));
    for (body, text) in bodies {
        let (client, _) = over(vec![answer(&body)]);
        let result = client.get_instance(&by_name());
        assert!(is_unreadable(&result), "{body}");
        if let Some(text) = text {
            assert_eq!(result, Err(ApiError::Unreadable(text)));
        }
    }
}

/// Asserts that `delete_instance` is one DELETE of the instance by name with the bearer
/// token and no body, and answers the operation the cloud started. It catches a delete
/// sent as another method and one whose operation is thrown away.
#[test]
fn delete_instance_is_one_delete_and_answers_the_operation() {
    let (client, script) = over(vec![answer(&operation_json("PENDING").to_string())]);
    let operation = client.delete_instance(&by_name()).unwrap();
    assert_eq!(operation.reference, operation_ref("operation-one"));
    assert_eq!(operation.status, OperationStatus::Pending);
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method.as_str(), "DELETE");
    assert_eq!(
        calls[0].url,
        format!("{}/instances/js-vm-one", zone_url(ZONE))
    );
    assert!(calls[0].body.is_empty());
}

/// Asserts that `get_operation` is one GET of the zone's operation named by the
/// reference, the name percent-encoded in the path. It catches a name that is spliced
/// into the path as it is.
#[test]
fn get_operation_is_one_get_of_the_named_operation() {
    let (client, script) = over(vec![
        answer(&operation_json("DONE").to_string()),
        answer(&operation_json("DONE").to_string()),
    ]);
    client
        .get_operation(&operation_ref("operation-one"))
        .unwrap();
    client.get_operation(&operation_ref("op/1 x")).unwrap();
    let calls = api_calls(&script);
    assert_eq!(calls[0].method.as_str(), "GET");
    assert_eq!(
        calls[0].url,
        format!("{}/operations/operation-one", zone_url(ZONE))
    );
    assert_eq!(
        calls[1].url,
        format!("{}/operations/op%2F1%20x", zone_url(ZONE))
    );
}

fn list_filter(labels: Vec<Label>, token: Option<&str>) -> ListFilter {
    ListFilter {
        project: project(),
        labels,
        page_token: token.map(str::to_owned),
    }
}

/// Asserts that `list_instances` is one GET of the project's aggregated instances with the
/// label terms joined by AND as the `filter`, the page token as `pageToken` when given, and
/// `returnPartialSuccess=true`, all percent-encoded and sorted; reads the instances of
/// every scope (two zones), skips a scope that holds a warning instead of instances, and
/// answers the next page token. It catches a filter that is not encoded, a list that reads
/// one scope only, and a page token that is dropped.
#[test]
fn list_instances_reads_every_scope_and_sends_the_filter_and_token() {
    let body = json!({
        "items": {
            "zones/region-one-a": {"instances": [
                instance_json("1234567890123456789", "js-vm-one", "RUNNING", "region-one-a"),
            ]},
            "zones/region-one-b": {"instances": [
                instance_json("1234567890123456790", "js-vm-two", "STAGING", "region-one-b"),
                instance_json("1234567890123456791", "js-vm-three", "TERMINATED", "region-one-b"),
            ]},
            "zones/region-one-c": {"warning": {"code": "NO_RESULTS_ON_PAGE"}},
        },
        "nextPageToken": "page-3",
    });
    let (client, script) = over(vec![answer(&body.to_string())]);
    let filter = list_filter(
        vec![
            label("js-scheduler", "sched-one"),
            label("js-acquire-id", "acquire-1"),
        ],
        Some("page-2"),
    );
    let page = client.list_instances(&filter).unwrap();
    let calls = api_calls(&script);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method.as_str(), "GET");
    assert_eq!(
        calls[0].url,
        format!(
            "{COMPUTE_BASE}/projects/sample-project/aggregated/instances?filter=labels.js-scheduler%20%3D%20%22sched-one%22%20AND%20labels.js-acquire-id%20%3D%20%22acquire-1%22&pageToken=page-2&returnPartialSuccess=true"
        )
    );
    let names: Vec<(&str, &str)> = page
        .items
        .iter()
        .map(|instance| (instance.name.as_str(), instance.zone.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("js-vm-one", "region-one-a"),
            ("js-vm-two", "region-one-b"),
            ("js-vm-three", "region-one-b"),
        ]
    );
    assert_eq!(page.items[2].status, InstanceStatus::Terminated);
    assert_eq!(page.next_page_token.as_deref(), Some("page-3"));
    assert!(!page.unreachable);
}

/// Asserts that a list with no labels sends no `filter`, and no page token sends no
/// `pageToken`; that an answer with unreachable scopes is a page marked unreachable; that
/// an empty next page token and an absent one both end the list; and that an answer with
/// no `items` is an empty page. It catches an empty filter term sent to the cloud, and
/// an unreachable scope that goes unreported.
#[test]
fn list_instances_reports_unreachable_scopes_and_the_end_of_the_list() {
    let (client, script) = over(vec![
        answer(
            &json!({
                "items": {"zones/region-one-a": {"warning": {"code": "UNREACHABLE"}}},
                "nextPageToken": "",
                "unreachables": ["zones/region-one-b"],
            })
            .to_string(),
        ),
        answer("{}"),
        answer(&json!({"unreachables": []}).to_string()),
    ]);
    let first = client
        .list_instances(&list_filter(Vec::new(), None))
        .unwrap();
    assert_eq!(
        first,
        Page {
            items: Vec::new(),
            next_page_token: None,
            unreachable: true,
        }
    );
    let second = client
        .list_instances(&list_filter(Vec::new(), None))
        .unwrap();
    assert_eq!(
        second,
        Page {
            items: Vec::new(),
            next_page_token: None,
            unreachable: false,
        }
    );
    let third = client
        .list_instances(&list_filter(Vec::new(), None))
        .unwrap();
    assert!(!third.unreachable);
    assert_eq!(
        api_calls(&script)[0].url,
        format!(
            "{COMPUTE_BASE}/projects/sample-project/aggregated/instances?returnPartialSuccess=true"
        )
    );
}

/// Asserts that a list answer whose `items` is not an object, whose scope holds
/// `instances` that is not a list, or which holds an instance that cannot be read, and one
/// that is not JSON, are each `Unreadable`, never a short page. It catches a list that
/// drops what it cannot read.
#[test]
fn a_list_answer_that_cannot_be_read_is_unreadable() {
    let bad_row = json!({"id": "1234567890123456789"});
    let bodies = [
        json!({"items": []}).to_string(),
        json!({"items": {"zones/region-one-a": {"instances": "none"}}}).to_string(),
        json!({"items": {"zones/region-one-a": {"instances": [bad_row]}}}).to_string(),
        "not json".to_owned(),
    ];
    for body in bodies {
        let (client, _) = over(vec![answer(&body)]);
        let result = client.list_instances(&list_filter(Vec::new(), None));
        assert!(is_unreadable(&result), "{body}");
    }
}

/// The insert body of an on-demand request for the N1 type `n1-standard-8` with
/// `attached` accelerators (the type name and the number), as the client sends it.
fn n1_body(attached: Option<(&str, u32)>) -> Value {
    let mut request = insert_request(on_demand(), "payload");
    request.machine_type = MachineTypeName("n1-standard-8".to_owned());
    request.accelerators = attached.map(|(name, count)| AttachedAccelerator {
        type_name: AcceleratorTypeName(name.to_owned()),
        count,
    });
    let (client, script) = over(vec![answer(&operation_json("PENDING").to_string())]);
    client.insert_instance(&request).unwrap();
    serde_json::from_slice(&api_calls(&script)[0].body).unwrap()
}

/// Asserts for every count of the attachable accelerator `name` that the whole insert body
/// is the N1 instance with `guestAccelerators` holding one entry: the zone-relative path of
/// the accelerator type and the number as a JSON number, and the `TERMINATE` host
/// maintenance an instance with accelerators needs.
fn assert_attached_body(name: &str, counts: &[u32]) {
    for count in counts {
        assert_eq!(
            n1_body(Some((name, *count))),
            json!({
                "name": "js-vm-one",
                "machineType": "zones/region-one-a/machineTypes/n1-standard-8",
                "disks": [{
                    "boot": true,
                    "autoDelete": true,
                    "initializeParams": {
                        "sourceImage": "projects/image-project/global/images/js-agent-x86",
                    },
                }],
                "networkInterfaces": [{
                    "network": "global/networks/default",
                    "subnetwork": "regions/region-one/subnetworks/default",
                }],
                "guestAccelerators": [{
                    "acceleratorType": format!("zones/region-one-a/acceleratorTypes/{name}"),
                    "acceleratorCount": count,
                }],
                "scheduling": {
                    "provisioningModel": "STANDARD",
                    "onHostMaintenance": "TERMINATE",
                    "automaticRestart": false,
                },
                "labels": {"js-scheduler": "sched-one", "js-acquire-id": "acquire-1"},
                "metadata": {"items": [{"key": "user-data", "value": "payload"}]},
            }),
            "{name} x {count}"
        );
    }
}

/// Asserts that an insert of the N1 type with 1, 2 or 4 attached `nvidia-tesla-t4` sends
/// that count and the type path in `guestAccelerators`. Catches a count sent as a string,
/// and a type path with the wrong form.
#[test]
fn an_insert_with_attached_t4_sends_its_guest_accelerators() {
    assert_attached_body("nvidia-tesla-t4", &[1, 2, 4]);
}

/// Asserts that an insert of the N1 type with 1, 2 or 4 attached `nvidia-tesla-p4` sends
/// that count and the type path in `guestAccelerators`. Catches a count sent as a string,
/// and a type path with the wrong form.
#[test]
fn an_insert_with_attached_p4_sends_its_guest_accelerators() {
    assert_attached_body("nvidia-tesla-p4", &[1, 2, 4]);
}

/// Asserts that an insert of the N1 type with 1, 2, 4 or 8 attached `nvidia-tesla-v100`
/// sends that count and the type path in `guestAccelerators`. Catches a count sent as a
/// string, and a type path with the wrong form.
#[test]
fn an_insert_with_attached_v100_sends_its_guest_accelerators() {
    assert_attached_body("nvidia-tesla-v100", &[1, 2, 4, 8]);
}

/// Asserts that an insert with no attached accelerator, which is what a CPU-only offer and
/// an offer of a type that carries its accelerators make, has no `guestAccelerators` key.
/// Catches an empty list sent for a type whose accelerators are part of the machine type.
#[test]
fn an_insert_with_no_attached_accelerator_sends_no_guest_accelerators() {
    let body = n1_body(None);
    assert!(body.get("guestAccelerators").is_none(), "{body}");
}
