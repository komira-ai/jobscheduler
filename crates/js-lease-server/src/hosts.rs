//! `GET /v1/hosts`: the hosts the server lists, whichever source they come from.
//!
//! [`listing`] reads them once for both the JSON route and the job service's `ListHosts`:
//! the route writes the [`HostList`] as its body, and the job service maps the same entries
//! to its own messages.

use std::collections::BTreeMap;

use js_agent_wire::Taint;
use js_registry::{HostMark, HostState, HostView};
use js_types::sched::usage::NodeLoad;
use js_types::{HostCapacity, HostId, MemoryHeadroom, Shape};

use crate::api::Shared;
use crate::sched::map::lease_text as sched_lease_text;
use serde::{Serialize, Serializer};

/// The body of `GET /v1/hosts`.
#[derive(Debug, Serialize)]
pub(crate) struct HostList {
    pub(crate) hosts: Vec<HostEntry>,
}

/// The shape of a host: what a lease on it holds.
#[derive(Debug, Serialize)]
pub(crate) struct ShapeView {
    pub(crate) cpu_millis: u64,
    pub(crate) memory_bytes: u64,
    pub(crate) gpus: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_class: Option<String>,
    /// Bytes of scratch disk the host books; left out when it books none, so a body for
    /// a host without disk reads as before.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) disk_bytes: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The memory of a host, or of a pool's floor, as an operator reads it: what was
/// configured, the percent of it that may be booked, and what that comes to (the
/// `memory_bytes` of a host's `shape`; the placement engine books no more than this).
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

/// Where a host comes from, written `static` or `registered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    /// The operator's pool file.
    Static,
    /// A host that said hello.
    Registered,
}

/// One host. `source` says where it comes from: `static` for the operator's pool file,
/// `registered` for a host that said hello. The remaining fields of a registered host
/// are what the registry knows of its session. The id, the source, the state and the mark
/// are held typed and written as text, so a reader other than the body reads them as they
/// are.
#[derive(Debug, Serialize)]
pub(crate) struct HostEntry {
    #[serde(serialize_with = "id_text")]
    pub(crate) id: HostId,
    pub(crate) source: Source,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "state_named"
    )]
    pub(crate) state: Option<HostState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lost: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    pub(crate) shape: ShapeView,
    pub(crate) memory: MemoryView,
    pub(crate) capabilities: BTreeMap<String, String>,
    pub(crate) labels: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_version: Option<String>,
    /// Set for a registered host: it has work to finish, so it takes no new work yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    releasing: Option<bool>,
    /// Set for a registered host that reported a taint: `{reason, run_id?, check?}`. It
    /// is sent no start and takes no new run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) taint: Option<Taint>,
    /// Set for a registered host that is set aside: `reserved` for a waiting whole-node
    /// request, `held` while a whole-node request is committed on it, `draining` and
    /// `releasing` while it is being scaled away.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "mark_named")]
    pub(crate) mark: Option<HostMark>,
    /// The request a reserved host is set aside for.
    #[serde(skip_serializing_if = "Option::is_none")]
    reserved_for: Option<String>,
    /// When the host a reservation is draining is expected to be drained at the latest, in
    /// milliseconds. Reaching it gives nothing up: the mark stays until its request is placed
    /// or cancelled, or the host goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    drain_deadline_ms: Option<u64>,
    /// Set for a registered host that is cordoned: when the cordon lapses. The host cordoned
    /// itself for an update of its agent; if the agent that did so is gone, the cordon
    /// lapses by itself within the last `lease_secs` it asked for (at most 3600 s from
    /// its last extension), or the host's own `:uncordon` lifts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cordon: Option<CordonListed>,
    /// Set for a registered host whose latest heartbeat carried a load: the reading of
    /// its actions control group.
    #[serde(skip_serializing_if = "Option::is_none")]
    load: Option<LoadView>,
}

/// The cordon of a listed host.
#[derive(Debug, Serialize)]
pub(crate) struct CordonListed {
    pub(crate) expires_at_ms: u64,
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
            id: host.host,
            source: Source::Static,
            state: None,
            lost: None,
            phase: None,
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
        Self {
            id: host.id,
            source: Source::Registered,
            state: Some(host.state),
            lost: Some(host.state == HostState::Lost),
            phase: Some(String::from(host.phase)),
            shape: shape_view(&host.shape),
            memory: MemoryView::of(host.memory),
            capabilities: host.shape.capabilities().clone(),
            labels: host.labels,
            agent_version: host.agent_version,
            releasing: Some(host.releasing),
            taint: host.taint,
            mark: host.mark,
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
    /// Every host of the static pool and every registered host, in id order. The registered
    /// hosts come from the registry's own view, which knows their session.
    pub(crate) fn of(statics: Vec<HostCapacity>, registered: Vec<HostView>) -> Self {
        let statics = statics.into_iter().map(HostEntry::listed);
        let mut hosts: Vec<HostEntry> = statics
            .chain(registered.into_iter().map(HostEntry::registered))
            .collect();
        hosts.sort_by(|a, b| a.id.cmp(&b.id));
        Self { hosts }
    }
}

/// Every host the server lists, the static pool's and the registered ones: what
/// `GET /v1/hosts` and the job service's `ListHosts` both read. The static pool's hosts are
/// those read from its file at start; the registered ones come from the registry, so a
/// registered host shows its session. It books, cordons and counts nothing.
pub(crate) fn listing(shared: &Shared) -> HostList {
    let statics = shared.static_hosts().to_vec();
    let registered = shared
        .agents()
        .map_or_else(Vec::new, |agents| agents.registry.hosts());
    HostList::of(statics, registered)
}

/// A host id as the body writes it: its text.
fn id_text<S: Serializer>(id: &HostId, out: S) -> Result<S::Ok, S::Error> {
    out.serialize_str(id.as_str())
}

/// A session's state as the body writes it ([`state_text`]); only called for a host that
/// has one.
fn state_named<S: Serializer>(state: &Option<HostState>, out: S) -> Result<S::Ok, S::Error> {
    out.serialize_str(state.map_or("", state_text))
}

/// A mark as the body writes it ([`mark_text`]); only called for a host that has one.
fn mark_named<S: Serializer>(mark: &Option<HostMark>, out: S) -> Result<S::Ok, S::Error> {
    out.serialize_str(mark.map_or("", mark_text))
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
            by_hello: false,
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

    /// Asserts the whole `GET /v1/hosts` body for a static host and a registered host that is
    /// reserved, has a drain deadline and is cordoned, by exact string equality: the keys,
    /// their order, and the absence of any lease key. It catches a field added, renamed or
    /// reordered in the hosts listing, and a lease field that comes back into it.
    #[test]
    fn the_hosts_body_is_pinned_byte_for_byte() {
        let mut reserved = view(Some(HostMark::Reserved));
        reserved.reserved_for = Some(js_types::RequestId::new("req-1").unwrap());
        reserved.drain_deadline = Some(js_types::Timestamp::from_millis(7000));
        reserved.cordon_until = Some(js_types::Timestamp::from_millis(9000));
        let resources = Resources::new(1000, 1 << 20, 0, None).unwrap();
        let listed = HostCapacity {
            host: HostId::new("s").unwrap(),
            shape: Shape::new(resources, BTreeMap::new()),
            memory: MemoryHeadroom::full(1 << 20),
            leased_to: None,
            leases: Vec::new(),
        };
        let list = HostList::of(vec![listed], vec![reserved]);
        let shape = r#"{"cpu_millis":1000,"memory_bytes":1048576,"gpus":0}"#;
        let memory =
            r#"{"configured_bytes":1048576,"schedulable_percent":100,"bookable_bytes":1048576}"#;
        let registered = format!(
            r#"{{"id":"a","source":"registered","state":"live","lost":false,"phase":"serving","shape":{shape},"memory":{memory},"capabilities":{{}},"labels":{{}},"releasing":false,"mark":"reserved","reserved_for":"req-1","drain_deadline_ms":7000,"cordon":{{"expires_at_ms":9000}}}}"#
        );
        let fixed = format!(
            r#"{{"id":"s","source":"static","shape":{shape},"memory":{memory},"capabilities":{{}},"labels":{{}}}}"#
        );
        assert_eq!(
            serde_json::to_string(&list).unwrap(),
            format!(r#"{{"hosts":[{registered},{fixed}]}}"#)
        );
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
            let list = HostList::of(Vec::new(), vec![view(mark)]);
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
        let list = HostList::of(Vec::new(), vec![with_disk, view(None)]);
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

    /// The listing of a server with a static pool and a registry shows the static pool's hosts
    /// as `static` and the registered hosts as `registered`, all in id order, and none of them
    /// carries a lease. A server without agents shows the static hosts alone. It catches a
    /// listing that leaves out either source or lists them in the order of their sources.
    #[test]
    fn the_listing_shows_the_static_hosts_and_the_registered_ones_in_id_order() {
        use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
        use js_types::{ManualClock, ProviderId, Timestamp};

        use crate::clock::SharedClock;
        use crate::config::parse_token;

        let pool = StaticPool::from_toml(ProviderId::new("static").unwrap(), EXAMPLE_CONFIG);
        let statics = pool.unwrap().hosts();
        let token = parse_token(&[b'q'; 40]).unwrap();
        let alone = Shared::new(token, None).with_static_hosts(statics.clone());
        let rows = |shared: &Shared| -> Vec<(String, Source)> {
            let list = listing(shared);
            list.hosts
                .iter()
                .map(|host| (host.id.to_string(), host.source))
                .collect()
        };
        let fixed = vec![
            ("gpu-01".to_owned(), Source::Static),
            ("gpu-02".to_owned(), Source::Static),
        ];
        assert_eq!(rows(&alone), fixed);

        let clock = SharedClock::new(ManualClock::new(Timestamp::from_millis(1_000)));
        let dir = js_testkit::dir::TestDir::under(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/hosts_unit"),
            "listing",
        );
        let resources = Resources::new(1000, 1 << 20, 0, None).unwrap();
        let pool = js_registry::PoolConfig::new(
            ProviderId::new("burst").unwrap(),
            js_types::ResourceClass::gpu_host(),
            Shape::new(resources, BTreeMap::new()),
            4,
        );
        let registry = js_registry::Registry::with_sessions(
            pool,
            clock.clone(),
            js_registry::SequentialSessions::new(),
            std::sync::Arc::new(js_sched_placer::place::SchedPlacer::default()),
        );
        let agents = crate::agents::Agents::over(registry, &clock, dir.path());
        let token = parse_token(&[b'q'; 40]).unwrap();
        let shared =
            Shared::new(token, Some(std::sync::Arc::new(agents))).with_static_hosts(statics);
        assert_eq!(rows(&shared), fixed);
    }
}
