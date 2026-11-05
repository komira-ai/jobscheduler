//! The `default` container platform over its machine providers and the host table of its
//! `static` one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use std::time::Duration;

use js_agent_wire::{Hello, Phase, ProtocolRange, Taint, WireShape};
use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, Held, HostRef, Observed, Offer,
    ProviderCaps, ProviderError, Released,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::DefaultPlatform;
use js_platform::{ContainerPlatform, PlatformId};
use js_provider::{Pool, StartAck, StartSpec};
use js_registry::{Binding, Credential, PoolConfig, Registry, StaticCapacity};
use js_sched::admission_ever::{
    Admission as AdmitVerdict, CatalogView, HostState, HostView, PoolLimits, ProviderView,
    can_ever_serve,
};
use js_types::{
    AcceleratorNeed, Admission, Arch, ClassId, ExecMode, HostId, IsaLevel, JobNetwork, LeaseId,
    Model, Os, ProviderId, Request, Requirements, ResourceClass, Resources, Shape,
};

/// The capabilities every host of the test pool reports and the pool books.
fn facts() -> BTreeMap<String, String> {
    [
        ("os", "linux"),
        ("arch", "x86_64"),
        ("disk-gib", "100"),
        ("isa_level", "x86-64-v3"),
        ("exec", "container"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// The class of the test pool: a whole host each.
fn rack() -> ResourceClass {
    ResourceClass::new(ClassId::new("rack").unwrap(), Admission::BookAll, 0)
}

/// A host table of a pool of 4 cores and 8 GiB per host, with `hosts` registered.
fn table(hosts: &[&str]) -> Registry {
    table_on(hosts, ManualClock::new(Timestamp::from_millis(1)))
}

/// [`table`], on `clock`.
fn table_on(hosts: &[&str], clock: ManualClock) -> Registry {
    let shape = Shape::new(Resources::new(4000, 8 << 30, 0, None).unwrap(), facts());
    let config = PoolConfig::new(ProviderId::new("rack").unwrap(), rack(), shape, 4);
    let registry = Registry::new(config, clock);
    for host in hosts {
        let hello = Hello {
            protocol: ProtocolRange { min: 1, max: 1 },
            host_id: (*host).into(),
            pool: "rack".into(),
            agent_version: "0.1.0".into(),
            instance: None,
            shape: WireShape {
                cpu_millis: 8000,
                memory_bytes: 16 << 30,
                gpus: 0,
                disk_bytes: 0,
                gpu_class: None,
                max_leases: None,
                accelerator: None,
            },
            capabilities: facts(),
            gpu_devices: Vec::new(),
            labels: BTreeMap::new(),
            phase: Phase::Serving,
            held: Vec::new(),
            accepts_host_credential: false,
        };
        registry.hello(&hello, &Credential::of_token("t")).unwrap();
    }
    registry
}

/// A launching provider that says it could offer `caps` and launches nothing.
struct Cloud(ProviderCaps);

impl CapacityProvider for Cloud {
    fn id(&self) -> ProviderId {
        ProviderId::known("cloud")
    }

    fn can_launch(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCaps {
        self.0.clone()
    }

    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        Err(ProviderError::new("the cloud's API did not answer"))
    }

    fn capacity(&self) -> Capacity {
        Capacity {
            supplied: 0,
            can_add: None,
        }
    }

    fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
        Vec::new()
    }

    fn payload_limit(&self) -> usize {
        0
    }

    fn acquire(&self, _request: &AcquireRequest) -> Acquired {
        Acquired::Unknown
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        Released::Unknown
    }

    fn observe(&self, _id: &HostRef) -> Observed {
        Observed::Unknown
    }

    fn verify_instance(&self, _hello: &js_elastic::Hello) -> Option<HostRef> {
        None
    }
}

/// A cloud that offers Linux x86-64 machines with `nvidia-a10` GPUs, whole.
fn gpu_cloud() -> Arc<dyn CapacityProvider> {
    Arc::new(Cloud(ProviderCaps {
        os: BTreeSet::from(["linux".to_owned()]),
        arch: BTreeSet::from(["x86_64".to_owned(), "sparc".to_owned()]),
        accelerators: BTreeSet::from([Model::parse("nvidia-a10").unwrap()]),
        whole_host: true,
        host_ports: true,
    }))
}

fn wanting(cores: u64, gpus: u64) -> Requirements {
    let class = (gpus > 0).then(|| "nvidia-a10".to_owned());
    let resources = Resources::new(cores * 1000, 1 << 30, gpus, class).unwrap();
    Requirements {
        accelerator: AcceleratorNeed::booked(&resources),
        resources,
        disk_bytes: None,
        os: Some("linux".to_owned()),
        arch: None,
        isa_level: None,
        exec: None,
        network: JobNetwork::None,
        host_ports: Vec::new(),
    }
}

/// Asserts that the `default` platform could ever serve a job when a registered host could
/// hold it, whether or not a launching provider could offer one; that it could not when
/// no host could and no launching provider says it could (the `static` provider's pooled
/// capabilities do not count); that a launching provider whose catalog cannot be read is
/// enough within its capabilities;
/// that a request naming every capability a host reports (disk in whole GiB, rounded up) is
/// judged on all of them; and that its capabilities are the union of its providers'. It
/// catches a `can_ever_serve` that ignores either source.
#[test]
fn default_platform_can_ever_serve_follows_hosts_and_provider_caps() {
    let hosts = table(&["a"]);
    let registered: Arc<dyn CapacityProvider> = Arc::new(StaticCapacity::new(hosts.clone()));
    let id = PlatformId::new("ours");
    let alone = DefaultPlatform::new(id.clone(), vec![registered.clone()], Some(hosts.clone()));
    assert!(alone.can_ever_serve(&wanting(2, 0)));
    assert!(!alone.can_ever_serve(&wanting(8, 0)));
    assert!(!alone.can_ever_serve(&wanting(1, 1)));
    let exact = Requirements {
        disk_bytes: Some((99 << 30) + 1),
        arch: Some("x86_64".to_owned()),
        isa_level: Some("x86-64-v2".parse::<IsaLevel>().unwrap()),
        exec: Some(ExecMode::Container),
        ..wanting(1, 0)
    };
    assert!(alone.can_ever_serve(&exact));
    let too_much_disk = Requirements {
        disk_bytes: Some((100 << 30) + 1),
        ..exact
    };
    assert!(!alone.can_ever_serve(&too_much_disk));
    let both = DefaultPlatform::new(id.clone(), vec![registered, gpu_cloud()], Some(hosts));
    assert!(both.can_ever_serve(&wanting(8, 1)));
    assert!(both.can_ever_serve(&wanting(2, 0)));
    let cloud_only = DefaultPlatform::new(id.clone(), vec![gpu_cloud()], None);
    assert!(cloud_only.can_ever_serve(&wanting(64, 1)));
    let macos = Requirements {
        os: Some("macos".to_owned()),
        ..wanting(1, 0)
    };
    assert!(!cloud_only.can_ever_serve(&macos));
    let nothing = DefaultPlatform::new(id.clone(), Vec::new(), None);
    assert!(!nothing.can_ever_serve(&wanting(1, 0)));
    assert_eq!(both.id(), id);
    let caps = both.capabilities();
    assert_eq!(caps.os, BTreeSet::from([Os::Linux]));
    assert_eq!(caps.arch, BTreeSet::from([Arch::X86_64]));
    assert_eq!(caps.accelerators, BTreeSet::from(["nvidia-a10".to_owned()]));
    assert!(caps.whole_host);
    assert_eq!(nothing.capabilities(), js_platform::PlatformCaps::default());
}

/// The view admission takes of host `a` of [`table`], in `state`, tainted and draining or
/// not.
fn view(state: HostState, tainted: bool, draining: bool) -> HostView {
    HostView {
        id: HostId::new("a").unwrap(),
        state,
        tainted,
        draining,
        total: Resources::new(4000, 8 << 30, 0, None).unwrap(),
        facts: facts(),
    }
}

/// Asserts that `platform` answers for [`wanting`] two cores what admission's own function
/// answers over `hosts` and `providers`, the views of the same host table and providers;
/// that answer.
fn agrees(platform: &DefaultPlatform, hosts: &[HostView], providers: &[ProviderView]) -> bool {
    let want = wanting(2, 0);
    let admitted = can_ever_serve(&want, hosts, providers) != AdmitVerdict::Refuse;
    assert_eq!(platform.can_ever_serve(&want), admitted);
    admitted
}

/// Asserts that the `default` platform's `can_ever_serve` is admission's function: for a
/// host that is busy, then lost (silent past its grace), for one that is tainted, for one
/// marked to drain (capable: the mark is undone when a request needs the host) and then
/// being released (draining out of the pool), and for a launching provider whose catalog
/// cannot be read, it answers what `js_sched`'s `can_ever_serve` answers over the same
/// views. Catches a platform that keeps a check of its own, which could disagree about a
/// tainted or draining host.
#[test]
fn default_platform_can_ever_serve_is_the_admission_function() {
    let id = PlatformId::new("ours");
    let on = |hosts: &Registry| DefaultPlatform::new(id.clone(), Vec::new(), Some(hosts.clone()));
    let clock = ManualClock::new(Timestamp::from_millis(1));
    let busy = table_on(&["a"], clock.clone());
    let request = Request {
        class: rack().id().clone(),
        resources: Resources::new(1000, 1 << 30, 0, None).unwrap(),
        capabilities: BTreeMap::new(),
        max_wall: Duration::from_secs(600),
        labels: BTreeMap::new(),
        validation_run: None,
        probe_host: None,
    };
    let spec = StartSpec::new(LeaseId::new(1, 1), request, &[rack()]).unwrap();
    assert!(matches!(
        busy.provider().start(spec),
        StartAck::Started { .. }
    ));
    assert!(agrees(
        &on(&busy),
        &[view(HostState::Busy, false, false)],
        &[]
    ));
    clock.advance(Duration::from_secs(3600));
    assert!(agrees(
        &on(&busy),
        &[view(HostState::Lost, false, false)],
        &[]
    ));
    let tainted = table(&[]);
    let taint = Taint {
        reason: "post-job-check".to_owned(),
        run_id: None,
        check: None,
    };
    let binding = Binding::new(Credential::of_token("t"), None).with_taint(Some(taint));
    tainted.restore_tainted(&HostId::new("a").unwrap(), binding);
    assert!(!agrees(
        &on(&tainted),
        &[view(HostState::Lost, true, false)],
        &[]
    ));
    let draining = table(&["a"]);
    let a = HostId::new("a").unwrap();
    draining.drain(&a).unwrap();
    let marked = [view(HostState::Serving, false, false)];
    assert!(agrees(&on(&draining), &marked, &[]));
    draining.release_host(&a).unwrap();
    let released = [view(HostState::Serving, false, true)];
    assert!(!agrees(&on(&draining), &released, &[]));
    let cloud = DefaultPlatform::new(id.clone(), vec![gpu_cloud()], None);
    let unread = ProviderView {
        kind_is_static: false,
        enabled: true,
        share: u32::MAX,
        capabilities: gpu_cloud().capabilities(),
        catalog: CatalogView::Unread,
        limits: PoolLimits::default(),
    };
    assert!(agrees(&cloud, &[], &[unread]));
}
