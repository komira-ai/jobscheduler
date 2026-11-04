//! The `default` container platform over its machine providers and the host table of its
//! `static` one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_agent_wire::{Hello, Phase, ProtocolRange, WireShape};
use js_elastic::{
    AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, GpuClass, Held, HostRef,
    Observed, Offer, ProviderCaps, ProviderError, Released,
};
use js_lease::{ManualClock, Timestamp};
use js_lease_server::DefaultPlatform;
use js_platform::{ContainerPlatform, PlatformId};
use js_registry::{Credential, PoolConfig, Registry, StaticCapacity};
use js_types::{
    Admission, Arch, ClassId, ExecMode, IsaLevel, Os, ProviderId, Requirements, ResourceClass,
    Resources, Shape,
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

/// A host table of a pool of 4 cores and 8 GiB per host, with `hosts` registered.
fn table(hosts: &[&str]) -> Registry {
    let class = ResourceClass::new(ClassId::new("rack").unwrap(), Admission::BookAll, 0);
    let shape = Shape::new(Resources::new(4000, 8 << 30, 0, None).unwrap(), facts());
    let config = PoolConfig::new(ProviderId::new("rack").unwrap(), class, shape, 4);
    let registry = Registry::new(config, ManualClock::new(Timestamp::from_millis(1)));
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
        Err(ProviderError::new("the catalog is not read for admission"))
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
        accelerators: BTreeSet::from([GpuClass::parse("nvidia-a10").unwrap()]),
        whole_host: true,
        host_ports: true,
    }))
}

fn wanting(cores: u64, gpus: u64) -> Requirements {
    let class = (gpus > 0).then(|| "nvidia-a10".to_owned());
    Requirements {
        resources: Resources::new(cores * 1000, 1 << 30, gpus, class).unwrap(),
        disk_bytes: None,
        os: Some("linux".to_owned()),
        arch: None,
        isa_level: None,
        exec: None,
    }
}

/// Asserts that the `default` platform could ever serve a job when a registered host could
/// hold it, whether or not a launching provider could offer one; that it could not when
/// no host could and no launching provider says it could (the `static` provider's pooled
/// capabilities do not count); that a launching provider's capabilities alone are enough;
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
