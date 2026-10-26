use crate::{ClassId, Dims, Resources};

/// How a kind of lease is admitted (design section 6.2). The design's `Whole` mode is
/// not here: `BookAll` already gives a lease the whole host in this slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Gate on these dimensions and count every dimension in the booked total.
    Book(Dims),
    /// Book every dimension of the host, so the lease holds the whole host with no
    /// overcommit.
    BookAll,
}

/// A request did not meet its class's minimum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassError {
    /// The request asks for fewer GPUs than the class needs.
    #[error("class {class} needs at least {min} gpus, request has {have}")]
    TooFewGpus {
        /// The class that was asked for.
        class: ClassId,
        /// The class's minimum.
        min: u64,
        /// What the request had.
        have: u64,
    },
}

/// A resource class as data: how it is admitted and what a request of it must hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceClass {
    id: ClassId,
    admission: Admission,
    min_gpus: u64,
}

impl ResourceClass {
    /// A class with this id, admission mode and GPU minimum.
    pub fn new(id: ClassId, admission: Admission, min_gpus: u64) -> Self {
        Self {
            id,
            admission,
            min_gpus,
        }
    }

    /// The shipped class for GPU testing: a whole host (`BookAll`) with at least one
    /// GPU of a class the request names.
    pub fn gpu_host() -> Self {
        let id = ClassId::from_trusted("gpu-host");
        Self::new(id, Admission::BookAll, 1)
    }

    /// The class id.
    pub fn id(&self) -> &ClassId {
        &self.id
    }

    /// The admission mode.
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    /// The fewest GPUs a request of this class may ask for.
    pub fn min_gpus(&self) -> u64 {
        self.min_gpus
    }

    /// Whether `wanted` meets the class minimum.
    pub fn check(&self, wanted: &Resources) -> Result<(), ClassError> {
        if wanted.gpus() < self.min_gpus {
            return Err(ClassError::TooFewGpus {
                class: self.id.clone(),
                min: self.min_gpus,
                have: wanted.gpus(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that the shipped GPU class books a whole host and needs one GPU, and
    /// that a request with none is refused with the numbers in the error.
    #[test]
    fn gpu_host_needs_a_gpu() {
        let class = ResourceClass::gpu_host();
        assert_eq!(class.id().as_str(), "gpu-host");
        assert_eq!(class.admission(), &Admission::BookAll);
        assert_eq!(class.min_gpus(), 1);
        let one = Resources::new(1, 1, 1, Some("a100".into())).unwrap();
        assert_eq!(class.check(&one), Ok(()));
        let none = Resources::new(1, 1, 0, None).unwrap();
        let err = class.check(&none).unwrap_err();
        assert_eq!(
            err.to_string(),
            "class gpu-host needs at least 1 gpus, request has 0"
        );
    }
}
