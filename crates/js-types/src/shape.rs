use std::collections::BTreeMap;

use crate::Resources;

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

    /// The capability facts the host reports.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Whether the host offers at least `wanted` and reports every key in `required`
    /// with the same value.
    pub fn satisfies(&self, wanted: &Resources, required: &Capabilities) -> bool {
        let caps_ok = required
            .iter()
            .all(|(key, value)| self.capabilities.get(key) == Some(value));
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
}
