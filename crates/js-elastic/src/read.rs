//! The read half of a provider: a handle that can look and cannot buy.
//!
//! [`ElasticProvider`] holds both halves, the reads and the calls that make or end a
//! machine. A pool in plan mode must launch nothing, and "nothing calls `acquire`" is a
//! property of the code that holds the provider. [`ReadHandle`] makes it a property of
//! the type: it offers the catalog, the checks, the host preparation, the payload limit,
//! the list, the unresolved acquire ids and the read by id, and has no `acquire` and no
//! `release`. Code that is given a `ReadHandle` cannot spend, and cannot be changed to
//! spend without changing its signature.
//!
//! The field is private and no method returns the provider, so a `ReadHandle` cannot be
//! turned back into a spend handle.

use std::sync::Arc;

use crate::answer::Held;
use crate::boot::BootStep;
use crate::ids::{AcquireId, HostRef};
use crate::offer::Offer;
use crate::provider::{CheckResult, ElasticProvider, ProviderError};

/// A provider seen through its reads only. Cheap to clone.
#[derive(Clone)]
pub struct ReadHandle(Arc<dyn ElasticProvider>);

impl ReadHandle {
    /// The read half of `provider`.
    pub fn new(provider: Arc<dyn ElasticProvider>) -> Self {
        Self(provider)
    }

    /// [`ElasticProvider::catalog`].
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the catalog cannot be had whole.
    pub fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        self.0.catalog()
    }

    /// [`ElasticProvider::checks`].
    pub fn checks(&self) -> Vec<CheckResult> {
        self.0.checks()
    }

    /// [`ElasticProvider::host_prep`].
    pub fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.0.host_prep(offer)
    }

    /// [`ElasticProvider::payload_limit`].
    pub fn payload_limit(&self) -> usize {
        self.0.payload_limit()
    }

    /// [`ElasticProvider::list`].
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole.
    pub fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.0.list()
    }

    /// [`ElasticProvider::unresolved`].
    pub fn unresolved(&self) -> Vec<AcquireId> {
        self.0.unresolved()
    }

    /// [`ElasticProvider::is_gone`].
    pub fn is_gone(&self, id: &HostRef) -> bool {
        self.0.is_gone(id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::answer::{Acquired, Released};
    use crate::provider::AcquireRequest;

    /// A provider that writes down the name of every call made on it.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<&'static str>>);

    impl Recorder {
        fn note(&self, call: &'static str) {
            self.0.lock().unwrap().push(call);
        }
    }

    impl ElasticProvider for Recorder {
        fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
            self.note("catalog");
            Ok(Vec::new())
        }

        fn checks(&self) -> Vec<CheckResult> {
            self.note("checks");
            Vec::new()
        }

        fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
            self.note("host_prep");
            Vec::new()
        }

        fn payload_limit(&self) -> usize {
            self.note("payload_limit");
            7
        }

        fn acquire(&self, _request: &AcquireRequest) -> Acquired {
            self.note("acquire");
            Acquired::Unknown
        }

        fn list(&self) -> Result<Vec<Held>, ProviderError> {
            self.note("list");
            Ok(Vec::new())
        }

        fn release(&self, _id: &HostRef) -> Released {
            self.note("release");
            Released::Unknown
        }

        fn unresolved(&self) -> Vec<AcquireId> {
            self.note("unresolved");
            Vec::new()
        }

        fn is_gone(&self, _id: &HostRef) -> bool {
            self.note("is_gone");
            true
        }
    }

    /// Asserts that every method of the read handle reaches the provider's method of the
    /// same name, with the provider's answer, and that the calls made through a handle
    /// are exactly the seven reads: no `acquire` and no `release` is among them, and the
    /// handle has no method that could make one. The same provider, called directly, does
    /// log an `acquire` and a `release`, so the absence above is a measurement. It catches a
    /// read that is wired to the wrong call, and a handle that grew a purchase.
    #[test]
    fn a_read_handle_makes_the_seven_reads_and_nothing_else() {
        let provider = Arc::new(Recorder::default());
        let handle = ReadHandle::new(provider.clone());
        let copy = handle.clone();
        let offer = Offer {
            id: "o".to_owned(),
            zone: None,
            cpu_millis: 1,
            memory_bytes: 1,
            disk_bytes: 1,
            gpus: 0,
            gpu_class: None,
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: js_types::ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: crate::offer::PriceUnit::UsCents,
            available: true,
        };
        assert_eq!(handle.catalog(), Ok(Vec::new()));
        assert!(handle.checks().is_empty());
        assert!(handle.host_prep(&offer).is_empty());
        assert_eq!(copy.payload_limit(), 7);
        assert_eq!(handle.list(), Ok(Vec::new()));
        assert!(handle.unresolved().is_empty());
        assert!(handle.is_gone(&HostRef("i".to_owned())));
        let reads = [
            "catalog",
            "checks",
            "host_prep",
            "payload_limit",
            "list",
            "unresolved",
            "is_gone",
        ];
        assert_eq!(*provider.0.lock().unwrap(), reads);

        let scheduler = crate::ids::SchedulerId::new("s").unwrap();
        let acquire = AcquireId::new("a").unwrap();
        let request = AcquireRequest {
            name: crate::ids::HostName::derive(&scheduler, &acquire),
            acquire,
            pool: "p".to_owned(),
            offer,
            labels: Vec::new(),
            payload: crate::secret::BootPayload::new("x".to_owned()),
        };
        assert_eq!(provider.acquire(&request), Acquired::Unknown);
        assert_eq!(
            provider.release(&HostRef("i".to_owned())),
            Released::Unknown
        );
        let log = provider.0.lock().unwrap().clone();
        assert_eq!(log[reads.len()..], ["acquire", "release"]);
    }
}
