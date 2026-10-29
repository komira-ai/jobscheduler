//! What one launchable thing would give, and whether it fits a job.

use std::fmt;

use js_types::{ExecMode, Requirements};

/// What a job needs of a host. The scaler and the matcher read one definition.
pub type Demand = Requirements;

/// A GPU class spelled as the agent reports it: the model name in lower case, its words
/// joined by `-`, so `NVIDIA A100-SXM4-80GB` is `nvidia-a100-sxm4-80gb`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GpuClass(String);

impl GpuClass {
    /// The class `text`, which must already be in the agent's spelling.
    ///
    /// # Errors
    ///
    /// The reason `text` is not a class.
    pub fn parse(text: &str) -> Result<Self, String> {
        let words: Vec<&str> = text.split('-').collect();
        let word = |w: &&str| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        };
        match (text.is_empty(), words.iter().all(word)) {
            (true, _) => Err("is empty".to_owned()),
            (false, false) => Err(
                "must be lower case letters and digits joined by single '-', as the agent reports a GPU model"
                    .to_owned(),
            ),
            (false, true) => Ok(Self(text.to_owned())),
        }
    }

    /// The class as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GpuClass {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.0)
    }
}

/// The unit a price is quoted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceUnit {
    /// US cents.
    UsCents,
}

/// The dimension of a [`Demand`] that an [`Offer`] fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// The operating system differs.
    Os,
    /// The CPU architecture differs.
    Arch,
    /// The execution mode differs.
    Exec,
    /// The offer has less CPU.
    CpuMillis,
    /// The offer has less memory.
    MemoryBytes,
    /// The offer has less disk.
    DiskBytes,
    /// The offer has fewer GPUs.
    Gpus,
    /// The offer has no GPUs of the class asked for.
    GpuClass,
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
    /// GPUs.
    pub gpus: u32,
    /// The class of the GPUs, as the agent reports it. `None` when there are none.
    pub gpu_class: Option<GpuClass>,
    /// The operating system (`linux`, `macos`).
    pub os: String,
    /// The CPU architecture (`x86_64`, `aarch64`).
    pub arch: String,
    /// The mode a host from this offer runs jobs in.
    pub exec: ExecMode,
    /// The price per hour, in the smallest part of the unit. `None`: not priced.
    pub price_cents_per_hour: Option<u64>,
    /// The unit of the price.
    pub price_unit: PriceUnit,
    /// Whether the catalog has capacity for it now.
    pub available: bool,
}

impl Offer {
    /// Whether a host from this offer would serve `demand`: the same platform where the
    /// demand names one, and at least the CPU, memory, disk and GPUs asked for, with
    /// the GPU class when GPUs are asked for. Whether the offer is [available](Self::available)
    /// is not part of the fit.
    ///
    /// # Errors
    ///
    /// The first [`Dimension`] it fails: platform first, then size.
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
                demand.disk_bytes.is_none_or(|disk| disk <= self.disk_bytes),
            ),
            (Dimension::Gpus, wanted.gpus() <= u64::from(self.gpus)),
            (
                Dimension::GpuClass,
                wanted.gpu_class().is_none_or(|class| {
                    self.gpu_class
                        .as_ref()
                        .is_some_and(|own| own.as_str() == class)
                }),
            ),
        ];
        match checks.into_iter().find(|(_, fits)| !fits) {
            Some((dimension, _)) => Err(dimension),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use js_types::Resources;

    use super::*;

    fn offer() -> Offer {
        Offer {
            id: "gpu_1x_sample".to_owned(),
            zone: Some("zone-a".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 64 << 30,
            disk_bytes: 512 << 30,
            gpus: 2,
            gpu_class: Some(GpuClass::parse("nvidia-sample").unwrap()),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
        }
    }

    fn demand() -> Demand {
        Demand {
            resources: Resources::new(8000, 64 << 30, 2, Some("nvidia-sample".into())).unwrap(),
            disk_bytes: Some(512 << 30),
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            exec: Some(ExecMode::Container),
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
            exec: None,
        };
        assert_eq!(offer().fit(&nothing), Ok(()));
        let mut small = demand();
        small.resources = Resources::new(1000, 1 << 30, 1, Some("nvidia-sample".into())).unwrap();
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
        arm.arch = "aarch64".to_owned();
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

        let ask = |cpu, memory, gpus, class: Option<&str>| {
            let mut wanted = demand();
            wanted.resources = Resources::new(cpu, memory, gpus, class.map(str::to_owned)).unwrap();
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
        assert_eq!(offer().fit(&ask(0, 0, 3, class)), Err(Dimension::Gpus));
        assert_eq!(
            offer().fit(&ask(0, 0, 1, Some("nvidia-other"))),
            Err(Dimension::GpuClass)
        );

        let mut disk = demand();
        disk.disk_bytes = Some((512 << 30) + 1);
        assert_eq!(offer().fit(&disk), Err(Dimension::DiskBytes));
        disk.disk_bytes = None;
        assert_eq!(offer().fit(&disk), Ok(()));
    }

    /// Asserts that an offer with no GPU class fails a demand for GPUs by class, and
    /// that when several dimensions fail the platform is named before the size.
    #[test]
    fn a_classless_offer_fails_a_gpu_demand_and_the_platform_is_named_first() {
        let mut cpu_only = offer();
        (cpu_only.gpus, cpu_only.gpu_class) = (2, None);
        assert_eq!(cpu_only.fit(&demand()), Err(Dimension::GpuClass));
        let mut worse = offer();
        (worse.os, worse.cpu_millis) = ("macos".to_owned(), 1);
        assert_eq!(worse.fit(&demand()), Err(Dimension::Os));
    }

    /// Asserts that a GPU class is accepted only in the agent's spelling.
    #[test]
    fn a_gpu_class_must_be_in_the_agents_spelling() {
        for good in ["nvidia-a10", "x", "nvidia-h100-80gb-hbm3", "a1-2b"] {
            let class = GpuClass::parse(good).unwrap();
            assert_eq!((class.as_str(), class.to_string().as_str()), (good, good));
        }
        for bad in [
            "",
            "NVIDIA-A10",
            "nvidia a10",
            "nvidia--a10",
            "-nvidia",
            "nvidia-",
            "nvidia_a10",
            "nvidia-a10 ",
            "nvidia-\u{e9}",
        ] {
            assert!(GpuClass::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(GpuClass::parse("").unwrap_err(), "is empty");
        assert!(
            GpuClass::parse("NVIDIA A10")
                .unwrap_err()
                .starts_with("must be lower case")
        );
    }
}
