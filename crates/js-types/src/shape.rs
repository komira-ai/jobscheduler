use std::collections::BTreeMap;

use crate::{DISK_GIB_KEY, MemoryPercent, Resources};

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
    /// with the same value. The one key compared as a minimum is `disk-gib`: the host's
    /// free GiB must be at least the amount required, and a host that reports no number
    /// (or a required amount that is not one) does not satisfy it.
    pub fn satisfies(&self, wanted: &Resources, required: &Capabilities) -> bool {
        let caps_ok = required.iter().all(|(key, value)| {
            let have = self.capabilities.get(key);
            match key.as_str() {
                DISK_GIB_KEY => have
                    .and_then(|have| have.parse::<u64>().ok())
                    .zip(value.parse::<u64>().ok())
                    .is_some_and(|(have, want)| want <= have),
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
