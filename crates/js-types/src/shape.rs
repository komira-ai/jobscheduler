use std::collections::BTreeMap;

use crate::job::{ARCH_KEY, ISA_LEVEL_KEY};
use crate::{
    ACCEL_KEYS, Accelerator, AcceleratorNeed, DISK_GIB_KEY, IsaLevel, MemoryPercent, Model,
    Resources, accelerator_of, normalise_arch,
};

/// Typed key/value facts a host reports, or a request needs (design section 3).
pub type Capabilities = BTreeMap<String, String>;

/// What one host offers: its resources and its capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    resources: Resources,
    capabilities: Capabilities,
}

impl Shape {
    /// A shape offering these resources and capabilities.
    pub fn new(resources: Resources, capabilities: Capabilities) -> Self {
        Self {
            resources,
            capabilities,
        }
    }

    /// The resources the host offers.
    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// The same shape with its memory cut to `percent` of itself, rounded down.
    #[must_use]
    pub fn with_memory_percent(&self, percent: MemoryPercent) -> Shape {
        Shape {
            resources: self.resources.with_memory_percent(percent),
            capabilities: self.capabilities.clone(),
        }
    }

    /// The capability facts the host reports.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Whether the host offers at least `wanted` and reports every key in `required`
    /// with the same value. Three keys have their own comparison:
    ///
    /// - `disk-gib` is a minimum: the host's free GiB must be at least the amount
    ///   required, and a host that reports no number (or a required amount that is not
    ///   one) does not satisfy it.
    /// - `arch` is equal in the one spelling ([`normalise_arch`]), so a host that reports
    ///   `aarch64` serves `arch=arm64` and the reverse.
    /// - `isa_level` is at least, within the family ([`IsaLevel::satisfies`]), when both
    ///   values state a level ([`IsaLevel::highest_listed`]: one level, or the list of it
    ///   and those below, as a pool's shape may name it); otherwise the two texts must be
    ///   equal, as before levels were compared, so every pairing that satisfied before
    ///   still does. A host that states no level satisfies no level.
    /// - The `accelerator.*` keys are one need ([`AcceleratorNeed`]), resolved through the
    ///   alias table and compared with [`AcceleratorNeed::met_by`] against the host's
    ///   accelerator ([`Self::accelerator`]), over the need the booked GPUs state when they
    ///   are of a class that is a model name ([`AcceleratorNeed::requested`]): the class is
    ///   the model, so a full spelling fits only its variant and a family every variant,
    ///   and a key the request names narrows that need without dropping it. Either way the request is held to the
    ///   host's GPU count, not to its GPU class. A request that books GPUs of a class that
    ///   is not a model name is held to the class exactly, as before.
    pub fn satisfies(&self, wanted: &Resources, required: &Capabilities) -> bool {
        let resources_ok = match AcceleratorNeed::requested(required, wanted) {
            Ok(None) => wanted.fits_in(&self.resources),
            Ok(Some(need)) => {
                need.met_by(self.accelerator().as_ref()).is_ok()
                    && counts_fit(wanted, &self.resources)
            }
            Err(_) => false,
        };
        let caps_ok = required
            .iter()
            .filter(|(key, _)| !ACCEL_KEYS.contains(&key.as_str()))
            .all(|(key, value)| {
                let have = self.capabilities.get(key);
                match key.as_str() {
                    DISK_GIB_KEY => have
                        .and_then(|have| have.parse::<u64>().ok())
                        .zip(value.parse::<u64>().ok())
                        .is_some_and(|(have, want)| want <= have),
                    ARCH_KEY => {
                        have.is_some_and(|have| normalise_arch(have) == normalise_arch(value))
                    }
                    ISA_LEVEL_KEY => {
                        let listed = have.map(String::as_str).and_then(IsaLevel::highest_listed);
                        match listed.zip(IsaLevel::highest_listed(value)) {
                            Some((have, want)) => have.satisfies(want),
                            None => have == Some(value),
                        }
                    }
                    _ => have == Some(value),
                }
            });
        [resources_ok, caps_ok].into_iter().all(|ok| ok)
    }

    /// The host's accelerators: what its `accelerator.*` capabilities state when it reports
    /// them, else its GPU count of its GPU class resolved through the alias table
    /// ([`accelerator_of`]; no memory is measured). `None` when it has no GPU, or a class
    /// that is not a model name, or reports keys that are not valid.
    #[must_use]
    pub fn accelerator(&self) -> Option<Accelerator> {
        match Accelerator::from_capabilities(&self.capabilities) {
            Ok(Some(reported)) => return Some(reported),
            Ok(None) => {}
            Err(_) => return None,
        }
        let model = Model::parse(self.resources.gpu_class()?).ok()?;
        let count = u32::try_from(self.resources.gpus()).unwrap_or(u32::MAX);
        Some(accelerator_of(&model, count, None))
    }
}

/// Whether `offered` has at least the CPU, memory, GPUs and disk of `wanted`, whatever the
/// GPU class.
fn counts_fit(wanted: &Resources, offered: &Resources) -> bool {
    [
        wanted.cpu_millis() <= offered.cpu_millis(),
        wanted.memory_bytes() <= offered.memory_bytes(),
        wanted.gpus() <= offered.gpus(),
        wanted.disk_bytes() <= offered.disk_bytes(),
    ]
    .into_iter()
    .all(|ok| ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(pairs: &[(&str, &str)]) -> Capabilities {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Asserts that a shape satisfies a request only when resources fit and every
    /// required capability is present with an equal value.
    #[test]
    fn satisfies_needs_resources_and_capabilities() {
        let shape = Shape::new(
            Resources::new(8000, 100, 1, Some("a100".into())).unwrap(),
            caps(&[("isa", "x86-64-v3"), ("driver", "550")]),
        );
        let small = Resources::new(1000, 1, 0, None).unwrap();
        assert!(shape.satisfies(&small, &caps(&[])));
        assert!(shape.satisfies(&small, &caps(&[("isa", "x86-64-v3")])));
        assert!(!shape.satisfies(&small, &caps(&[("isa", "arm64")])));
        assert!(!shape.satisfies(&small, &caps(&[("rdma", "yes")])));
        let big = Resources::new(9000, 1, 0, None).unwrap();
        assert!(!shape.satisfies(&big, &caps(&[])));
        assert_eq!(shape.resources().gpus(), 1);
        assert_eq!(shape.capabilities().len(), 2);
    }

    /// Asserts that `disk-gib` is a minimum, with the exact edge accepted, that a host
    /// reporting no disk or a non-number, or a requirement that is not a number, never
    /// satisfies it, and that `exec` stays an equality.
    #[test]
    fn disk_is_a_minimum_and_exec_is_equal() {
        let resources = Resources::new(8000, 100, 0, None).unwrap();
        let shape = |pairs: &[(&str, &str)]| Shape::new(resources.clone(), caps(pairs));
        let host = shape(&[("disk-gib", "100"), ("exec", "container")]);
        let none = Resources::new(0, 0, 0, None).unwrap();
        assert!(host.satisfies(&none, &caps(&[("disk-gib", "99")])));
        assert!(host.satisfies(&none, &caps(&[("disk-gib", "100")])));
        assert!(!host.satisfies(&none, &caps(&[("disk-gib", "101")])));
        assert!(host.satisfies(&none, &caps(&[("exec", "container")])));
        assert!(!host.satisfies(&none, &caps(&[("exec", "sandbox-exec")])));
        assert!(!shape(&[]).satisfies(&none, &caps(&[("disk-gib", "1")])));
        assert!(!shape(&[("disk-gib", "lots")]).satisfies(&none, &caps(&[("disk-gib", "1")])));
        assert!(!host.satisfies(&none, &caps(&[("disk-gib", "lots")])));
    }

    /// Asserts that `arch` is compared in the one spelling, so a host that reports
    /// `aarch64` (an agent built before `arm64` was canonical) serves `arch=arm64` and a
    /// host that reports `arm64` serves `arch=aarch64`, that another architecture still
    /// does not, and that a host that reports none serves no `arch`. Catches the plain
    /// text equality that keeps an old agent's host from every `arm64` request.
    #[test]
    fn arch_is_equal_in_the_one_spelling() {
        let none = Resources::new(0, 0, 0, None).unwrap();
        let host = |arch: &str| {
            Shape::new(
                Resources::new(8000, 100, 0, None).unwrap(),
                caps(&[("arch", arch)]),
            )
        };
        let want = |arch: &str| caps(&[("arch", arch)]);
        assert!(host("aarch64").satisfies(&none, &want("arm64")));
        assert!(host("arm64").satisfies(&none, &want("aarch64")));
        assert!(host("arm64").satisfies(&none, &want("arm64")));
        assert!(!host("aarch64").satisfies(&none, &want("x86_64")));
        assert!(!host("x86_64").satisfies(&none, &want("arm64")));
        assert!(host("sparc").satisfies(&none, &want("sparc")));
        let bare = Shape::new(Resources::new(8000, 100, 0, None).unwrap(), caps(&[]));
        assert!(!bare.satisfies(&none, &want("arm64")));
    }

    /// Asserts that `isa_level` is at least, within the family: a host at v4, or listing
    /// v1 to v3, serves a v3 request, a host at v2 does not, an Arm host serves no x86-64
    /// level, and a host that states no level, or a level no release knows, serves no
    /// `isa_level` request. Catches the plain text equality that turns a v4 host away
    /// from a v3 job.
    #[test]
    fn isa_level_is_at_least_within_the_family() {
        let none = Resources::new(0, 0, 0, None).unwrap();
        let host = |level: &str| {
            Shape::new(
                Resources::new(8000, 100, 0, None).unwrap(),
                caps(&[("isa_level", level)]),
            )
        };
        let v3 = caps(&[("isa_level", "x86-64-v3")]);
        assert!(host("x86-64-v3").satisfies(&none, &v3));
        assert!(host("x86-64-v4").satisfies(&none, &v3));
        assert!(host("x86-64-v1,x86-64-v2,x86-64-v3").satisfies(&none, &v3));
        assert!(!host("x86-64-v2").satisfies(&none, &v3));
        assert!(!host("armv8.4-a").satisfies(&none, &v3));
        assert!(!host("fast").satisfies(&none, &v3));
        let a82 = caps(&[("isa_level", "armv8.2-a")]);
        assert!(host("armv8.4-a").satisfies(&none, &a82));
        assert!(!host("armv8.0-a").satisfies(&none, &a82));
        assert!(!host("x86-64-v4").satisfies(&none, &a82));
        let bare = Shape::new(Resources::new(8000, 100, 0, None).unwrap(), caps(&[]));
        assert!(!bare.satisfies(&none, &v3));
        assert!(!host("x86-64-v4").satisfies(&none, &caps(&[("isa_level", "fast")])));
    }

    /// Asserts that every `isa_level` pairing that satisfied before levels were compared
    /// still does: a pool floor that names the list `x86-64-v1,x86-64-v2` is met by a host
    /// reporting that same list, or a higher level, and not by one reporting only v1; and
    /// two equal texts that are not levels still satisfy each other, while unequal ones do
    /// not. Catches a floor written as a list that no host can meet any more, which would
    /// refuse every hello of a pool whose configuration was accepted before.
    #[test]
    fn a_floor_that_lists_levels_is_met_as_before() {
        let none = Resources::new(0, 0, 0, None).unwrap();
        let host = |level: &str| {
            Shape::new(
                Resources::new(8000, 100, 0, None).unwrap(),
                caps(&[("isa_level", level)]),
            )
        };
        let floor = caps(&[("isa_level", "x86-64-v1,x86-64-v2")]);
        assert!(host("x86-64-v1,x86-64-v2").satisfies(&none, &floor));
        assert!(host("x86-64-v3").satisfies(&none, &floor));
        assert!(!host("x86-64-v1").satisfies(&none, &floor));
        assert!(!host("armv8.4-a").satisfies(&none, &floor));
        let fast = caps(&[("isa_level", "fast")]);
        assert!(host("fast").satisfies(&none, &fast));
        assert!(!host("slow").satisfies(&none, &fast));
        let mixed = "x86-64-v3,armv8.4-a";
        assert!(host(mixed).satisfies(&none, &caps(&[("isa_level", mixed)])));
    }

    /// Asserts that cutting a shape's memory changes only the memory: cpu, gpus, the gpu
    /// class and the capabilities are kept, and the cut rounds down.
    #[test]
    fn a_memory_percent_cuts_only_the_memory() {
        let shape = Shape::new(
            Resources::new(8000, 1001, 1, Some("a100".into())).unwrap(),
            caps(&[("isa", "x86-64-v3")]),
        );
        let cut = shape.with_memory_percent(MemoryPercent::new(50).unwrap());
        assert_eq!(cut.resources().memory_bytes(), 500);
        assert_eq!(cut.resources().cpu_millis(), 8000);
        assert_eq!(cut.resources().gpus(), 1);
        assert_eq!(cut.resources().gpu_class(), Some("a100"));
        assert_eq!(cut.capabilities(), shape.capabilities());
        assert_eq!(shape.with_memory_percent(MemoryPercent::FULL), shape);
    }

    /// A host of `gpus` GPUs of `class` with ample CPU, memory and disk.
    fn gpu_host(gpus: u64, class: &str, reported: &[(&str, &str)]) -> Shape {
        let resources = Resources::new(64_000, 1 << 40, gpus, Some(class.into()))
            .unwrap()
            .with_disk_bytes(1 << 40);
        Shape::new(resources, caps(reported))
    }

    /// Asserts the capability-key path of `satisfies`: a family need fits every variant of
    /// the family a host's class spells, a full spelling only its own variant, an unknown
    /// name only an identical class, the count is at least, the booked class is not
    /// compared on this path (a family request books its family name), a request without
    /// keys is held to the need its booked class states, and one whose class is not a
    /// model name to that class exactly. Catches a family request that never places on a
    /// host that reports a full spelling, or a full spelling widened to its family.
    #[test]
    fn accelerator_keys_resolve_the_hosts_class_through_the_table() {
        let a100_80 = gpu_host(8, "nvidia-a100-sxm4-80gb", &[]);
        let a100_40 = gpu_host(8, "nvidia-a100-sxm4-40gb", &[]);
        let gb300 = gpu_host(8, "nvidia-gb300", &[]);
        let family = Resources::new(1000, 1, 4, Some("nvidia-a100".into())).unwrap();
        let need = |pairs: &[(&str, &str)]| caps(pairs);
        let family_need = need(&[
            ("accelerator.model", "nvidia-a100"),
            ("accelerator.count", "4"),
        ]);
        assert!(a100_80.satisfies(&family, &family_need));
        assert!(a100_40.satisfies(&family, &family_need));
        assert!(!gb300.satisfies(&family, &family_need));
        let full = Resources::new(1000, 1, 4, Some("nvidia-a100-sxm4-80gb".into())).unwrap();
        let full_need = need(&[
            ("accelerator.model", "nvidia-a100-sxm4-80gb"),
            ("accelerator.count", "4"),
        ]);
        assert!(a100_80.satisfies(&full, &full_need));
        assert!(!a100_40.satisfies(&full, &full_need));
        let unknown = Resources::new(1000, 1, 4, Some("nvidia-gb300".into())).unwrap();
        let unknown_need = need(&[
            ("accelerator.model", "nvidia-gb300"),
            ("accelerator.count", "4"),
        ]);
        assert!(gb300.satisfies(&unknown, &unknown_need));
        assert!(!a100_80.satisfies(&unknown, &unknown_need));
        let nine = Resources::new(1000, 1, 9, Some("nvidia-a100".into())).unwrap();
        let nine_need = need(&[
            ("accelerator.model", "nvidia-a100"),
            ("accelerator.count", "9"),
        ]);
        assert!(!a100_80.satisfies(&nine, &nine_need));
        let eight = Resources::new(1000, 1, 8, Some("nvidia-a100".into())).unwrap();
        let eight_need = need(&[
            ("accelerator.model", "nvidia-a100"),
            ("accelerator.count", "8"),
        ]);
        assert!(
            a100_80.satisfies(&eight, &eight_need),
            "exactly the host's count"
        );
        assert!(
            a100_80.satisfies(&family, &caps(&[])),
            "without keys the booked class is the need: a family fits every variant"
        );
        assert!(a100_80.satisfies(&full, &caps(&[])));
        assert!(
            !a100_40.satisfies(&full, &caps(&[])),
            "a booked full spelling fits only its variant"
        );
        let odd = Resources::new(1000, 1, 1, Some("A100_X".into())).unwrap();
        assert!(gpu_host(8, "A100_X", &[]).satisfies(&odd, &caps(&[])));
        assert!(
            !gpu_host(8, "A100_Y", &[]).satisfies(&odd, &caps(&[])),
            "a class that is not a model name is compared exactly"
        );
        assert!(!a100_80.satisfies(&family, &need(&[("accelerator.model", "A100")])));
        let cpu = Resources::new(1000, 1, 0, None).unwrap();
        assert!(a100_80.satisfies(&cpu, &need(&[("accelerator.vendor", "nvidia")])));
        assert!(!a100_80.satisfies(&cpu, &need(&[("accelerator.vendor", "amd")])));
        let too_big = Resources::new(64_001, 1, 4, Some("nvidia-a100".into())).unwrap();
        assert!(
            !a100_80.satisfies(&too_big, &family_need),
            "CPU is still compared"
        );
    }

    /// Asserts where a host's accelerator comes from: the `accelerator.*` capabilities it
    /// reports when it reports them, else its class and count through the table (the
    /// variant's nominal memory), and none for a host without GPUs, with a class that is
    /// not a model name, or with keys that are not valid. Catches a host whose reported
    /// accelerator is ignored for its class, or a GPU host that reads as having none.
    #[test]
    fn a_hosts_accelerator_is_reported_or_derived_from_its_class() {
        let derived = gpu_host(8, "nvidia-h100-80gb-hbm3", &[])
            .accelerator()
            .unwrap();
        assert_eq!(derived.model.as_str(), "nvidia-h100");
        assert_eq!(derived.vendor.as_str(), "nvidia");
        assert_eq!(
            (derived.count, derived.memory_per_device),
            (8, Some(80 << 30))
        );
        let reported = gpu_host(
            8,
            "nvidia-h100-80gb-hbm3",
            &[
                ("accelerator.model", "nvidia-gb300"),
                ("accelerator.count", "4"),
                ("accelerator.memory-gib", "279"),
            ],
        )
        .accelerator()
        .unwrap();
        assert_eq!(reported.model.as_str(), "nvidia-gb300");
        assert_eq!(
            (reported.count, reported.memory_per_device),
            (4, Some(279 << 30))
        );
        let plain = Shape::new(Resources::new(1, 1, 0, None).unwrap(), caps(&[]));
        assert_eq!(plain.accelerator(), None);
        assert_eq!(gpu_host(1, "A100_X", &[]).accelerator(), None);
        let bad = gpu_host(1, "nvidia-a100", &[("accelerator.count", "many")]);
        assert_eq!(bad.accelerator(), None);
        assert!(!bad.satisfies(
            &Resources::new(1, 1, 1, Some("nvidia-a100".into())).unwrap(),
            &caps(&[("accelerator.count", "x")])
        ));
    }
}
