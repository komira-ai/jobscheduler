use std::collections::BTreeSet;
use std::fmt;

use crate::MemoryPercent;

/// A resource vector was malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypeError {
    /// A dimension name is not one of the known dimensions.
    #[error("unknown resource dimension {0:?}")]
    UnknownDimension(String),
    /// GPUs were counted without a GPU class, or a GPU class named for zero GPUs.
    #[error("a GPU class must be named exactly when gpus is above zero")]
    GpuClassMismatch,
}

/// One named dimension of a resource vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dim {
    /// Thousandths of a CPU core.
    CpuMillis,
    /// Bytes of memory.
    MemoryBytes,
    /// Whole GPU devices. Fractions are out of scope.
    Gpus,
    /// Bytes of scratch disk.
    DiskBytes,
}

impl Dim {
    /// The dimension called `name`, or an error naming it.
    pub fn from_name(name: &str) -> Result<Self, TypeError> {
        match name {
            "cpu_millis" => Ok(Self::CpuMillis),
            "memory_bytes" => Ok(Self::MemoryBytes),
            "gpus" => Ok(Self::Gpus),
            "disk_bytes" => Ok(Self::DiskBytes),
            other => Err(TypeError::UnknownDimension(other.to_string())),
        }
    }

    /// The dimension's name as written in configuration.
    pub fn name(&self) -> &'static str {
        match self {
            Self::CpuMillis => "cpu_millis",
            Self::MemoryBytes => "memory_bytes",
            Self::Gpus => "gpus",
            Self::DiskBytes => "disk_bytes",
        }
    }
}

impl fmt::Display for Dim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of dimensions, as the gate set of a `Book` admission.
pub type Dims = BTreeSet<Dim>;

/// A resource vector: named non-negative integers plus, when `gpus` is above zero,
/// the class of those GPUs (for example `nvidia-a100`).
///
/// The GPU class is set exactly when `gpus > 0`, so a vector never counts GPUs of no
/// kind. Dimensions are closed: [`Resources::from_named`] refuses any name it does not
/// know, and new dimensions arrive as new variants of [`Dim`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    cpu_millis: u64,
    memory_bytes: u64,
    gpus: u64,
    gpu_class: Option<String>,
    disk_bytes: u64,
}

impl Resources {
    /// A vector with the given counts, refusing a GPU class that does not match `gpus`.
    pub fn new(
        cpu_millis: u64,
        memory_bytes: u64,
        gpus: u64,
        gpu_class: Option<String>,
    ) -> Result<Self, TypeError> {
        if (gpus > 0) != gpu_class.is_some() {
            return Err(TypeError::GpuClassMismatch);
        }
        Ok(Self {
            cpu_millis,
            memory_bytes,
            gpus,
            gpu_class,
            disk_bytes: 0,
        })
    }

    /// This vector with `disk_bytes` of scratch disk. [`Resources::new`] keeps its
    /// signature (other repositories pin this crate by revision), so disk is a builder.
    pub fn with_disk_bytes(mut self, disk_bytes: u64) -> Self {
        self.disk_bytes = disk_bytes;
        self
    }

    /// A vector from `(dimension name, count)` pairs; a dimension left out is zero.
    /// An unknown name is refused by name.
    pub fn from_named<'a>(
        dims: impl IntoIterator<Item = (&'a str, u64)>,
        gpu_class: Option<String>,
    ) -> Result<Self, TypeError> {
        let mut counts = [0u64; 4];
        for (name, count) in dims {
            let slot = match Dim::from_name(name)? {
                Dim::CpuMillis => 0,
                Dim::MemoryBytes => 1,
                Dim::Gpus => 2,
                Dim::DiskBytes => 3,
            };
            counts[slot] = count;
        }
        Ok(Self::new(counts[0], counts[1], counts[2], gpu_class)?.with_disk_bytes(counts[3]))
    }

    /// Thousandths of a CPU core.
    pub fn cpu_millis(&self) -> u64 {
        self.cpu_millis
    }

    /// Bytes of memory.
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// Whole GPU devices.
    pub fn gpus(&self) -> u64 {
        self.gpus
    }

    /// Bytes of scratch disk; zero when the vector books none.
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes
    }

    /// The class of the GPUs, set exactly when [`Resources::gpus`] is above zero.
    pub fn gpu_class(&self) -> Option<&str> {
        self.gpu_class.as_deref()
    }

    /// The same vector with its memory cut to `percent` of itself, rounded down.
    #[must_use]
    pub fn with_memory_percent(&self, percent: MemoryPercent) -> Resources {
        Resources {
            memory_bytes: percent.of(self.memory_bytes),
            ..self.clone()
        }
    }

    /// Whether `offered` has at least this much of every dimension, and the same GPU
    /// class when this vector counts GPUs.
    pub fn fits_in(&self, offered: &Resources) -> bool {
        let class_matches = match &self.gpu_class {
            None => true,
            Some(class) => offered.gpu_class() == Some(class.as_str()),
        };
        [
            self.cpu_millis <= offered.cpu_millis,
            self.memory_bytes <= offered.memory_bytes,
            self.gpus <= offered.gpus,
            self.disk_bytes <= offered.disk_bytes,
            class_matches,
        ]
        .into_iter()
        .all(|ok| ok)
    }

    /// The sum of two vectors, or `None` when a count overflows or the two name
    /// different GPU classes.
    pub fn checked_add(&self, other: &Resources) -> Option<Resources> {
        let gpu_class = match (&self.gpu_class, &other.gpu_class) {
            (Some(a), Some(b)) if a != b => return None,
            (Some(class), _) | (_, Some(class)) => Some(class.clone()),
            (None, None) => None,
        };
        Some(Resources {
            cpu_millis: self.cpu_millis.checked_add(other.cpu_millis)?,
            memory_bytes: self.memory_bytes.checked_add(other.memory_bytes)?,
            gpus: self.gpus.checked_add(other.gpus)?,
            gpu_class,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(cpu: u64, mem: u64, gpus: u64, class: Option<&str>) -> Resources {
        Resources::new(cpu, mem, gpus, class.map(str::to_string)).unwrap()
    }

    /// Asserts that every known dimension name round-trips and an unknown name is
    /// refused with the name in the error.
    #[test]
    fn dimension_names_round_trip_and_unknown_is_refused() {
        for dim in [Dim::CpuMillis, Dim::MemoryBytes, Dim::Gpus, Dim::DiskBytes] {
            assert_eq!(Dim::from_name(dim.name()), Ok(dim));
            assert_eq!(dim.to_string(), dim.name());
        }
        let err = Dim::from_name("vms").unwrap_err();
        assert_eq!(err, TypeError::UnknownDimension("vms".to_string()));
        assert!(err.to_string().contains("\"vms\""));
    }

    /// Asserts that a GPU class is required exactly when gpus is above zero.
    #[test]
    fn gpu_class_must_match_gpu_count() {
        assert!(Resources::new(1, 1, 1, Some("a100".into())).is_ok());
        assert!(Resources::new(1, 1, 0, None).is_ok());
        assert_eq!(
            Resources::new(1, 1, 1, None),
            Err(TypeError::GpuClassMismatch)
        );
        assert_eq!(
            Resources::new(1, 1, 0, Some("a100".into())),
            Err(TypeError::GpuClassMismatch)
        );
    }

    /// Asserts that `from_named` fills named dimensions, leaves others at zero, and
    /// refuses an unknown name.
    #[test]
    fn from_named_reads_names() {
        let r = Resources::from_named(
            [("cpu_millis", 4000), ("memory_bytes", 8), ("gpus", 2)],
            Some("a100".into()),
        )
        .unwrap();
        assert_eq!((r.cpu_millis(), r.memory_bytes(), r.gpus()), (4000, 8, 2));
        assert_eq!(r.gpu_class(), Some("a100"));
        let cpu_only = Resources::from_named([("cpu_millis", 5)], None).unwrap();
        assert_eq!((cpu_only.memory_bytes(), cpu_only.gpus()), (0, 0));
        assert_eq!(
            Resources::from_named([("vms", 1)], None),
            Err(TypeError::UnknownDimension("vms".into()))
        );
        assert_eq!(
            Resources::from_named([("gpus", 1)], None),
            Err(TypeError::GpuClassMismatch),
            "GPUs without a class"
        );
    }

    /// Asserts that `from_named` reads `disk_bytes`, that `new` leaves disk at zero and
    /// that the builder sets it without touching the other dimensions.
    #[test]
    fn disk_bytes_is_built_by_builder_and_from_named() {
        assert_eq!(res(1, 2, 0, None).disk_bytes(), 0);
        let built = res(1, 2, 1, Some("a100")).with_disk_bytes(9);
        assert_eq!(
            (built.cpu_millis(), built.memory_bytes(), built.gpus()),
            (1, 2, 1)
        );
        assert_eq!(built.disk_bytes(), 9);
        let named = Resources::from_named([("disk_bytes", 7), ("cpu_millis", 3)], None).unwrap();
        assert_eq!((named.cpu_millis(), named.disk_bytes()), (3, 7));
    }

    /// Asserts that fit compares every dimension and the GPU class when GPUs are
    /// wanted.
    #[test]
    fn fits_in_checks_each_dimension_and_class() {
        let host = res(8000, 100, 4, Some("a100"));
        assert!(res(8000, 100, 4, Some("a100")).fits_in(&host));
        assert!(res(0, 0, 0, None).fits_in(&host));
        assert!(!res(8001, 100, 4, Some("a100")).fits_in(&host));
        assert!(!res(8000, 101, 4, Some("a100")).fits_in(&host));
        assert!(!res(8000, 100, 5, Some("a100")).fits_in(&host));
        assert!(!res(8000, 100, 1, Some("h100")).fits_in(&host));
        assert!(!res(0, 0, 1, Some("a100")).fits_in(&res(1, 1, 0, None)));
    }

    /// Asserts that fit refuses a request for more disk than offered, and that disk
    /// alone decides it when every other dimension fits.
    #[test]
    fn fits_in_checks_disk() {
        let host = res(8, 8, 0, None).with_disk_bytes(100);
        assert!(res(8, 8, 0, None).with_disk_bytes(100).fits_in(&host));
        assert!(!res(8, 8, 0, None).with_disk_bytes(101).fits_in(&host));
        assert!(res(8, 8, 0, None).fits_in(&res(8, 8, 0, None)));
        assert!(
            !res(0, 0, 0, None)
                .with_disk_bytes(1)
                .fits_in(&res(8, 8, 0, None))
        );
    }

    /// Asserts that addition sums disk and refuses a disk overflow.
    #[test]
    fn checked_add_sums_disk_and_refuses_overflow() {
        let a = res(1, 1, 0, None).with_disk_bytes(5);
        let b = res(1, 1, 0, None).with_disk_bytes(6);
        assert_eq!(a.checked_add(&b).unwrap().disk_bytes(), 11);
        let max = res(0, 0, 0, None).with_disk_bytes(u64::MAX);
        assert_eq!(
            max.checked_add(&res(0, 0, 0, None).with_disk_bytes(1)),
            None
        );
    }

    /// Asserts that addition sums counts, merges the GPU class from either side, and
    /// refuses a class conflict or an overflow in each dimension.
    #[test]
    fn checked_add_sums_and_refuses_conflicts() {
        let a = res(1, 2, 1, Some("a100"));
        let b = res(10, 20, 2, Some("a100"));
        let sum = a.checked_add(&b).unwrap();
        assert_eq!(
            (sum.cpu_millis(), sum.memory_bytes(), sum.gpus()),
            (11, 22, 3)
        );
        let plain = res(5, 5, 0, None);
        assert_eq!(a.checked_add(&plain).unwrap().gpu_class(), Some("a100"));
        assert_eq!(plain.checked_add(&a).unwrap().gpu_class(), Some("a100"));
        assert_eq!(plain.checked_add(&plain).unwrap().gpu_class(), None);
        assert_eq!(a.checked_add(&res(1, 1, 1, Some("h100"))), None);
        let max = u64::MAX;
        assert_eq!(res(max, 0, 0, None).checked_add(&res(1, 0, 0, None)), None);
        assert_eq!(res(0, max, 0, None).checked_add(&res(0, 1, 0, None)), None);
        let g = res(0, 0, max, Some("a100"));
        assert_eq!(g.checked_add(&res(0, 0, 1, Some("a100"))), None);
    }
}
