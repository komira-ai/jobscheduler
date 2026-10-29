//! `GET /v1/hosts`: the hosts the server leases, whichever source they come from.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::Taint;
use js_provider::HostCapacity;
use js_registry::{HostMark, HostState, HostView};
use js_types::{HostId, LeaseId, MemoryHeadroom, Shape};
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

/// The memory of a host, or of a pool's floor, as an operator reads it: what was
/// configured, the percent of it that is booked, and what that comes to (the
/// `memory_bytes` of a host's `shape`).
#[derive(Debug, Serialize)]
pub(crate) struct MemoryView {
    pub(crate) configured_bytes: u64,
    pub(crate) schedulable_percent: u8,
    pub(crate) bookable_bytes: u64,
}

impl MemoryView {
    pub(crate) fn of(memory: MemoryHeadroom) -> Self {
        Self {
            configured_bytes: memory.configured_bytes(),
            schedulable_percent: memory.percent().get(),
            bookable_bytes: memory.bookable_bytes(),
        }
    }
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
    memory: MemoryView,
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
    /// Set for a registered host that is set aside: `reserved` for a waiting whole-node
    /// request, `held` while a whole-node lease is committed on it, `draining` and
    /// `releasing` while it is being scaled away.
    #[serde(skip_serializing_if = "Option::is_none")]
    mark: Option<&'static str>,
    /// The request a reserved host is set aside for.
    #[serde(skip_serializing_if = "Option::is_none")]
    reserved_for: Option<String>,
    /// When a reservation that is draining a busy host is given up, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    drain_deadline_ms: Option<u64>,
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
            memory: MemoryView::of(host.memory),
            capabilities: host.shape.capabilities().clone(),
            labels: BTreeMap::new(),
            agent_version: None,
            releasing: None,
            taint: None,
            mark: None,
            reserved_for: None,
            drain_deadline_ms: None,
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
            memory: MemoryView::of(host.memory),
            capabilities: host.shape.capabilities().clone(),
            labels: host.labels,
            agent_version: host.agent_version,
            releasing: Some(host.releasing),
            taint: host.taint,
            mark: host.mark.map(|mark| match mark {
                HostMark::Reserved => "reserved",
                HostMark::Held => "held",
                HostMark::Draining => "draining",
                HostMark::Releasing => "releasing",
            }),
            reserved_for: host.reserved_for.map(|id| id.to_string()),
            drain_deadline_ms: host.drain_deadline.map(|at| at.millis()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use js_agent_wire::Phase;
    use js_types::Resources;

    fn view(mark: Option<HostMark>) -> HostView {
        let resources = Resources::new(1000, 1 << 20, 0, None).unwrap();
        HostView {
            id: HostId::new("a").unwrap(),
            state: HostState::Live,
            phase: Phase::Serving,
            lease: None,
            shape: Shape::new(resources, BTreeMap::new()),
            memory: MemoryHeadroom::full(1 << 20),
            claim: None,
            labels: BTreeMap::new(),
            agent_version: None,
            epoch: 1,
            releasing: false,
            taint: None,
            mark,
            reserved_for: None,
            drain_deadline: None,
        }
    }

    /// Each mark a registered host can carry is listed under its own name, and a host with
    /// none has no `mark` field at all. It catches a mark added to the registry that the
    /// listing maps to another name, and a field that appears as `null` for a plain host,
    /// which would change the body existing clients read.
    #[test]
    fn each_mark_is_listed_by_its_name() {
        let cases = [
            (None, None),
            (Some(HostMark::Reserved), Some("reserved")),
            (Some(HostMark::Held), Some("held")),
            (Some(HostMark::Draining), Some("draining")),
            (Some(HostMark::Releasing), Some("releasing")),
        ];
        for (mark, listed) in cases {
            let list = HostList::of(Vec::new(), vec![view(mark)], None);
            let body = serde_json::to_value(&list).unwrap();
            let host = &body["hosts"][0];
            assert_eq!(host["id"], "a");
            assert_eq!(host["source"], "registered");
            assert_eq!(host.get("mark").and_then(|m| m.as_str()), listed);
            assert_eq!(host.get("mark").is_none(), listed.is_none());
        }
    }
}
