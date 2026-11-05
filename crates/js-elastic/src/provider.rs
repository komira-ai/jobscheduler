//! The contract of a provider that launches machines.

use crate::answer::{Acquired, Held, Observed, Released};
use crate::boot::BootStep;
use crate::caps::{Capacity, ProviderCaps};
use js_types::ProviderId;

use crate::ids::{AcquireId, Hello, HostName, HostRef};
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

/// What one named check of a provider found. A check is a read: it never makes anything
/// on the provider's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The name of the check, such as `ssh_key`.
    pub name: &'static str,
    /// Why it failed, in words written to be shown; `None` when it held. The words hold
    /// no secret.
    pub failure: Option<String>,
    /// Something worth showing about a check that held, such as what a launch will make.
    pub note: Option<String>,
}

impl CheckResult {
    /// A check that held.
    pub fn held(name: &'static str) -> Self {
        Self {
            name,
            failure: None,
            note: None,
        }
    }

    /// A check that held, with a note to show.
    pub fn held_noting(name: &'static str, note: impl Into<String>) -> Self {
        Self {
            note: Some(note.into()),
            ..Self::held(name)
        }
    }

    /// A check that failed for `reason`.
    pub fn failed(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            failure: Some(reason.into()),
            ..Self::held(name)
        }
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
    /// The longest the cloud itself may keep the machine, if the pool has a bound: the
    /// scheduler's `max_age` plus its ceiling on a lease's wall time plus a margin. A
    /// provider whose cloud can enforce a lifetime (a run-duration limit, a shutdown timer)
    /// sets it at launch, so billing stops even if the scheduler is dead; one whose cloud
    /// has none (Lambda: `shutdown -h` leaves billing on) ignores it. `None` for a pool
    /// with no bound. Nothing in this crate reads it.
    pub lifetime: Option<std::time::Duration>,
    /// More marks for the machine: key and value.
    pub labels: Vec<(String, String)>,
    /// What the machine boots with, delivered as it is.
    pub payload: BootPayload,
}

/// A notice a provider sends of its own accord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// The provider reclaimed the machine (a preemption): it no longer runs and the work on
    /// it is lost. It still needs a [`CapacityProvider::release`].
    Reclaimed(HostRef),
}

/// The notices of one provider, in the order it sent them. `next` waits for the next one
/// and answers `None` once the provider will send no more.
pub type EventStream = Box<dyn Iterator<Item = ProviderEvent> + Send>;

/// A scheduler's view of a provider that supplies machines which run the agent.
///
/// The calls block, and each is bound by the time limit of the client under it. The
/// trait must not be called with a lock held that other work waits on (the lease
/// table's is one): an acquire takes a request or two, and a slow provider makes every
/// call slow. [`CapacityProvider::id`], [`CapacityProvider::can_launch`],
/// [`CapacityProvider::capabilities`], [`CapacityProvider::capacity`] and
/// [`CapacityProvider::verify_instance`] answer from the provider's own code and records
/// and make no call. It is not the lease-level `Pool` of `js-provider`, the allocation
/// port that books hosts that already exist.
///
/// What each call means is pinned by the cases of the `conformance` module (behind the
/// crate's `conformance` feature), which every provider crate runs against its provider.
pub trait CapacityProvider: Send + Sync {
    /// The provider's stable name, such as its kind. The scheduler tells providers apart
    /// by it and never branches on it.
    fn id(&self) -> ProviderId;

    /// Whether [`CapacityProvider::acquire`] can launch a machine at all. A provider of
    /// machines that register themselves has nothing to launch.
    fn can_launch(&self) -> bool;

    /// What the provider could ever offer, so that a job none of its machines could serve
    /// is refused up front. Every offer of [`CapacityProvider::catalog`] is inside it.
    fn capabilities(&self) -> ProviderCaps;

    /// What the provider could launch now, one offer per zone with capacity. A call that
    /// works is also a check that the credential does.
    ///
    /// # Errors
    ///
    /// [`ProviderError`] when the catalog cannot be had whole.
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError>;

    /// How many machines the provider holds for this scheduler and how many more it could
    /// add, from what it last listed, launched or released. It never calls the cloud and
    /// never waits for a call in flight.
    fn capacity(&self) -> Capacity;

    /// The provider's own live checks, beyond the catalog call: what only its account can
    /// say, such as a missing key or a ruleset that holds rules. Called on each check pass
    /// of a pool whose catalog call worked, and must only read: it never creates or
    /// changes anything on the provider's side. A provider with none keeps the default.
    fn checks(&self) -> Vec<CheckResult> {
        Vec::new()
    }

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
    ///
    /// A reference that is not an id of this provider is not one of its machines: the
    /// answer is [`Released::Refused`] with [`crate::Claim::Foreign`], a definite answer that is
    /// never [`Released::Gone`], and nothing is sent to the provider.
    fn release(&self, id: &HostRef) -> Released;

    /// The acquire ids with a call that got no answer and may have landed: while an id is
    /// here, [`CapacityProvider::acquire`] with it does not launch again but answers
    /// [`Acquired::Deferred`], and a retry settles the call once a list shows the machine
    /// or the provider's hold has run out. The scheduler keeps a launch it planned for such
    /// an id instead of forgetting it. A provider that holds no such call has none.
    fn unresolved(&self) -> Vec<AcquireId> {
        Vec::new()
    }

    /// Reads the machine `id` by id. [`Observed::Absent`] only when the provider said it
    /// does not exist; a failed, throttled or timed-out read, and a reference the provider
    /// cannot read, are [`Observed::Unknown`]; a machine in any state, terminated included,
    /// is [`Observed::Present`].
    fn observe(&self, id: &HostRef) -> Observed;

    /// Whether a read of the machine `id` by id says the provider no longer has it:
    /// [`CapacityProvider::observe`] answers [`Observed::Absent`]. A failed read and a
    /// machine in any state (terminated included) are `false`.
    fn is_gone(&self, id: &HostRef) -> bool {
        self.observe(id) == Observed::Absent
    }

    /// The machine this provider launched that `hello`, an agent's first hello, comes
    /// from, or `None` when it launched none by that name (and, when the hello carries
    /// one, that instance id). A name that carries another scheduler's prefix is never
    /// one of its machines. Answers from the provider's own records and makes no call.
    fn verify_instance(&self, hello: &Hello) -> Option<HostRef>;

    /// The provider's notices, such as a reclaimed machine, for a provider that sends any;
    /// `None` (the default) for one that does not, whose reclaims a list shows instead.
    fn events(&self) -> Option<EventStream> {
        None
    }
}

#[cfg(test)]
mod tests {
    use js_types::ExecMode;

    use super::*;
    use crate::answer::HostStatus;
    use crate::ids::SchedulerId;
    use crate::offer::PriceUnit;

    fn offer() -> Offer {
        Offer {
            id: "type".to_owned(),
            zone: None,
            cpu_millis: 1,
            memory_bytes: 1,
            disk_bytes: 1,
            accelerator: Some(js_types::accelerator_of(
                &js_types::Model::parse("x").unwrap(),
                1,
                None,
            )),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: None,
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
        }
    }

    fn request() -> AcquireRequest {
        let scheduler = SchedulerId::new("s").unwrap();
        let acquire = AcquireId::new("a").unwrap();
        AcquireRequest {
            name: HostName::derive(&scheduler, &acquire),
            acquire,
            pool: "gpu".to_owned(),
            lifetime: None,
            offer: offer(),
            labels: Vec::new(),
            payload: BootPayload::new("join-token-MARKER".to_owned()),
        }
    }

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

        let request = request();
        let shown = format!("{request:?}");
        assert!(!shown.contains("MARKER"), "{shown}");
        assert!(shown.contains("***"), "{shown}");
        assert!(shown.contains("gpu"));
    }

    /// A provider that implements only the required calls.
    struct Bare;

    impl CapacityProvider for Bare {
        fn id(&self) -> ProviderId {
            ProviderId::known("bare")
        }

        fn can_launch(&self) -> bool {
            false
        }

        fn capabilities(&self) -> ProviderCaps {
            ProviderCaps::default()
        }

        fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
            Ok(Vec::new())
        }

        fn capacity(&self) -> Capacity {
            Capacity {
                supplied: 0,
                can_add: None,
            }
        }

        fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
            Vec::new()
        }

        fn payload_limit(&self) -> usize {
            0
        }

        fn acquire(&self, _request: &AcquireRequest) -> Acquired {
            Acquired::Unknown
        }

        fn list(&self) -> Result<Vec<Held>, ProviderError> {
            Ok(Vec::new())
        }

        fn release(&self, _id: &HostRef) -> Released {
            Released::Unknown
        }

        fn observe(&self, id: &HostRef) -> Observed {
            match id.as_str() {
                "i-gone" => Observed::Absent,
                "i-off" => Observed::Present(HostStatus::Terminated),
                _ => Observed::Unknown,
            }
        }

        fn verify_instance(&self, _hello: &Hello) -> Option<HostRef> {
            None
        }
    }

    /// Asserts the defaults of the optional calls: a provider that does not say it holds
    /// an unresolved call has none, one that sends no notices has no event stream, and a
    /// machine is gone only when its read by id answers absent: a read with no answer and
    /// a terminated machine are not gone. It catches a default that reads "no answer" or
    /// "terminated" as "gone", which would have the scheduler settle a machine that may
    /// exist.
    #[test]
    fn a_provider_that_says_nothing_has_no_unresolved_call_and_finds_nothing_gone() {
        let id = HostRef("i-1".to_owned());
        assert!(Bare.unresolved().is_empty());
        assert!(Bare.events().is_none());
        assert!(!Bare.is_gone(&id));
        assert!(!Bare.is_gone(&HostRef("i-off".to_owned())));
        assert!(Bare.is_gone(&HostRef("i-gone".to_owned())));
        // The required calls of the stand-in, so that it is whole.
        assert_eq!(Bare.id().as_str(), "bare");
        assert!(!Bare.can_launch());
        assert_eq!(Bare.capabilities(), ProviderCaps::default());
        assert_eq!(Bare.capacity().can_add, None);
        assert_eq!(Bare.catalog(), Ok(Vec::new()));
        assert!(Bare.host_prep(&offer()).is_empty());
        assert_eq!(Bare.payload_limit(), 0);
        assert_eq!(Bare.list(), Ok(Vec::new()));
        assert_eq!(Bare.release(&id), Released::Unknown);
        assert_eq!(Bare.acquire(&request()), Acquired::Unknown);
        let hello = Hello {
            name: "js-0-0".to_owned(),
            instance_id: None,
        };
        assert_eq!(Bare.verify_instance(&hello), None);
    }

    /// Asserts that a notice names the machine it is about and compares by it.
    #[test]
    fn a_reclaim_notice_names_its_machine() {
        let notice = ProviderEvent::Reclaimed(HostRef("i-1".to_owned()));
        assert_eq!(notice, ProviderEvent::Reclaimed(HostRef("i-1".to_owned())));
        assert_ne!(notice, ProviderEvent::Reclaimed(HostRef("i-2".to_owned())));
    }
}
