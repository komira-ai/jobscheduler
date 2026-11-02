//! `GET /v1/hosts`: the hosts the server leases, whichever source they come from.

use std::collections::{BTreeMap, BTreeSet};

use js_agent_wire::Taint;
use js_provider::HostCapacity;
use js_registry::{HostMark, HostState, HostView};
use js_types::sched::usage::NodeLoad;
use js_types::{HostId, LeaseId, MemoryHeadroom, Shape};

use crate::sched::map::lease_text as sched_lease_text;
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
    /// Bytes of scratch disk the host books; left out when it books none, so a body for
    /// a host without disk reads as before.
    #[serde(skip_serializing_if = "is_zero")]
    disk_bytes: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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
    /// Every lease on the host, when it carries more than one (`lease` is the first of
    /// them); absent for a host with none or one, so those bodies are as they were.
    #[serde(skip_serializing_if = "at_most_one")]
    leases: Vec<String>,
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
    /// Set for a registered host that is cordoned: when the cordon lapses. The host cordoned
    /// itself for an update of its agent; if the agent that did so is gone, the cordon
    /// lapses by itself within the last `lease_secs` it asked for (at most 3600 s from
    /// its last extension), or the host's own `:uncordon` lifts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    cordon: Option<CordonListed>,
    /// Set for a registered host whose latest heartbeat carried a load: the reading of
    /// its actions control group.
    #[serde(skip_serializing_if = "Option::is_none")]
    load: Option<LoadView>,
}

/// The cordon of a listed host.
#[derive(Debug, Serialize)]
struct CordonListed {
    expires_at_ms: u64,
}

/// The latest load of a host, in the units its field names state. It is what the agent
/// measured and sent, not an estimate: CPU time is a counter, so a rate is the difference
/// of two readings over the difference of their `mono_usec`.
#[derive(Debug, Serialize)]
struct LoadView {
    /// The reading's number within the agent's process.
    seq: u64,
    /// The agent's monotonic clock at the reading, in microseconds.
    mono_usec: u64,
    /// Cumulative CPU time of the cgroup that holds every action, in microseconds.
    cpu_usage_usec: u64,
    /// Memory in use by the actions, in bytes.
    memory_working_set_bytes: u64,
    /// Active file cache, in bytes.
    memory_active_file_bytes: u64,
    /// Whether the actions cgroup has a CPU limit.
    actions_cpu_limited: bool,
    /// Leases (`term-seq`) that ran the whole window since the previous reading.
    covered: Vec<String>,
    /// When each running lease started, in microseconds on the agent's clock.
    lease_clocks: Vec<LeaseClockView>,
    /// Closed lease-minutes.
    minutes: Vec<LeaseMinuteView>,
}

#[derive(Debug, Serialize)]
struct LeaseClockView {
    lease: String,
    started_mono_usec: u64,
}

#[derive(Debug, Serialize)]
struct LeaseMinuteView {
    lease: String,
    minute: u32,
    /// Mean CPU over the observed span, in thousandths of a core.
    cpu_mean_millis: u32,
    /// The highest CPU rate of any window in the minute, in thousandths of a core.
    cpu_peak_window_millis: u32,
    /// The largest working set seen in the minute, in bytes.
    ws_max_bytes: u64,
}

impl LoadView {
    fn of(load: NodeLoad) -> Self {
        Self {
            seq: load.seq,
            mono_usec: load.mono_usec,
            cpu_usage_usec: load.cpu_usage_usec,
            memory_working_set_bytes: load.memory_working_set_bytes,
            memory_active_file_bytes: load.memory_active_file_bytes,
            actions_cpu_limited: load.actions_cpu_limited,
            covered: load.covered.into_iter().map(sched_lease_text).collect(),
            lease_clocks: load
                .lease_clocks
                .into_iter()
                .map(|clock| LeaseClockView {
                    lease: sched_lease_text(clock.lease),
                    started_mono_usec: clock.started_mono_usec,
                })
                .collect(),
            minutes: load
                .minutes
                .into_iter()
                .map(|minute| LeaseMinuteView {
                    lease: sched_lease_text(minute.lease),
                    minute: minute.minute,
                    cpu_mean_millis: minute.cpu_mean_millis,
                    cpu_peak_window_millis: minute.cpu_peak_window_millis,
                    ws_max_bytes: minute.ws_max_bytes,
                })
                .collect(),
        }
    }
}

fn shape_view(shape: &Shape) -> ShapeView {
    let resources = shape.resources();
    ShapeView {
        cpu_millis: resources.cpu_millis(),
        memory_bytes: resources.memory_bytes(),
        gpus: resources.gpus(),
        gpu_class: resources.gpu_class().map(str::to_string),
        disk_bytes: resources.disk_bytes(),
    }
}

fn lease_texts(leases: &[LeaseId]) -> Vec<String> {
    leases.iter().map(ToString::to_string).collect()
}

fn at_most_one(leases: &[String]) -> bool {
    leases.len() <= 1
}

fn lease_text(lease: Option<LeaseId>) -> Option<String> {
    lease.map(|lease| lease.to_string())
}

/// How a session is told in an answer: `awaiting`, `live` or `lost`.
pub(crate) fn state_text(state: HostState) -> &'static str {
    match state {
        HostState::Awaiting => "awaiting",
        HostState::Live => "live",
        HostState::Lost => "lost",
    }
}

/// How a set-aside host is told in an answer: `reserved`, `held`, `draining` or `releasing`.
pub(crate) fn mark_text(mark: HostMark) -> &'static str {
    match mark {
        HostMark::Reserved => "reserved",
        HostMark::Held => "held",
        HostMark::Draining => "draining",
        HostMark::Releasing => "releasing",
    }
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
            leases: lease_texts(&host.leases),
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
            cordon: None,
            load: None,
        }
    }

    fn registered(host: HostView) -> Self {
        let state = state_text(host.state);
        Self {
            id: host.id.to_string(),
            source: "registered",
            state: Some(state),
            lost: Some(host.state == HostState::Lost),
            phase: Some(String::from(host.phase)),
            lease: lease_text(host.lease),
            leases: lease_texts(&host.leases),
            shape: shape_view(&host.shape),
            memory: MemoryView::of(host.memory),
            capabilities: host.shape.capabilities().clone(),
            labels: host.labels,
            agent_version: host.agent_version,
            releasing: Some(host.releasing),
            taint: host.taint,
            mark: host.mark.map(mark_text),
            reserved_for: host.reserved_for.map(|id| id.to_string()),
            drain_deadline_ms: host.drain_deadline.map(|at| at.millis()),
            cordon: host.cordon_until.map(|at| CordonListed {
                expires_at_ms: at.millis(),
            }),
            load: host.load.map(LoadView::of),
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
            leases: Vec::new(),
            shape: Shape::new(resources, BTreeMap::new()),
            memory: MemoryHeadroom::full(1 << 20),
            claim: None,
            labels: BTreeMap::new(),
            agent_version: None,
            epoch: 1,
            heard_since_hello: true,
            releasing: false,
            taint: None,
            mark,
            reserved_for: None,
            drain_deadline: None,
            cordon_until: None,
            load: None,
        }
    }

    /// A host that carries several leases lists them all beside `lease` (the first), for a
    /// registered host and a static one; a host with one or none has no `leases` key, so its
    /// body is as it was. It catches a listing that shows only the first lease.
    #[test]
    fn a_host_with_several_leases_lists_them_all() {
        let ids = [LeaseId::new(1, 1), LeaseId::new(1, 2)];
        let mut many = view(None);
        many.lease = Some(ids[0]);
        many.leases = ids.to_vec();
        let mut one = view(None);
        one.id = HostId::new("b").unwrap();
        one.lease = Some(ids[0]);
        one.leases = vec![ids[0]];
        let resources = Resources::new(1000, 1 << 20, 0, None).unwrap();
        let capacity = |name: &str, leases: Vec<LeaseId>| HostCapacity {
            host: HostId::new(name).unwrap(),
            shape: Shape::new(resources.clone(), BTreeMap::new()),
            memory: MemoryHeadroom::full(1 << 20),
            leased_to: leases.first().copied(),
            leases,
        };
        let statics = vec![
            capacity("s2", ids.to_vec()),
            capacity("s1", ids[..1].to_vec()),
        ];
        let list = HostList::of(statics, vec![many, one], None);
        let body = serde_json::to_value(&list).unwrap();
        let by_id = |id: &str| {
            let hosts = body["hosts"].as_array().unwrap();
            hosts.iter().find(|host| host["id"] == id).unwrap().clone()
        };
        for id in ["a", "s2"] {
            assert_eq!(by_id(id)["lease"], "1-1");
            assert_eq!(by_id(id)["leases"], serde_json::json!(["1-1", "1-2"]));
        }
        for id in ["b", "s1"] {
            assert_eq!(by_id(id)["lease"], "1-1");
            assert!(by_id(id).get("leases").is_none());
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

    /// Asserts that a host that books scratch disk lists it as `shape.disk_bytes`, that a
    /// host that books none has no such key (so the body is what it was before), and that
    /// the value is the host's booked shape's. It catches a view that drops the disk,
    /// and one that writes `0` for every host.
    #[test]
    fn the_shape_lists_booked_disk_only_when_there_is_some() {
        let mut with_disk = view(None);
        let resources = Resources::new(1000, 1 << 20, 0, None)
            .unwrap()
            .with_disk_bytes(5 << 30);
        with_disk.shape = Shape::new(resources, BTreeMap::new());
        let list = HostList::of(Vec::new(), vec![with_disk, view(None)], None);
        let body = serde_json::to_value(&list).unwrap();
        let shapes: Vec<_> = body["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|host| host["shape"].clone())
            .collect();
        assert_eq!(shapes.len(), 2);
        let sized = shapes
            .iter()
            .filter(|shape| shape.get("disk_bytes").is_some());
        assert_eq!(sized.count(), 1);
        let disk = shapes.iter().find_map(|shape| shape.get("disk_bytes"));
        assert_eq!(disk.and_then(|disk| disk.as_u64()), Some(5 << 30));
    }
}
