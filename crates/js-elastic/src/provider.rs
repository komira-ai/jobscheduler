//! The contract of a provider that launches machines.

use crate::answer::{Acquired, Held, Released};
use crate::boot::BootStep;
use crate::ids::{AcquireId, HostName, HostRef};
use crate::offer::Offer;
use crate::secret::BootPayload;

/// A call to a provider failed, so what it would have said is not known. An error is
/// never an empty answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProviderError(String);

impl ProviderError {
    /// An error that reads as `message`: the provider's own words, with nothing secret
    /// in them.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// One acquire: launch one machine of `offer`.
#[derive(Debug)]
pub struct AcquireRequest {
    /// The id of the acquire. A second request with the same id launches nothing when
    /// the first one's machine exists.
    pub acquire: AcquireId,
    /// The name of the pool the machine is for.
    pub pool: String,
    /// The name to give the machine, chosen before the launch. A provider that marks
    /// what it launches by name refuses one that does not carry its marker.
    pub name: HostName,
    /// What to launch.
    pub offer: Offer,
    /// More marks for the machine: key and value.
    pub labels: Vec<(String, String)>,
    /// What the machine boots with, delivered as it is.
    pub payload: BootPayload,
}

/// A scheduler's view of a provider that launches machines.
///
/// The calls block, and each is bound by the time limit of the client under it. The
/// trait must not be called with a lock held that other work waits on (the lease
/// table's is one): an acquire takes a request or two, and a slow provider makes every
/// call slow. It is not the lease-level `Provider` of `js-provider`, which books hosts
/// that already exist.
pub trait ElasticProvider: Send + Sync {
    /// What the provider could launch now, one offer per zone with capacity. A call that
    /// works is also a check that the credential does.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the catalog cannot be had whole.
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError>;

    /// What a machine of `offer` lacks that its image does not bring, as boot steps. Data
    /// that depends on the offer alone.
    fn host_prep(&self, offer: &Offer) -> Vec<BootStep>;

    /// The most bytes of payload the provider carries. A payload over it is refused.
    fn payload_limit(&self) -> usize;

    /// Launches one machine for `request`, or finds the one an earlier request with the
    /// same acquire id launched. Delivers `request.payload` as it is.
    fn acquire(&self, request: &AcquireRequest) -> Acquired;

    /// The machines this scheduler launched, in the provider's order, terminated ones
    /// included if the provider lists them.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole. An error is never an empty
    /// list.
    fn list(&self) -> Result<Vec<Held>, ProviderError>;

    /// Terminates one machine, if it is ours, and reads it back.
    fn release(&self, id: &HostRef) -> Released;
}

#[cfg(test)]
mod tests {
    use js_types::ExecMode;

    use super::*;
    use crate::ids::SchedulerId;
    use crate::offer::{GpuClass, PriceUnit};

    /// Asserts that an error reads as the provider's words and compares by them, and
    /// that a request prints its payload as `***` and not as text.
    #[test]
    fn an_error_reads_as_its_message_and_a_request_hides_its_payload() {
        let error = ProviderError::new("the list is not complete");
        assert_eq!(error.to_string(), "the list is not complete");
        assert_eq!(
            error,
            ProviderError::new(String::from("the list is not complete"))
        );

        let scheduler = SchedulerId::new("s").unwrap();
        let acquire = AcquireId::new("a").unwrap();
        let request = AcquireRequest {
            name: HostName::derive(&scheduler, &acquire),
            acquire,
            pool: "gpu".to_owned(),
            offer: Offer {
                id: "type".to_owned(),
                zone: None,
                cpu_millis: 1,
                memory_bytes: 1,
                disk_bytes: 1,
                gpus: 1,
                gpu_class: Some(GpuClass::parse("x").unwrap()),
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                exec: ExecMode::Container,
                price_cents_per_hour: None,
                price_unit: PriceUnit::UsCents,
                available: true,
            },
            labels: Vec::new(),
            payload: BootPayload::new("join-token-MARKER".to_owned()),
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("MARKER"), "{shown}");
        assert!(shown.contains("***"), "{shown}");
        assert!(shown.contains("gpu"));
    }
}
