//! The launch side over Compute Engine: `instances.insert`, `instances.get`,
//! `instances.delete`, `instances.aggregatedList` and `zoneOperations.get`. The cloud's JSON
//! is read into the records of [`crate::launch`].

use js_logstore::Method;
use js_logstore::sigv4::uri_encode;
use serde_json::{Map, Value, json};

use super::{COMPUTE_BASE, HttpGce, last_segment, unreadable};
use crate::error::{ApiError, ErrorReason};
use crate::launch::{
    InsertRequest, Instance, InstanceId, InstanceRef, InstanceStatus, Label, ListFilter, Operation,
    OperationRef, OperationStatus, PAYLOAD_KEY, Page,
};
use crate::model::{MachineTypeName, Project, Zone};

impl HttpGce {
    /// `instances.insert` for one instance: the operation the cloud answers with. The
    /// request id is the `requestId` query parameter, so a repeat is the same insert. The
    /// boot payload is written into the body byte for byte and is logged nowhere.
    pub(super) fn insert(&self, request: &InsertRequest) -> Result<Operation, ApiError> {
        let url = instances_url(&request.project, &request.zone);
        let query = [("requestId", request.request_id.as_str().to_owned())];
        let body = insert_body(request);
        let answer = self.request(Method::Post, &url, &query, Some(&body))?;
        operation(&request.project, &answer)
    }

    /// `instances.get` for one instance.
    pub(super) fn read_instance(&self, instance: &InstanceRef) -> Result<Instance, ApiError> {
        self.get_json(&instance_url(instance), &[])
            .and_then(|answer| instance_of(&answer))
    }

    /// `instances.delete` for one instance: the operation the cloud answers with.
    pub(super) fn delete(&self, instance: &InstanceRef) -> Result<Operation, ApiError> {
        let answer = self.request(Method::Delete, &instance_url(instance), &[], None)?;
        operation(&instance.project, &answer)
    }

    /// One page of `instances.aggregatedList` for the instances `filter` selects. The call
    /// asks for a partial success, so a scope the cloud cannot reach is named in
    /// `unreachables` (which the page reports) instead of failing the call.
    pub(super) fn list(&self, filter: &ListFilter) -> Result<Page<Instance>, ApiError> {
        let url = format!(
            "{COMPUTE_BASE}/projects/{}/aggregated/instances",
            filter.project
        );
        let terms: Vec<String> = filter
            .labels
            .iter()
            .map(|label| format!("labels.{} = \"{}\"", label.key, label.value))
            .collect();
        let mut query = vec![("returnPartialSuccess", "true".to_owned())];
        query.extend((!terms.is_empty()).then(|| ("filter", terms.join(" AND "))));
        query.extend(
            filter
                .page_token
                .as_ref()
                .map(|token| ("pageToken", token.clone())),
        );
        let answer = self.get_json(&url, &query)?;
        let scopes = answer
            .get("items")
            .map(|items| {
                items
                    .as_object()
                    .ok_or_else(|| unreadable("items is not an object"))
            })
            .transpose()?;
        let items = scopes
            .into_iter()
            .flat_map(Map::values)
            .filter_map(|scope| scope.get("instances"))
            .map(|rows| {
                rows.as_array()
                    .ok_or_else(|| unreadable("instances is not a list"))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .map(instance_of)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            items,
            next_page_token: answer
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
            unreachable: answer
                .get("unreachables")
                .and_then(Value::as_array)
                .is_some_and(|scopes| !scopes.is_empty()),
        })
    }

    /// `zoneOperations.get` for one operation.
    pub(super) fn read_operation(&self, reference: &OperationRef) -> Result<Operation, ApiError> {
        let url = format!(
            "{COMPUTE_BASE}/projects/{}/zones/{}/operations/{}",
            reference.project,
            reference.zone,
            uri_encode(&reference.name, false)
        );
        let answer = self.get_json(&url, &[])?;
        operation(&reference.project, &answer)
    }
}

/// The URL of the instances of a zone.
fn instances_url(project: &Project, zone: &Zone) -> String {
    format!("{COMPUTE_BASE}/projects/{project}/zones/{zone}/instances")
}

/// The URL of one instance.
fn instance_url(instance: &InstanceRef) -> String {
    format!(
        "{}/{}",
        instances_url(&instance.project, &instance.zone),
        instance.instance.as_str()
    )
}

/// The body of `instances.insert`. The instance has a boot disk that is deleted with it, one
/// network interface with no external address, and the labels and metadata of the request.
fn insert_body(request: &InsertRequest) -> Value {
    let zone = request.zone.as_str();
    let mut disk = Map::new();
    disk.insert("sourceImage".to_owned(), json!(request.image.as_str()));
    disk.extend(
        request
            .disk_size_gb
            .map(|size| ("diskSizeGb".to_owned(), json!(size.to_string()))),
    );
    let scheduling = &request.scheduling;
    let mut schedule = Map::new();
    schedule.insert(
        "provisioningModel".to_owned(),
        json!(scheduling.provisioning_model.name()),
    );
    schedule.insert(
        "onHostMaintenance".to_owned(),
        json!(scheduling.on_host_maintenance.name()),
    );
    schedule.insert(
        "automaticRestart".to_owned(),
        json!(scheduling.automatic_restart),
    );
    schedule.extend(
        scheduling
            .termination_action
            .map(|action| ("instanceTerminationAction".to_owned(), json!(action.name()))),
    );
    schedule.extend(scheduling.max_run_duration.map(|limit| {
        (
            "maxRunDuration".to_owned(),
            json!({"seconds": limit.as_secs().to_string()}),
        )
    }));
    let labels: Map<String, Value> = request
        .labels
        .iter()
        .map(|label| (label.key.clone(), json!(label.value)))
        .collect();
    let mut body = Map::new();
    body.insert("name".to_owned(), json!(request.name.as_str()));
    body.insert(
        "machineType".to_owned(),
        json!(format!(
            "zones/{zone}/machineTypes/{}",
            request.machine_type
        )),
    );
    body.insert(
        "disks".to_owned(),
        json!([{
            "boot": true,
            "autoDelete": true,
            "initializeParams": disk,
        }]),
    );
    body.insert(
        "networkInterfaces".to_owned(),
        json!([{
            "network": request.network.network,
            "subnetwork": request.network.subnetwork,
        }]),
    );
    body.extend(request.accelerators.as_ref().map(|accelerator| {
        (
            "guestAccelerators".to_owned(),
            json!([{
                "acceleratorType":
                    format!("zones/{zone}/acceleratorTypes/{}", accelerator.type_name),
                "acceleratorCount": accelerator.count,
            }]),
        )
    }));
    body.insert("scheduling".to_owned(), Value::Object(schedule));
    body.insert("labels".to_owned(), Value::Object(labels));
    body.insert(
        "metadata".to_owned(),
        json!({"items": [{"key": PAYLOAD_KEY, "value": request.payload.expose()}]}),
    );
    Value::Object(body)
}

/// The text field `key` of `value`.
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| unreadable(&format!("{key} is missing")))
}

/// The zone a URL of the cloud names (`.../zones/<zone>`).
fn zone_of(url: &str) -> Result<Zone, ApiError> {
    Zone::new(last_segment(url)).map_err(|_| unreadable("zone is not a zone name"))
}

/// The instance an `instances.get` answer or an `instances` row of an aggregated list
/// holds. An instance with no `labels` has none.
fn instance_of(value: &Value) -> Result<Instance, ApiError> {
    let labels = value
        .get("labels")
        .map(|labels| {
            let labels = labels
                .as_object()
                .ok_or_else(|| unreadable("labels is not an object"))?;
            labels
                .iter()
                .map(|(key, label)| {
                    label
                        .as_str()
                        .map(|value| Label {
                            key: key.clone(),
                            value: value.to_owned(),
                        })
                        .ok_or_else(|| unreadable("a label is not text"))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(Instance {
        id: InstanceId::new(text(value, "id")?).ok_or_else(|| unreadable("id is not an id"))?,
        name: text(value, "name")?.to_owned(),
        status: InstanceStatus::parse(text(value, "status")?),
        labels: labels.unwrap_or_default(),
        machine_type: MachineTypeName(last_segment(text(value, "machineType")?).to_owned()),
        zone: zone_of(text(value, "zone")?)?,
    })
}

/// The reasons an operation's `error.errors` lists, in order.
fn error_codes(error: &Value) -> Result<Vec<ErrorReason>, ApiError> {
    error
        .get("errors")
        .and_then(Value::as_array)
        .ok_or_else(|| unreadable("error.errors is missing"))?
        .iter()
        .map(|entry| text(entry, "code").map(ErrorReason::new))
        .collect()
}

/// The operation of `project` an answer describes.
fn operation(project: &Project, value: &Value) -> Result<Operation, ApiError> {
    let status = OperationStatus::parse(text(value, "status")?)
        .ok_or_else(|| unreadable("status is not an operation status"))?;
    let target_id = value
        .get("targetId")
        .map(|id| {
            id.as_str()
                .and_then(InstanceId::new)
                .ok_or_else(|| unreadable("targetId is not an id"))
        })
        .transpose()?;
    let codes = value.get("error").map(error_codes).transpose()?;
    Ok(Operation {
        reference: OperationRef {
            project: project.clone(),
            zone: zone_of(text(value, "zone")?)?,
            name: text(value, "name")?.to_owned(),
        },
        status,
        target_id,
        error_codes: codes.unwrap_or_default(),
    })
}
