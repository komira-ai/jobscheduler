//! The read half of a provider: a handle that can look and cannot buy.
//!
//! [`CapacityProvider`] holds both halves, the reads and the calls that make or end a
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
use crate::provider::{CapacityProvider, CatalogFuture, CheckResult, ProviderError};

/// A provider seen through its reads only. Cheap to clone.
#[derive(Clone)]
pub struct ReadHandle(Arc<dyn CapacityProvider>);

impl ReadHandle {
    /// The read half of `provider`.
    pub fn new(provider: Arc<dyn CapacityProvider>) -> Self {
        Self(provider)
    }

    /// [`CapacityProvider::catalog`]: a future whose output is [`ProviderError`] when
    /// the catalog cannot be had whole.
    pub fn catalog(&self) -> CatalogFuture<'_> {
        self.0.catalog()
    }

    /// [`CapacityProvider::checks`].
    pub fn checks(&self) -> Vec<CheckResult> {
        self.0.checks()
    }

    /// [`CapacityProvider::host_prep`].
    pub fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.0.host_prep(offer)
    }

    /// [`CapacityProvider::payload_limit`].
    pub fn payload_limit(&self) -> usize {
        self.0.payload_limit()
    }

    /// [`CapacityProvider::list`].
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the list cannot be had whole.
    pub fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.0.list()
    }

    /// [`CapacityProvider::unresolved`].
    pub fn unresolved(&self) -> Vec<AcquireId> {
        self.0.unresolved()
    }

    /// [`CapacityProvider::is_gone`].
    pub fn is_gone(&self, id: &HostRef) -> bool {
        self.0.is_gone(id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::answer::{Acquired, Observed, Released};
    use crate::caps::{Capacity, ProviderCaps};
    use js_types::ProviderId;

    use crate::ids::Hello;
    use crate::provider::AcquireRequest;
    use crate::wait::block_on;

    /// A provider that writes down the name of every call made on it.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<&'static str>>);

    impl Recorder {
        fn note(&self, call: &'static str) {
            self.0.lock().unwrap().push(call);
        }
    }

    impl CapacityProvider for Recorder {
        fn id(&self) -> ProviderId {
            self.note("id");
            ProviderId::known("recorder")
        }

        fn can_launch(&self) -> bool {
            self.note("can_launch");
            true
        }

        fn capabilities(&self) -> ProviderCaps {
            self.note("capabilities");
            ProviderCaps::default()
        }

        fn catalog(&self) -> CatalogFuture<'_> {
            self.note("catalog");
            Box::pin(std::future::ready(Ok(Vec::new())))
        }

        fn capacity(&self) -> Capacity {
            self.note("capacity");
            Capacity {
                supplied: 0,
                can_add: None,
            }
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

        fn observe(&self, _id: &HostRef) -> Observed {
            self.note("observe");
            Observed::Absent
        }

        fn is_gone(&self, _id: &HostRef) -> bool {
            self.note("is_gone");
            true
        }

        fn verify_instance(&self, _hello: &Hello) -> Option<HostRef> {
            self.note("verify_instance");
            None
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
            accelerator: None,
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: js_types::ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: crate::offer::PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
        };
        assert_eq!(block_on(handle.catalog()), Ok(Vec::new()));
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
            lifetime: None,
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

        // The calls the handle does not offer, made on the provider itself, are logged too.
        assert_eq!(provider.id().as_str(), "recorder");
        assert!(provider.can_launch());
        assert_eq!(provider.capabilities(), ProviderCaps::default());
        assert_eq!(provider.capacity().supplied, 0);
        assert_eq!(provider.observe(&HostRef("i".to_owned())), Observed::Absent);
        let hello = Hello {
            name: "n".to_owned(),
            instance_id: None,
            identity: None,
        };
        assert_eq!(provider.verify_instance(&hello), None);
        let log = provider.0.lock().unwrap().clone();
        let direct = [
            "id",
            "can_launch",
            "capabilities",
            "capacity",
            "observe",
            "verify_instance",
        ];
        assert_eq!(log[reads.len() + 2..], direct);
    }
}
