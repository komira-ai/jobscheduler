//! `GET /v1/hosts`: the hosts the server leases, whichever source they come from.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::Taint;
use js_provider::HostCapacity;
use js_registry::{HostState, HostView};
use js_types::{HostId, LeaseId, Shape};
use serde::Serialize;

/// The body of `GET /v1/hosts`.
#[derive(Debug, Serialize)]
pub(crate) struct HostList {
    hosts: Vec<HostEntry>,
}

/// The shape of a host: what a lease on it holds.
#[derive(Debug, Serialize)]
struct ShapeView {
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu_class: Option<String>,
}

/// One host. `source` says where it comes from: `static` for the operator's pool file,
/// `registered` for a host that said hello. The remaining fields of a registered host
/// are what the registry knows of its session.
#[derive(Debug, Serialize)]
struct HostEntry {
    id: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lost: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease: Option<String>,
    shape: ShapeView,
    capabilities: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_version: Option<String>,
    /// Set for a registered host: it holds no lease but has runs to finish, so it takes
    /// no new lease yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    releasing: Option<bool>,
    /// Set for a registered host that reported a taint: `{reason, run_id?, check?}`. It
    /// is sent no start and takes no new run.
    #[serde(skip_serializing_if = "Option::is_none")]
    taint: Option<Taint>,
}

fn shape_view(shape: &Shape) -> ShapeView {
    let resources = shape.resources();
    ShapeView {
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        gpus: resources.gpus(),
        gpu_class: resources.gpu_class().map(str::to_string),
    }
}

fn lease_text(lease: Option<LeaseId>) -> Option<String> {
    lease.map(|lease| lease.to_string())
}

impl HostEntry {
    fn listed(host: HostCapacity) -> Self {
        Self {
            id: host.host.to_string(),
            source: "static",
            state: None,
            lost: None,
            phase: None,
            lease: lease_text(host.leased_to),
            shape: shape_view(&host.shape),
            capabilities: host.shape.capabilities().clone(),
            labels: BTreeMap::new(),
            agent_version: None,
            releasing: None,
            taint: None,
        }
    }

    fn registered(host: HostView) -> Self {
        let state = match host.state {
            HostState::Awaiting => "awaiting",
            HostState::Live => "live",
            HostState::Lost => "lost",
        };
        Self {
            id: host.id.to_string(),
            source: "registered",
            state: Some(state),
            lost: Some(host.state == HostState::Lost),
            phase: Some(String::from(host.phase)),
            lease: lease_text(host.lease),
            shape: shape_view(&host.shape),
            capabilities: host.shape.capabilities().clone(),
            labels: host.labels,
            agent_version: host.agent_version,
            releasing: Some(host.releasing),
            taint: host.taint,
        }
    }
}

impl HostList {
    /// Every host of the static pool and every registered host, in id order.
    ///
    /// `capacity` is what the provider lists, which holds both kinds; the static hosts
    /// are those of it that `reserved` names (the ids the registry keeps for the static
    /// pool), or all of it when there is no registry. The registered hosts come from the
    /// registry's own view, which knows their session.
    pub(crate) fn of(
        capacity: Vec<HostCapacity>,
        registered: Vec<HostView>,
        reserved: Option<&BTreeSet<HostId>>,
    ) -> Self {
        let is_static = |host: &HostCapacity| reserved.is_none_or(|ids| ids.contains(&host.host));
        let statics = capacity
            .into_iter()
            .filter(is_static)
            .map(HostEntry::listed);
        let mut hosts: Vec<HostEntry> = statics
            .chain(registered.into_iter().map(HostEntry::registered))
            .collect();
        hosts.sort_by(|a, b| a.id.cmp(&b.id));
        Self { hosts }
    }
}
