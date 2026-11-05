//! The conformance suite against providers that break its rules: a case must fail on a
//! provider that breaks the rule it pins, so that a green run on a real provider means
//! something. The `expected` text is the start of the case's assertion message, so each
//! test also pins why the case fails.

use std::sync::Mutex;

use js_elastic::conformance::{
    Rig, acquire_is_idempotent_on_the_acquire_id, refused_codes_are_the_plain_constants,
    release_terminates_then_reads_back,
};
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootPayload, BootStep, Capacity, CapacityProvider, Held,
    Hello, HostName, HostRef, HostStatus, Observed, Offer, PriceUnit, ProviderCaps, ProviderError,
    RefusalCode, Released, SchedulerId,
};
use js_types::{ExecMode, ProviderId};

/// How the stand-in breaks the contract.
#[derive(Clone, Copy)]
enum Flaw {
    /// Every acquire launches a new machine, a retry with the same acquire id included.
    LaunchesOnEveryAcquire,
    /// No acquire is ever answered.
    NeverAnswers,
    /// A release answers what a terminate would say, without reading the machine back.
    AnswersFromTerminate,
}

/// A provider with one flaw, which counts its launches.
struct Broken {
    flaw: Flaw,
    launched: Mutex<usize>,
}

impl CapacityProvider for Broken {
    fn id(&self) -> ProviderId {
        ProviderId::known("broken")
    }

    fn can_launch(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ProviderCaps {
        ProviderCaps::default()
    }

    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        Ok(vec![offer()])
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

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        if let Flaw::NeverAnswers = self.flaw {
            return Acquired::Unknown;
        }
        let mut launched = self.launched.lock().unwrap();
        *launched += 1;
        Acquired::Accepted {
            held: Held {
                id: HostRef(format!("broken-{launched}")),
                acquire: request.acquire.to_string(),
                name: Some(request.name.to_string()),
                offer: request.offer.id.clone(),
                zone: None,
                status: HostStatus::Booting,
            },
            extras: Vec::new(),
        }
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(Vec::new())
    }

    fn release(&self, _id: &HostRef) -> Released {
        match self.flaw {
            Flaw::AnswersFromTerminate => Released::Terminating(HostStatus::Terminating),
            Flaw::LaunchesOnEveryAcquire | Flaw::NeverAnswers => Released::Unknown,
        }
    }

    fn observe(&self, _id: &HostRef) -> Observed {
        Observed::Unknown
    }

    fn verify_instance(&self, _hello: &Hello) -> Option<HostRef> {
        None
    }
}

fn offer() -> Offer {
    Offer {
        id: "broken-type".to_owned(),
        zone: None,
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 1 << 30,
        accelerator: None,
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(1),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
    }
}

/// The rig of the stand-in: its cloud is the provider itself, and no state can be
/// scripted (the cases run here never ask for one).
struct BrokenRig(Broken);

impl BrokenRig {
    fn new(flaw: Flaw) -> Self {
        Self(Broken {
            flaw,
            launched: Mutex::new(0),
        })
    }
}

impl Rig for BrokenRig {
    fn provider(&self) -> &dyn CapacityProvider {
        &self.0
    }

    fn scheduler(&self) -> SchedulerId {
        SchedulerId::new("broken-scheduler").unwrap()
    }

    fn request(&self, acquire: &AcquireId) -> AcquireRequest {
        AcquireRequest {
            acquire: acquire.clone(),
            pool: "pool".to_owned(),
            name: HostName::derive(&self.scheduler(), acquire),
            offer: offer(),
            lifetime: None,
            labels: Vec::new(),
            payload: BootPayload::new(String::new()),
        }
    }

    fn make_room(&mut self, _machines: u32) {
        *self.0.launched.lock().unwrap() = 0;
    }

    fn refuse_next(&mut self, _code: RefusalCode) {}

    fn lose_next_answer_that_landed(&mut self) {}

    fn lose_next_answer_that_did_not_land(&mut self) {}

    fn lose_next_launch_that_did_not_land(&mut self) {}

    fn fail_read_after_next_terminate(&mut self) {}

    fn foreign_machine_exists(&mut self) -> Vec<HostRef> {
        Vec::new()
    }

    fn fail_list(&mut self) {}

    fn forget(&mut self, _id: &HostRef) {}

    fn throttle_reads(&mut self) {}

    fn outwait_unresolved(&mut self) {}

    fn launches(&self) -> usize {
        *self.0.launched.lock().unwrap()
    }

    fn terminates(&self) -> usize {
        0
    }
}

/// Asserts that the idempotency case fails on a provider that launches a new machine on
/// a retry with the same acquire id. Catches a case that cannot fail, which would make a
/// green run of the suite on a real provider prove nothing.
#[test]
#[should_panic(
    expected = "acquire_is_idempotent_on_the_acquire_id: a retry answers the machine of the first acquire"
)]
fn a_conformance_case_fails_on_a_provider_that_breaks_its_rule() {
    acquire_is_idempotent_on_the_acquire_id(&mut BrokenRig::new(Flaw::LaunchesOnEveryAcquire));
}

/// Asserts that a case that needs a launch fails, naming the answer, on a provider whose
/// acquire never answers: a case never reads a missing launch as a pass.
#[test]
#[should_panic(
    expected = "acquire_is_idempotent_on_the_acquire_id: an acquire the cloud has room for answers Unknown, not Accepted"
)]
fn a_case_fails_on_a_provider_whose_acquire_never_answers() {
    acquire_is_idempotent_on_the_acquire_id(&mut BrokenRig::new(Flaw::NeverAnswers));
}

/// Asserts that the refusal case fails, naming the constant it wanted, on a provider that
/// launches where its cloud refused: a refusal never reads as a pass when the provider
/// answers something else.
#[test]
#[should_panic(
    expected = "refused_codes_are_the_plain_constants: a refusal for Capacity answers Accepted"
)]
fn the_refusal_case_fails_on_a_provider_that_launches_instead() {
    refused_codes_are_the_plain_constants(&mut BrokenRig::new(Flaw::LaunchesOnEveryAcquire));
}

/// Asserts that the release case fails on a provider that answers a release from the
/// terminate call alone: when the read after the terminate gets no answer, such a provider
/// still says the machine is terminating.
#[test]
#[should_panic(
    expected = "release_terminates_then_reads_back: a release whose read after the terminate got no answer is Unknown"
)]
fn the_release_case_fails_on_a_provider_that_answers_from_the_terminate() {
    release_terminates_then_reads_back(&mut BrokenRig::new(Flaw::AnswersFromTerminate));
}
