//! Pure conversions between this server's vocabulary (`js_types`: [`Shape`], [`Request`],
//! [`HostId`], [`LeaseId`]) and the scheduler's (`js_sched` and `js_types::sched`).
//!
//! The two vocabularies keep different types on purpose (`js_types::sched` says so), so
//! each conversion here says what it keeps and what it drops. None reads a clock or does
//! I/O.
//!
//! | This server | The scheduler |
//! |---|---|
//! | `Resources` cpu, memory, gpus | `sched::Resources` (a booking vector) |
//! | `Resources` gpu class | the `gpu.kind` platform key, on the host's `NodeCaps` and on the request's `needs` |
//! | `Shape` | the host's capacity and the `NodeCaps` its node report would give |
//! | `HostId` | `WorkerId`, same text |
//! | `LeaseId` | `sched::LeaseId`, printed `<term>-<seq>` |

use std::collections::BTreeMap;

use js_caps::{NodeCaps, ReportError, RequestError};
use js_sched::Request as SchedRequest;
use js_types::sched::{LeaseId as SchedLeaseId, LeaseKind, Qos, WorkKey, WorkerId};
use js_types::{HostId, IdError, Request, Resources, Shape};

/// The driver a shape's node reports: jobs run in containers.
const DRIVER: &str = "container";

/// The platform key that carries a GPU class.
const GPU_KIND: &str = "gpu.kind";

/// Report keys a shape's resources decide; a capability of the same name is ignored.
const FROM_RESOURCES: [&str; 4] = ["cpus", "mem_gib", "gpu", GPU_KIND];

/// Why a conversion was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MapError {
    /// The shape's capabilities are not a node report.
    #[error("shape does not describe a node: {0}")]
    Report(#[from] ReportError),
    /// The request's capabilities are not platform requirements.
    #[error("request capabilities are not platform requirements: {0}")]
    Needs(#[from] RequestError),
    /// The request names its GPU class twice, differently.
    #[error("gpu class {class:?} conflicts with capability {GPU_KIND}={capability:?}")]
    GpuClass {
        /// The class in the resources.
        class: String,
        /// The value of the capability.
        capability: String,
    },
    /// A worker name is not a host id.
    #[error(transparent)]
    Host(#[from] IdError),
    /// The text is not a lease id in the form [`lease_text`] prints.
    #[error("lease id {0:?} is not <term>-<seq>")]
    LeaseText(String),
}

/// The booking vector of `resources`: CPU, memory, GPU count and disk. The GPU class has no
/// place in it; see [`needs`] and [`node_caps`].
pub(crate) fn booking(resources: &Resources) -> js_types::sched::Resources {
    js_types::sched::Resources::new(resources.cpu_millis(), resources.memory_bytes())
        .with_gpus(resources.gpus())
        .with_disk_bytes(resources.disk_bytes())
}

/// What a host of `shape` offers the scheduler: its capacity, and the capabilities its
/// node report would carry.
///
/// The report is rendered from the shape: `cpus` (whole cores, rounded down), `mem_gib`
/// (whole GiB, rounded down), `gpu` and `gpu.kind` from its resources, the `container`
/// driver, and every capability entry the scheduler reads. A capability named like one
/// of those the resources decide is ignored. A shape must name its `arch`.
///
/// # Errors
/// [`MapError::Report`] when a capability is not a valid report entry (no `arch`, an unknown `arch`,
/// a countable entry that is not a whole number).
pub(crate) fn host(shape: &Shape) -> Result<(js_types::sched::Resources, NodeCaps), MapError> {
    let resources = shape.resources();
    let mut entries: Vec<(String, String)> = shape
        .capabilities()
        .iter()
        .filter(|(key, _)| !FROM_RESOURCES.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    entries.push(("drivers".into(), DRIVER.into()));
    entries.push(("cpus".into(), (resources.cpu_millis() / 1000).to_string()));
    entries.push((
        "mem_gib".into(),
        (resources.memory_bytes() >> 30).to_string(),
    ));
    entries.push(("gpu".into(), resources.gpus().to_string()));
    if let Some(class) = resources.gpu_class() {
        entries.push((GPU_KIND.into(), class.into()));
    }
    let caps = NodeCaps::from_report(entries.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
    Ok((booking(resources), caps))
}

/// What `request` requires of a node: its capabilities as platform requirements, and its
/// GPU class as `gpu.kind`.
///
/// # Errors
/// [`MapError::Needs`] when a capability is not a platform key;
/// [`MapError::GpuClass`] when a `gpu.kind` capability names another class than the
/// resources.
pub(crate) fn needs(request: &Request) -> Result<js_caps::Request, MapError> {
    let mut pairs: BTreeMap<&str, &str> = request
        .capabilities
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    if let Some(class) = request.resources.gpu_class() {
        match pairs.insert(GPU_KIND, class) {
            Some(other) if other != class => {
                return Err(MapError::GpuClass {
                    class: class.into(),
                    capability: other.into(),
                });
            }
            _ => {}
        }
    }
    Ok(js_caps::Request::parse(pairs)?)
}

/// The scheduler's request for `request` as a job named `dedup`.
///
/// The work key is the request's class and `dedup`. The request is an `action` lease at
/// `batch` urgency, not hermetic (a job may use the network, so a silent host
/// self-fences it) and not cacheable or joinable.
///
/// # Errors
/// As [`needs`].
pub(crate) fn job(request: &Request, dedup: &str) -> Result<SchedRequest, MapError> {
    Ok(SchedRequest {
        key: WorkKey::new(request.class.as_str(), dedup),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: booking(&request.resources),
        needs: needs(request)?,
        hermetic: false,
        do_not_cache: true,
    })
}

/// The scheduler's name for `host`: the same text.
pub(crate) fn worker(host: &HostId) -> WorkerId {
    WorkerId::new(host.as_str())
}

/// The host a worker names.
///
/// # Errors
/// [`MapError::Host`] when the worker's name is not a host id.
pub(crate) fn host_id(worker: &WorkerId) -> Result<HostId, MapError> {
    Ok(HostId::new(worker.as_str())?)
}

/// The text of `lease` in the API: `<term>-<seq>`.
pub(crate) fn lease_text(lease: SchedLeaseId) -> String {
    format!("{}-{}", lease.term, lease.seq)
}

/// The lease `text` names, only in the exact form [`lease_text`] prints: decimal digits
/// without sign, space or leading zeros on each side of one `-`.
///
/// # Errors
/// [`MapError::LeaseText`] for anything else.
pub(crate) fn lease_id(text: &str) -> Result<SchedLeaseId, MapError> {
    let refuse = || MapError::LeaseText(text.to_owned());
    let (term, seq) = text.split_once('-').ok_or_else(refuse)?;
    let id = SchedLeaseId::new(
        term.parse().map_err(|_| refuse())?,
        seq.parse().map_err(|_| refuse())?,
    );
    if lease_text(id) == text {
        Ok(id)
    } else {
        Err(refuse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_types::{Capabilities, ClassId};
    use std::time::Duration;

    fn resources(cpu: u64, mem: u64, gpus: u64, class: Option<&str>) -> Resources {
        Resources::new(cpu, mem, gpus, class.map(str::to_owned)).unwrap()
    }

    fn request(resources: Resources, capabilities: &[(&str, &str)]) -> Request {
        Request {
            class: ClassId::new("gpu-host").unwrap(),
            resources,
            capabilities: capabilities
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            max_wall: Duration::from_secs(600),
            labels: BTreeMap::new(),
            validation_run: None,
        }
    }

    fn shape(resources: Resources, capabilities: &[(&str, &str)]) -> Shape {
        // Every test shape names an arch unless it says another.
        let caps: Capabilities = [("arch", "x86_64")]
            .iter()
            .chain(capabilities)
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Shape::new(resources, caps)
    }

    /// The standard conformance host (8000 millis, 1 GiB, one `conformance-gpu`) books
    /// exactly `Resources::new(8000, 1 << 30).with_gpus(1)`, and its node reports the
    /// GPU, its class, the cores and the memory. Catches swapped or dropped axes.
    #[test]
    fn the_standard_host_maps_to_its_capacity_and_caps() {
        let id = HostId::new("h1").unwrap();
        let host_shape = shape(resources(8000, 1 << 30, 1, Some("conformance-gpu")), &[]);
        let (capacity, caps) = host(&host_shape).unwrap();
        assert_eq!(
            capacity,
            js_types::sched::Resources::new(8000, 1 << 30).with_gpus(1)
        );
        assert_eq!(caps.consumables[&js_caps::Consumable::Cpus], 8);
        assert_eq!(caps.consumables[&js_caps::Consumable::MemGib], 1);
        assert_eq!(caps.consumables[&js_caps::Consumable::Gpus], 1);
        assert_eq!(caps.exact["gpu.kind"], "conformance-gpu");
        assert!(
            LeaseKind::Action
                .drivers()
                .iter()
                .any(|d| caps.drivers.contains(*d))
        );
        assert_eq!(worker(&id).as_str(), "h1");
    }

    /// The booking keeps cpu and memory in their own axes: unequal values land where
    /// they belong.
    #[test]
    fn booking_keeps_each_axis() {
        let got = booking(&resources(1500, 7 << 30, 2, Some("a100")));
        assert_eq!(got.cpu_millis, 1500);
        assert_eq!(got.memory_bytes, 7 << 30);
        assert_eq!(got.gpus, 2);
        assert_eq!(got.vms, 0);
    }

    /// Disk reaches the scheduler's ledger on both sides: a request's `disk_bytes` lands
    /// in its booking, a shape's lands in the host's capacity, and a request larger than
    /// the host's disk is not runnable on it though cpu and memory fit. Catches a mapping
    /// that copies cpu, memory and GPUs only, which leaves disk 0 on request and capacity,
    /// so nothing is ever refused for disk.
    #[test]
    fn lease_disk_reaches_ledger() {
        let got = booking(&resources(1, 2, 0, None).with_disk_bytes(9 << 30));
        assert_eq!(got.disk_bytes, 9 << 30);
        assert_eq!((got.cpu_millis, got.memory_bytes), (1, 2));
        let (capacity, caps) = host(&shape(
            resources(8000, 1 << 30, 0, None).with_disk_bytes(100 << 30),
            &[],
        ))
        .unwrap();
        assert_eq!(capacity.disk_bytes, 100 << 30);
        let fits = job(
            &request(resources(1000, 1, 0, None).with_disk_bytes(100 << 30), &[]),
            "j",
        )
        .unwrap();
        assert_eq!(fits.resources.disk_bytes, 100 << 30);
        assert!(fits.runnable_on(&caps, &capacity));
        let too_big = job(
            &request(
                resources(1000, 1, 0, None).with_disk_bytes((100 << 30) + 1),
                &[],
            ),
            "j",
        )
        .unwrap();
        assert!(!too_big.runnable_on(&caps, &capacity));
    }

    /// A request that needs the standard host's class is runnable on that host; one that
    /// needs another class is not, though the host has the count. Catches a GPU class
    /// that maps to nothing.
    #[test]
    fn a_class_the_host_lacks_is_not_runnable() {
        let (capacity, caps) = host(&shape(
            resources(8000, 1 << 30, 1, Some("conformance-gpu")),
            &[],
        ))
        .unwrap();
        let fits = job(
            &request(resources(1000, 1, 1, Some("conformance-gpu")), &[]),
            "j",
        )
        .unwrap();
        assert!(fits.runnable_on(&caps, &capacity));
        let wrong = job(&request(resources(1000, 1, 1, Some("h100")), &[]), "j").unwrap();
        assert_eq!(wrong.resources.gpus, 1);
        assert!(!wrong.runnable_on(&caps, &capacity));
        let cpu_only = job(&request(resources(1000, 1, 0, None), &[]), "j").unwrap();
        assert!(cpu_only.runnable_on(&caps, &capacity));
    }

    /// A job request carries its class and dedup text as the work key and the fixed job
    /// policy: action lease, batch, networked, uncached.
    #[test]
    fn a_job_request_has_the_job_policy() {
        let got = job(&request(resources(1000, 5, 0, None), &[]), "job-7").unwrap();
        assert_eq!(got.key, WorkKey::new("gpu-host", "job-7"));
        assert_eq!(got.qos, Qos::Batch);
        assert_eq!(got.kind, LeaseKind::Action);
        assert_eq!(got.resources, js_types::sched::Resources::new(1000, 5));
        assert!(!got.hermetic);
        assert!(got.do_not_cache);
        assert!(!got.joinable());
    }

    /// Capabilities travel both ways: a host's `os` satisfies a request for it and not a
    /// request for another; an `arch` in the shape decides what `arch` requests match.
    #[test]
    fn capabilities_are_matched() {
        let (capacity, caps) = host(&shape(
            resources(4000, 1 << 30, 0, None),
            &[("os", "linux"), ("arch", "arm64")],
        ))
        .unwrap();
        let ask = |pairs: &[(&str, &str)]| {
            job(&request(resources(1000, 1, 0, None), pairs), "j").unwrap()
        };
        assert!(ask(&[("os", "linux"), ("arch", "arm64")]).runnable_on(&caps, &capacity));
        assert!(!ask(&[("os", "macos")]).runnable_on(&caps, &capacity));
        assert!(!ask(&[("arch", "x86_64")]).runnable_on(&caps, &capacity));
        let (_, plain) = host(&shape(resources(4000, 1 << 30, 0, None), &[])).unwrap();
        assert!(ask(&[("arch", "x86_64")]).runnable_on(&plain, &capacity));
        assert!(!ask(&[("arch", "arm64")]).runnable_on(&plain, &capacity));
    }

    /// Bad capabilities are refused with a typed error, not dropped: a shape with an
    /// unknown arch or a non-numeric countable, a request with an unknown key, and a
    /// request whose `gpu.kind` capability contradicts its class.
    #[test]
    fn bad_capabilities_are_refused() {
        let none = resources(1000, 1 << 30, 0, None);
        assert!(matches!(
            host(&shape(none.clone(), &[("arch", "sparc")])),
            Err(MapError::Report(ReportError::UnknownArch(_)))
        ));
        assert!(matches!(
            host(&shape(none.clone(), &[("vm.slots", "many")])),
            Err(MapError::Report(ReportError::NotANumber { .. }))
        ));
        assert!(matches!(
            needs(&request(none, &[("flavour", "x")])),
            Err(MapError::Needs(RequestError::UnknownKey(_)))
        ));
        let gpu = resources(1000, 1, 1, Some("a100"));
        assert_eq!(
            needs(&request(gpu.clone(), &[("gpu.kind", "h100")])).unwrap_err(),
            MapError::GpuClass {
                class: "a100".into(),
                capability: "h100".into(),
            }
        );
        assert!(needs(&request(gpu, &[("gpu.kind", "a100")])).is_ok());
    }

    /// A shape that names no `arch` is refused, naming the key, rather than advertised
    /// as some architecture.
    #[test]
    fn a_shape_without_arch_is_refused() {
        let bare = Shape::new(resources(1000, 1 << 30, 0, None), Capabilities::new());
        let err = host(&bare).unwrap_err();
        assert_eq!(err, MapError::Report(ReportError::NoArch));
        assert!(err.to_string().contains("\"arch\""), "{err}");
    }

    /// Cores and memory are reported in whole units, rounded down: 1500 millis is one
    /// core and 1.5 GiB is one GiB.
    #[test]
    fn partial_cores_and_gib_round_down() {
        let (capacity, caps) = host(&shape(resources(1500, 3 << 29, 0, None), &[])).unwrap();
        assert_eq!(caps.consumables[&js_caps::Consumable::Cpus], 1);
        assert_eq!(caps.consumables[&js_caps::Consumable::MemGib], 1);
        assert_eq!(capacity.cpu_millis, 1500);
        assert_eq!(capacity.memory_bytes, 3 << 29);
    }

    /// A capability that names a key the resources decide does not override them.
    #[test]
    fn resources_decide_their_own_report_keys() {
        let (_, caps) = host(&shape(
            resources(2000, 2 << 30, 0, None),
            &[("cpus", "64"), ("gpu", "8"), ("gpu.kind", "x")],
        ))
        .unwrap();
        assert_eq!(caps.consumables[&js_caps::Consumable::Cpus], 2);
        assert_eq!(caps.consumables[&js_caps::Consumable::Gpus], 0);
        assert!(!caps.exact.contains_key("gpu.kind"));
    }

    /// A host id becomes a worker with the same text and back; a worker name that is not
    /// a host id is refused.
    #[test]
    fn host_and_worker_ids_round_trip() {
        for name in ["h1", "gpu-01.a_b", &"a".repeat(63)] {
            let host = HostId::new(name).unwrap();
            let w = worker(&host);
            assert_eq!(w.as_str(), name);
            assert_eq!(host_id(&w).unwrap(), host);
        }
        for bad in ["", "Upper", "has space", &"a".repeat(64)] {
            assert!(matches!(
                host_id(&WorkerId::new(bad)),
                Err(MapError::Host(_))
            ));
        }
    }

    /// A lease id prints as `<term>-<seq>` and reads back, at the extremes too.
    #[test]
    fn lease_ids_round_trip() {
        for (term, seq) in [(0, 0), (1, 9), (3, 12), (u64::MAX, u64::MAX)] {
            let id = SchedLeaseId::new(term, seq);
            let text = lease_text(id);
            assert_eq!(text, format!("{term}-{seq}"));
            assert_eq!(text, js_types::LeaseId::new(term, seq).to_string());
            assert_eq!(lease_id(&text).unwrap(), id);
        }
    }

    /// Anything but the printed form is refused, each with the text in the error:
    /// missing or extra separators, empty sides, signs, spaces, leading zeros, the dotted
    /// form of the scheduler's own printing, and overflow.
    #[test]
    fn malformed_lease_ids_are_refused() {
        for bad in [
            "",
            "-",
            "1",
            "1-",
            "-1",
            "1-2-3",
            "+1-2",
            "1-+2",
            " 1-2",
            "1-2 ",
            "01-2",
            "1-02",
            "1.2",
            "a-b",
            "-1-2",
            "18446744073709551616-1",
            "1-18446744073709551616",
        ] {
            assert_eq!(
                lease_id(bad),
                Err(MapError::LeaseText(bad.to_owned())),
                "{bad:?}"
            );
        }
    }
}
