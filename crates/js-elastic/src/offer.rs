//! What one launchable thing would give, and whether it fits a job.

use js_types::{Accelerator, AcceleratorMiss, ExecMode, IsaLevel, Requirements};

/// What a job needs of a host. The scaler and the matcher read one definition.
pub type Demand = Requirements;

/// The unit a price is quoted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceUnit {
    /// US cents.
    UsCents,
}

/// How a machine from an offer is bought: the purchase option a catalog states for it.
/// A provider's configured `purchase` list orders these when offers tie on everything
/// else (design 5.6, step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Purchase {
    /// Bought by the hour at the list price, kept until released.
    OnDemand,
    /// Spare capacity at a lower price, which the cloud may take back.
    Spot,
    /// Capacity reserved for a fixed block of time.
    CapacityBlock,
    /// Capacity held by a reservation the account already has.
    Reserved,
    /// A purchase option the catalog names that none of the others is, as the catalog
    /// spells it.
    Other(String),
}

/// The dimension of a [`Demand`] that an [`Offer`] fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// The operating system differs.
    Os,
    /// The CPU architecture differs.
    Arch,
    /// The demand asks for an ISA level the offer does not state, or states one of
    /// another family or a lower one.
    IsaLevel,
    /// The execution mode differs.
    Exec,
    /// The offer has less CPU.
    CpuMillis,
    /// The offer has less memory.
    MemoryBytes,
    /// The offer has less disk.
    DiskBytes,
    /// The offer's accelerators are of another vendor, or it has none.
    AcceleratorVendor,
    /// The offer's accelerators are of another model (compared by canonical family and,
    /// for a full spelling, by its variant's facts), or it has none.
    AcceleratorModel,
    /// The offer has fewer accelerators.
    AcceleratorCount,
    /// The offer's accelerators have less memory per device, or it is not known.
    AcceleratorMemory,
    /// The offer's accelerators are linked by another kind or a smaller domain, or how is
    /// not known.
    AcceleratorInterconnect,
    /// The offer's accelerators are of another form factor, or it is not known.
    AcceleratorFormFactor,
}

impl From<AcceleratorMiss> for Dimension {
    fn from(miss: AcceleratorMiss) -> Self {
        match miss {
            AcceleratorMiss::Vendor => Self::AcceleratorVendor,
            AcceleratorMiss::Model => Self::AcceleratorModel,
            AcceleratorMiss::Count => Self::AcceleratorCount,
            AcceleratorMiss::Memory => Self::AcceleratorMemory,
            AcceleratorMiss::Interconnect => Self::AcceleratorInterconnect,
            AcceleratorMiss::FormFactor => Self::AcceleratorFormFactor,
        }
    }
}

/// What one launchable thing would give: the provider's id for it, where, its size, its
/// platform and its price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The provider's name for it; opaque to the core.
    pub id: String,
    /// Where it would run. One offer per zone with capacity.
    pub zone: Option<String>,
    /// CPU in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Disk in bytes.
    pub disk_bytes: u64,
    /// The accelerators, resolved through the alias table (`js_types::accelerator_of`).
    /// `None` when there are none.
    pub accelerator: Option<Accelerator>,
    /// The operating system (`linux`, `macos`).
    pub os: String,
    /// The CPU architecture, canonical: `x86_64` or `arm64` (every constructor of an offer
    /// reads a source's `aarch64` as `arm64`; an architecture no release knows is kept as
    /// the source spells it).
    pub arch: String,
    /// The highest ISA level a machine from this offer supports. `None`: not stated, and
    /// then the offer fits no demand that asks for a level.
    pub isa_level: Option<IsaLevel>,
    /// The mode a host from this offer runs jobs in.
    pub exec: ExecMode,
    /// The price per hour, in the smallest part of the unit. `None`: not priced.
    pub price_cents_per_hour: Option<u64>,
    /// The unit of the price.
    pub price_unit: PriceUnit,
    /// Whether the catalog has capacity for it now.
    pub available: bool,
    /// How a machine from it is bought. `None`: the catalog does not say. It is not part
    /// of the [fit](Self::fit).
    pub purchase: Option<Purchase>,
}

impl Offer {
    /// Whether a host from this offer would serve `demand`: the same platform where the
    /// demand names one, an ISA level of the same family at least as high where the demand
    /// asks for one (an offer that states no level serves no such demand), at least the
    /// CPU, memory and disk asked for (the disk both as the `disk-gib` requirement and as
    /// the scratch disk the request books), and the accelerators the demand needs. Whether
    /// the offer is [available](Self::available) is not part of the fit.
    ///
    /// The accelerator need is resolved through the alias table and compared with
    /// `AcceleratorNeed::met_by`: vendor, canonical model (a family fits every variant; a
    /// full spelling pins its variant's memory and form factor; an unknown name fits only
    /// an identical one), count at least, memory per device at least, interconnect at
    /// least, form factor. The GPUs the demand books must also be at most the offer's
    /// count, and a demand that books GPUs of a class that is not a model name fits no
    /// offer. A demand with no accelerator and no GPUs fits any offer, GPU offers included.
    ///
    /// # Errors
    ///
    /// The first [`Dimension`] it fails: platform first (the ISA level after the
    /// architecture), then size, then the accelerator in the order above.
    pub fn fit(&self, demand: &Demand) -> Result<(), Dimension> {
        let wanted = &demand.resources;
        let checks = [
            (
                Dimension::Os,
                demand.os.as_ref().is_none_or(|os| *os == self.os),
            ),
            (
                Dimension::Arch,
                demand.arch.as_ref().is_none_or(|arch| *arch == self.arch),
            ),
            (
                Dimension::IsaLevel,
                demand
                    .isa_level
                    .is_none_or(|want| self.isa_level.is_some_and(|have| have.satisfies(want))),
            ),
            (
                Dimension::Exec,
                demand.exec.is_none_or(|exec| exec == self.exec),
            ),
            (Dimension::CpuMillis, wanted.cpu_millis() <= self.cpu_millis),
            (
                Dimension::MemoryBytes,
                wanted.memory_bytes() <= self.memory_bytes,
            ),
            (
                Dimension::DiskBytes,
                [
                    demand.disk_bytes.is_none_or(|disk| disk <= self.disk_bytes),
                    wanted.disk_bytes() <= self.disk_bytes,
                ]
                .into_iter()
                .all(|fits| fits),
            ),
        ];
        match checks.into_iter().find(|(_, fits)| !fits) {
            Some((dimension, _)) => Err(dimension),
            None => self.accelerator_fit(demand),
        }
    }

    /// The accelerator part of [`Self::fit`].
    fn accelerator_fit(&self, demand: &Demand) -> Result<(), Dimension> {
        let have = self.accelerator.as_ref();
        let wanted = &demand.resources;
        match &demand.accelerator {
            Some(need) => need.met_by(have)?,
            None if wanted.gpu_class().is_some() => return Err(Dimension::AcceleratorModel),
            None => {}
        }
        match wanted.gpus() <= u64::from(have.map_or(0, |have| have.count)) {
            true => Ok(()),
            false => Err(Dimension::AcceleratorCount),
        }
    }
}

#[cfg(test)]
mod tests {
    use js_types::{AcceleratorNeed, Arch, Model, Os, Resources, accelerator_of};

    use super::*;

    /// `count` accelerators of the model `name`, as an offer states them.
    fn accelerators(name: &str, count: u32) -> Option<Accelerator> {
        Some(accelerator_of(&Model::parse(name).unwrap(), count, None))
    }

    /// A need for `count` accelerators of the model `name`, as a request that books them
    /// states it.
    fn need(name: &str, count: u32) -> Option<AcceleratorNeed> {
        Some(AcceleratorNeed {
            model: Some(Model::parse(name).unwrap()),
            count,
            ..AcceleratorNeed::default()
        })
    }

    fn offer() -> Offer {
        Offer {
            id: "gpu_1x_sample".to_owned(),
            zone: Some("zone-a".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 512 << 30,
            accelerator: accelerators("nvidia-sample", 2),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
        }
    }

    fn demand() -> Demand {
        Demand {
            resources: Resources::new(8000, 64 << 30, 2, Some("nvidia-sample".into())).unwrap(),
            disk_bytes: Some(512 << 30),
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            isa_level: None,
            exec: Some(ExecMode::Container),
            accelerator: need("nvidia-sample", 2),
            network: Default::default(),
            host_ports: Vec::new(),
        }
    }

    /// Asserts that an offer fits a demand it exactly meets, one that asks for nothing,
    /// and one that asks for less, and that availability is not part of the fit.
    #[test]
    fn an_offer_fits_what_it_meets() {
        assert_eq!(offer().fit(&demand()), Ok(()));
        let nothing = Demand {
            resources: Resources::new(0, 0, 0, None).unwrap(),
            disk_bytes: None,
            os: None,
            arch: None,
            isa_level: None,
            exec: None,
            accelerator: None,
            network: Default::default(),
            host_ports: Vec::new(),
        };
        assert_eq!(offer().fit(&nothing), Ok(()));
        let mut small = demand();
        small.resources = Resources::new(1000, 1 << 30, 1, Some("nvidia-sample".into())).unwrap();
        small.accelerator = need("nvidia-sample", 1);
        assert_eq!(offer().fit(&small), Ok(()));
        let mut gone = offer();
        gone.available = false;
        assert_eq!(gone.fit(&demand()), Ok(()));
    }

    /// Asserts that an offer whose operating system or architecture differs from the
    /// demand's is refused, naming that dimension, and that a demand that names neither
    /// fits an offer of any platform.
    #[test]
    fn an_offer_of_another_os_or_arch_is_refused() {
        let mut mac = offer();
        mac.os = "macos".to_owned();
        assert_eq!(mac.fit(&demand()), Err(Dimension::Os));
        let mut arm = offer();
        arm.arch = "arm64".to_owned();
        assert_eq!(arm.fit(&demand()), Err(Dimension::Arch));
        let mut any = demand();
        (any.os, any.arch) = (None, None);
        assert_eq!(mac.fit(&any), Ok(()));
        assert_eq!(arm.fit(&any), Ok(()));
        // Only the dimension named is checked.
        let mut os_only = demand();
        os_only.arch = None;
        assert_eq!(arm.fit(&os_only), Ok(()));
        let mut arch_only = demand();
        arch_only.os = None;
        assert_eq!(mac.fit(&arch_only), Ok(()));
    }

    /// Asserts that an offer built without a purchase option holds `None`; that each
    /// option set on an offer reads back from it and from its copy as the same option,
    /// equal to itself and to no other (an `Other` by its spelling); and that the option
    /// is not part of the fit. Catches a field that a copy drops, options that compare
    /// equal across kinds, and a fit that refuses an offer for how it is bought.
    #[test]
    fn purchase_round_trips_and_defaults_to_none() {
        assert_eq!(offer().purchase, None);
        let options = [
            Purchase::OnDemand,
            Purchase::Spot,
            Purchase::CapacityBlock,
            Purchase::Reserved,
            Purchase::Other("savings-plan".to_owned()),
            Purchase::Other("preemptible".to_owned()),
        ];
        for (index, option) in options.iter().enumerate() {
            let bought = Offer {
                purchase: Some(option.clone()),
                ..offer()
            };
            let copy = bought.clone();
            assert_eq!(copy.purchase.as_ref(), Some(option));
            assert_eq!(copy, bought);
            assert_ne!(copy, offer());
            assert_eq!(bought.fit(&demand()), Ok(()));
            for (other, different) in options.iter().enumerate() {
                assert_eq!(
                    option == different,
                    index == other,
                    "{option:?} {different:?}"
                );
            }
        }
    }

    /// Asserts that a job whose image is built for `macos`/`arm64` turns a demand that named
    /// no platform into one that a Linux x86_64 offer fails on its operating system, a Linux
    /// arm64 offer still fails on its operating system, a macOS x86_64 offer fails on its
    /// architecture, and a macOS arm64 offer meets. Catches an image platform that never
    /// reaches the fit, so a macOS job is offered a Linux host.
    #[test]
    fn the_image_platform_of_a_job_reaches_the_fit() {
        let mut any = demand();
        (any.os, any.arch) = (None, None);
        let mac_job = any.with_image_platform(Os::Macos, Arch::Arm64).unwrap();
        let host = |os: &str, arch: &str| Offer {
            os: os.to_owned(),
            arch: arch.to_owned(),
            ..offer()
        };
        assert_eq!(host("linux", "x86_64").fit(&mac_job), Err(Dimension::Os));
        assert_eq!(host("linux", "arm64").fit(&mac_job), Err(Dimension::Os));
        assert_eq!(host("macos", "x86_64").fit(&mac_job), Err(Dimension::Arch));
        assert_eq!(host("macos", "arm64").fit(&mac_job), Ok(()));
    }

    /// The level `name`, which is one.
    fn level(name: &str) -> IsaLevel {
        name.parse().unwrap()
    }

    /// The test offer stating `name` as its level (`None`: stating none).
    fn at(name: Option<&str>) -> Offer {
        Offer {
            isa_level: name.map(level),
            ..offer()
        }
    }

    /// The test demand asking for the level `name` (`None`: asking for none).
    fn asking(name: Option<&str>) -> Demand {
        Demand {
            isa_level: name.map(level),
            ..demand()
        }
    }

    /// Asserts that an ISA level is at least, within the family: an `x86-64-v3` demand
    /// fits offers stating v3 and v4 and fails one stating v2, an `armv8.2-a` demand fits
    /// `armv8.4-a` and fails `armv8.0-a`, both by the ISA level dimension; that a demand
    /// with a level fails an offer that states none, and one without a level fits an
    /// offer that states one. Catches a fit that compares levels for equality, ignores the
    /// field, or counts an offer that has not said its level.
    #[test]
    fn an_isa_level_request_matches_at_least_within_the_family() {
        let v3 = asking(Some("x86-64-v3"));
        assert_eq!(at(Some("x86-64-v3")).fit(&v3), Ok(()));
        assert_eq!(at(Some("x86-64-v4")).fit(&v3), Ok(()));
        assert_eq!(at(Some("x86-64-v2")).fit(&v3), Err(Dimension::IsaLevel));
        let mut arm = asking(Some("armv8.2-a"));
        arm.arch = Some("arm64".to_owned());
        let arm_at = |name| Offer {
            arch: "arm64".to_owned(),
            ..at(Some(name))
        };
        assert_eq!(arm_at("armv8.2-a").fit(&arm), Ok(()));
        assert_eq!(arm_at("armv8.4-a").fit(&arm), Ok(()));
        assert_eq!(arm_at("armv8.0-a").fit(&arm), Err(Dimension::IsaLevel));
        assert_eq!(at(None).fit(&v3), Err(Dimension::IsaLevel));
        assert_eq!(at(Some("x86-64-v1")).fit(&asking(None)), Ok(()));
        assert_eq!(at(None).fit(&asking(None)), Ok(()));
    }

    /// Asserts that an ISA level of one family never serves a demand for the other: an
    /// `x86-64-v3` demand against an offer stating `armv8.4-a`, and an `armv8.2-a` demand
    /// against `x86-64-v4`, both fail by the ISA level dimension (the demands name no
    /// architecture, so only the level decides). Catches levels ranked on one scale.
    #[test]
    fn an_isa_request_does_not_cross_families() {
        let mut v3 = asking(Some("x86-64-v3"));
        v3.arch = None;
        let mut a82 = asking(Some("armv8.2-a"));
        a82.arch = None;
        assert_eq!(at(Some("armv8.4-a")).fit(&v3), Err(Dimension::IsaLevel));
        assert_eq!(at(Some("x86-64-v4")).fit(&a82), Err(Dimension::IsaLevel));
    }

    /// Asserts that an offer whose architecture differs is named by the architecture
    /// before the ISA level, and a level miss before any size. Catches a dimension order
    /// that reports a level miss for a machine of the wrong architecture.
    #[test]
    fn the_arch_is_named_before_the_level_and_the_level_before_the_size() {
        let mut arm = at(Some("armv8.4-a"));
        arm.arch = "arm64".to_owned();
        assert_eq!(arm.fit(&asking(Some("x86-64-v3"))), Err(Dimension::Arch));
        let mut small = at(Some("x86-64-v2"));
        small.cpu_millis = 1;
        assert_eq!(
            small.fit(&asking(Some("x86-64-v3"))),
            Err(Dimension::IsaLevel)
        );
    }

    /// Asserts that each size and mode dimension refuses on its own, by name, and that
    /// a demand that leaves disk or mode unset ignores the offer's.
    #[test]
    fn each_dimension_refuses_by_name() {
        let mut sandbox = offer();
        sandbox.exec = ExecMode::SandboxExec;
        assert_eq!(sandbox.fit(&demand()), Err(Dimension::Exec));
        let mut either = demand();
        either.exec = None;
        assert_eq!(sandbox.fit(&either), Ok(()));

        let ask = |cpu, memory, gpus: u64, class: Option<&str>| {
            let mut wanted = demand();
            wanted.resources = Resources::new(cpu, memory, gpus, class.map(str::to_owned)).unwrap();
            wanted.accelerator = class.and_then(|class| need(class, gpus as u32));
            wanted
        };
        let class = Some("nvidia-sample");
        assert_eq!(
            offer().fit(&ask(8001, 0, 0, None)),
            Err(Dimension::CpuMillis)
        );
        assert_eq!(
            offer().fit(&ask(0, (64 << 30) + 1, 0, None)),
            Err(Dimension::MemoryBytes)
        );
        assert_eq!(
            offer().fit(&ask(0, 0, 3, class)),
            Err(Dimension::AcceleratorCount)
        );
        assert_eq!(
            offer().fit(&ask(0, 0, 1, Some("nvidia-other"))),
            Err(Dimension::AcceleratorModel)
        );

        let mut disk = demand();
        disk.disk_bytes = Some((512 << 30) + 1);
        assert_eq!(offer().fit(&disk), Err(Dimension::DiskBytes));
        disk.disk_bytes = None;
        assert_eq!(offer().fit(&disk), Ok(()));
    }

    /// Asserts that the scratch disk a request books (the disk of its resources, not the
    /// `disk-gib` requirement) is judged against the offer's disk: the exact edge fits,
    /// one byte more is refused by the disk dimension, and a request that books none fits
    /// an offer with none. It catches a fit that reads only the `disk-gib` requirement,
    /// which counts an offer for a request the scheduler refuses for disk.
    #[test]
    fn the_scratch_disk_a_request_books_is_judged_against_the_offer() {
        let books = |bytes: u64| {
            let mut wanted = demand();
            wanted.disk_bytes = None;
            wanted.resources = Resources::new(1000, 1 << 30, 0, None)
                .unwrap()
                .with_disk_bytes(bytes);
            wanted
        };
        assert_eq!(offer().fit(&books(512 << 30)), Ok(()));
        assert_eq!(
            offer().fit(&books((512 << 30) + 1)),
            Err(Dimension::DiskBytes)
        );
        let mut none = offer();
        none.disk_bytes = 0;
        assert_eq!(none.fit(&books(0)), Ok(()));
        assert_eq!(none.fit(&books(1)), Err(Dimension::DiskBytes));
    }

    /// Asserts that an offer with no accelerator fails a demand for GPUs by model, and
    /// that when several dimensions fail the platform is named before the size.
    #[test]
    fn a_classless_offer_fails_a_gpu_demand_and_the_platform_is_named_first() {
        let mut cpu_only = offer();
        cpu_only.accelerator = None;
        assert_eq!(cpu_only.fit(&demand()), Err(Dimension::AcceleratorModel));
        let mut worse = offer();
        (worse.os, worse.cpu_millis) = ("macos".to_owned(), 1);
        assert_eq!(worse.fit(&demand()), Err(Dimension::Os));
    }
}
