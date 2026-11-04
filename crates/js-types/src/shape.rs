use std::collections::BTreeMap;

use crate::job::{ARCH_KEY, ISA_LEVEL_KEY};
use crate::{DISK_GIB_KEY, IsaLevel, MemoryPercent, Resources, normalise_arch};

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
    /// - `isa_level` is at least, within the family ([`IsaLevel::satisfies`]): the host's
    ///   value states its highest level ([`IsaLevel::highest_listed`]), and a host that
    ///   states none (or a required text that is not a level) does not satisfy it.
    pub fn satisfies(&self, wanted: &Resources, required: &Capabilities) -> bool {
        let caps_ok = required.iter().all(|(key, value)| {
            let have = self.capabilities.get(key);
            match key.as_str() {
                DISK_GIB_KEY => have
                    .and_then(|have| have.parse::<u64>().ok())
                    .zip(value.parse::<u64>().ok())
                    .is_some_and(|(have, want)| want <= have),
                ARCH_KEY => have.is_some_and(|have| normalise_arch(have) == normalise_arch(value)),
                ISA_LEVEL_KEY => have
                    .map(String::as_str)
                    .and_then(IsaLevel::highest_listed)
                    .zip(value.parse::<IsaLevel>().ok())
                    .is_some_and(|(have, want)| have.satisfies(want)),
                _ => have == Some(value),
            }
        });
        [wanted.fits_in(&self.resources), caps_ok]
            .into_iter()
            .all(|ok| ok)
    }
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
}
