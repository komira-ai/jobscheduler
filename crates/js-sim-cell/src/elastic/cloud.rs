//! One elastic pool's cloud in the cell: the elastic fake, on the cell's clock, behind a
//! recording [`CapacityProvider`] that the server's scaler is handed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::PriceUnit;
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture,
    CheckResult, EventStream, Held, Hello, HostRef, Observed, Offer, ProviderCaps, ProviderError,
    Released,
};
use js_elastic_fake::{ElasticFake, FakeClock};
use js_types::sched::FarmTime;
use js_types::{ExecMode, Model, ProviderId, accelerator_of};

use super::record::{AcquireAnswer, CloudCall, CloudEvent, ReleaseAnswer};
use super::{ElasticPool, OfferSpec};
use crate::counters::{Counter, Counters};
use crate::server::{EPOCH_MS, Shape, virtual_time};

/// One elastic pool's cloud: the [`ElasticFake`] the server launches machines on, and the
/// record of every call the server made to it ([`CloudCall`]). Clones share both.
///
/// The fake's clock starts at [`EPOCH_MS`], virtual time zero, and only the cell moves it:
/// before every input of the server it is advanced to the input's virtual time
/// ([`Cloud::advance_to`]), so the server and the cloud read one clock, and a call is
/// recorded at the virtual time of the step it happened in.
///
/// [`Cloud::crash_at_next_acquire`] makes the next acquire crash the server: the call
/// reaches the fake, which launches the machine, and the server is answered as for a call
/// whose answer was lost (`Acquired::Unknown`) and stopped at the end of the tick, before a
/// later tick acts on anything ([`CloudEvent::Crash`]). What survives is what the scaler
/// had written before the call and in the rest of that tick.
///
/// The cloud counts what a witness reads of it ([`Cloud::counters`]): each acquire the fake
/// answered with no answer, a deferral or a refusal, and each machine it preempted while it
/// ran. Whether a booted machine says hello starts as its pool says
/// ([`ElasticPool::joined`]) and can be switched while the cell runs ([`Cloud::joins`]).
#[derive(Clone)]
pub struct Cloud {
    pool: ElasticPool,
    shape: Shape,
    fake: ElasticFake,
    record: Arc<Mutex<Vec<CloudCall>>>,
    /// Whether the next acquire crashes the server.
    armed: Arc<AtomicBool>,
    /// Whether an acquire crashed the server since the node last asked.
    crashed: Arc<AtomicBool>,
    /// What the cloud counted.
    counters: Arc<Mutex<Counters>>,
    /// Whether a machine that boots from now on says hello.
    joins: Arc<AtomicBool>,
}

impl Cloud {
    /// The cloud of `pool`, empty, at virtual time zero, with the pool's knobs, offering
    /// each of the pool's offers as a machine of `shape` ([`offer`]).
    #[must_use]
    pub fn new(pool: &ElasticPool, shape: &Shape) -> Self {
        let offers = pool.offers().iter().map(|spec| offer(spec, shape));
        let clock = FakeClock::new(EPOCH_MS);
        let fake = ElasticFake::new(clock, pool.billing(), offers.collect());
        fake.set_knobs(pool.cloud_knobs().clone());
        Self {
            pool: pool.clone(),
            shape: shape.clone(),
            fake,
            record: Arc::default(),
            armed: Arc::default(),
            crashed: Arc::default(),
            counters: Arc::default(),
            joins: Arc::new(AtomicBool::new(pool.joined())),
        }
    }

    /// What the cloud counted so far: [`Counter::AcquireLost`],
    /// [`Counter::AcquireThrottled`], [`Counter::AcquireRefused`] and
    /// [`Counter::MachinePreempted`].
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.counted().clone()
    }

    /// Whether a machine that boots from now on says hello: the pool's
    /// [`ElasticPool::joined`] until [`crate::Cell::machines_never_join`] switches it.
    #[must_use]
    pub fn joins(&self) -> bool {
        self.joins.load(Ordering::SeqCst)
    }

    /// Makes the machines that boot from now on say hello (`true`) or never (`false`).
    pub(crate) fn set_joins(&self, joins: bool) {
        self.joins.store(joins, Ordering::SeqCst);
    }

    /// Makes the fake preempt machine `id` (`ElasticFake::preempt`), counting
    /// [`Counter::MachinePreempted`] when the fake was running it.
    pub(crate) fn preempt(&self, id: &HostRef) {
        let running = self.fake.running().contains(id);
        self.fake.preempt(id);
        if running {
            self.counted().bump(Counter::MachinePreempted);
        }
    }

    fn counted(&self) -> MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the next acquire the server makes crash it, as a process that dies after the
    /// call left and before it read the answer (see the type's doc).
    pub fn crash_at_next_acquire(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Whether an acquire crashed the server since the last call, which the server node
    /// makes after every tick and then stops its server.
    pub(crate) fn take_crashed(&self) -> bool {
        self.crashed.swap(false, Ordering::SeqCst)
    }

    /// What every machine of the cloud is: the server's shape.
    #[must_use]
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// The pool this cloud serves.
    #[must_use]
    pub fn pool(&self) -> &ElasticPool {
        &self.pool
    }

    /// The elastic fake: what it holds, what it billed, and its scripts.
    #[must_use]
    pub fn fake(&self) -> &ElasticFake {
        &self.fake
    }

    /// Every call recorded so far, in the order made.
    #[must_use]
    pub fn calls(&self) -> Vec<CloudCall> {
        self.lock().clone()
    }

    /// The calls recorded after the first `skip`.
    pub(crate) fn calls_after(&self, skip: usize) -> Vec<CloudCall> {
        self.lock().iter().skip(skip).cloned().collect()
    }

    /// The provider the server's scaler is handed: the fake behind the recording wrapper.
    pub(crate) fn provider(&self) -> Box<dyn CapacityProvider> {
        Box::new(Recording(self.clone()))
    }

    /// Moves the fake's clock forward to virtual time `at`; a time it has passed leaves it
    /// where it is (the clock only advances).
    pub(crate) fn advance_to(&self, at: FarmTime) {
        let clock = self.fake.clock();
        let target = EPOCH_MS.saturating_add(at.as_millis());
        let by = target.saturating_sub(clock.now_ms());
        clock.advance(Duration::from_millis(by));
    }

    /// The virtual time the fake's clock reads.
    #[must_use]
    pub fn now(&self) -> FarmTime {
        virtual_time(self.fake.clock().now_ms())
    }

    /// Notes that the pool's brake read `tripped` after a tick, when that differs
    /// from the last reading recorded (none reads not tripped).
    pub(crate) fn note_brake(&self, tripped: bool) {
        let mut record = self.lock();
        let mut readings = record.iter().filter_map(|call| match call.event {
            CloudEvent::Brake { tripped } => Some(tripped),
            CloudEvent::BrakeReset => Some(false),
            _ => None,
        });
        if readings.next_back().unwrap_or(false) != tripped {
            let at = self.now();
            let event = CloudEvent::Brake { tripped };
            record.push(CloudCall { at, event });
        }
    }

    /// Notes the operator's reset of the pool's brake.
    pub(crate) fn note_reset(&self) {
        self.note(CloudEvent::BrakeReset);
    }

    fn note(&self, event: CloudEvent) {
        let at = self.now();
        self.lock().push(CloudCall { at, event });
    }

    fn lock(&self) -> MutexGuard<'_, Vec<CloudCall>> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The fake behind the recording wrapper: every call is forwarded as it is, and each
/// acquire, release and list is recorded with its arguments and its answer.
struct Recording(Cloud);

impl CapacityProvider for Recording {
    fn id(&self) -> ProviderId {
        self.0.fake.id()
    }

    fn can_launch(&self) -> bool {
        self.0.fake.can_launch()
    }

    fn capabilities(&self) -> ProviderCaps {
        self.0.fake.capabilities()
    }

    fn catalog(&self) -> CatalogFuture<'_> {
        self.0.fake.catalog()
    }

    fn capacity(&self) -> Capacity {
        self.0.fake.capacity()
    }

    fn checks(&self) -> Vec<CheckResult> {
        self.0.fake.checks()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        self.0.fake.host_prep(offer)
    }

    fn payload_limit(&self) -> usize {
        self.0.fake.payload_limit()
    }

    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        let answer = self.0.fake.acquire(request);
        if let Some(counter) = counted(&answer) {
            self.0.counted().bump(counter);
        }
        self.0.note(CloudEvent::Acquire {
            offer: request.offer.id.clone(),
            zone: request.offer.zone.clone(),
            acquire: request.acquire.as_str().to_owned(),
            answer: AcquireAnswer::of(&answer),
        });
        let crash = self.0.armed.swap(false, Ordering::SeqCst);
        if crash {
            self.0.crashed.store(true, Ordering::SeqCst);
            self.0.note(CloudEvent::Crash);
            return Acquired::Unknown;
        }
        answer
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        let answer = self.0.fake.list();
        let listed = answer.as_ref().ok().map(|held| {
            let ids = held.iter().map(|machine| machine.id.as_str().to_owned());
            ids.collect()
        });
        self.0.note(CloudEvent::List { listed });
        answer
    }

    fn release(&self, id: &HostRef) -> Released {
        let answer = self.0.fake.release(id);
        self.0.note(CloudEvent::Release {
            machine: id.as_str().to_owned(),
            answer: ReleaseAnswer::of(&answer),
        });
        answer
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.0.fake.unresolved()
    }

    fn observe(&self, id: &HostRef) -> Observed {
        self.0.fake.observe(id)
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.0.fake.is_gone(id)
    }

    fn verify_instance(&self, hello: &Hello) -> Option<HostRef> {
        self.0.fake.verify_instance(hello)
    }

    /// No reclaim notices: the server reads such a stream on a thread of its own, which
    /// would act at a moment of real time rather than of the seed's (see the module doc of
    /// `elastic`). A preempted machine is found by the scaler's next list.
    fn events(&self) -> Option<EventStream> {
        None
    }
}

/// What the cloud counts of an acquire the fake answered `answer`: nothing for a machine.
fn counted(answer: &Acquired) -> Option<Counter> {
    match answer {
        Acquired::Accepted { .. } => None,
        Acquired::Unknown => Some(Counter::AcquireLost),
        Acquired::Deferred { .. } => Some(Counter::AcquireThrottled),
        Acquired::Refused(_) => Some(Counter::AcquireRefused),
    }
}

/// The cloud's offer of `spec`: a machine of `shape` (its cores, its memory and its GPUs,
/// of its GPU class), with 512 GiB of disk, that runs containers on `linux` and `x86_64`,
/// at the spec's price.
#[must_use]
pub fn offer(spec: &OfferSpec, shape: &Shape) -> Offer {
    let count = u32::try_from(shape.gpus).unwrap_or(u32::MAX);
    let model = shape
        .gpu_class
        .as_deref()
        .and_then(|c| Model::parse(c).ok());
    Offer {
        id: spec.id.clone(),
        zone: spec.zone.clone(),
        cpu_millis: shape.cpu_millis,
        memory_bytes: shape.memory_bytes,
        disk_bytes: 512 << 30,
        accelerator: model.map(|model| accelerator_of(&model, count, None)),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        isa_level: None,
        exec: ExecMode::Container,
        price_cents_per_hour: Some(spec.cents_per_hour),
        price_unit: PriceUnit::UsCents,
        available: true,
        purchase: None,
        quota: None,
        block: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cloud of a pool of one offer, on the server's GPU host.
    fn cloud() -> Cloud {
        let offers = vec![OfferSpec::new("sim_1x_a100", "zone-a", 120)];
        Cloud::new(
            &ElasticPool::ec2_like("burst", offers, 2),
            &Shape::gpu_host(),
        )
    }

    /// Catches: a wrapper that answers a call itself rather than asking the fake (the
    /// scaler would be judged against a cloud that is not the one billed), that records a
    /// call other than an acquire, a release or a list, or that hands the server a stream
    /// of reclaim notices (read on a thread of the server's own, at a moment of real time).
    #[test]
    fn the_wrapper_forwards_every_call_and_sends_no_notices() {
        let cloud = cloud();
        let provider = cloud.provider();
        let fake = cloud.fake();
        let offer = offer(&cloud.pool().offers()[0], cloud.shape());
        let id = HostRef("fake-0001".to_owned());
        let hello = Hello {
            name: "js-0-0".to_owned(),
            instance_id: None,
            identity: None,
        };
        assert_eq!(provider.id(), fake.id());
        assert_eq!(provider.can_launch(), fake.can_launch());
        assert_eq!(provider.capabilities(), fake.capabilities());
        assert_eq!(provider.capacity(), fake.capacity());
        assert_eq!(provider.checks(), fake.checks());
        assert_eq!(provider.host_prep(&offer), fake.host_prep(&offer));
        assert_eq!(provider.payload_limit(), fake.payload_limit());
        assert_eq!(provider.unresolved(), fake.unresolved());
        assert_eq!(provider.observe(&id), fake.observe(&id));
        assert_eq!(provider.is_gone(&id), fake.is_gone(&id));
        assert_eq!(
            provider.verify_instance(&hello),
            fake.verify_instance(&hello)
        );
        assert!(provider.events().is_none());
        assert!(cloud.calls().is_empty(), "none of these is recorded");
        assert_eq!(cloud.shape(), &Shape::gpu_host());
    }

    /// Catches: a brake noted at every tick rather than when it changes, a trip after an
    /// operator's reset taken as already noted (the reset reads as untripped), and the
    /// clock of the cloud moved back, or not to the input's virtual time.
    #[test]
    fn the_brake_is_noted_when_it_changes_on_the_cells_clock() {
        let cloud = cloud();
        cloud.note_brake(false);
        assert!(cloud.calls().is_empty(), "untripped is how a pool starts");
        cloud.advance_to(FarmTime::from_millis(5_000));
        let _ = cloud.provider().list();
        cloud.note_brake(true);
        cloud.note_brake(true);
        cloud.note_reset();
        cloud.advance_to(FarmTime::from_millis(4_000));
        cloud.note_brake(true);
        let events: Vec<(u64, CloudEvent)> = cloud
            .calls()
            .into_iter()
            .map(|call| (call.at.as_millis(), call.event))
            .collect();
        let listed = CloudEvent::List {
            listed: Some(Vec::new()),
        };
        let tripped = CloudEvent::Brake { tripped: true };
        assert_eq!(
            events,
            [
                (5_000, listed),
                (5_000, tripped.clone()),
                (5_000, CloudEvent::BrakeReset),
                (5_000, tripped),
            ]
        );
        assert_eq!(
            cloud.now(),
            FarmTime::from_millis(5_000),
            "the clock never goes back"
        );
        assert_eq!(cloud.calls_after(3).len(), 1);
    }

    /// Catches: an acquire answer counted as another (a lost answer as a refusal, a 429 as
    /// a lost answer), and a machine counted as a fault.
    #[test]
    fn each_faulty_acquire_answer_is_counted_as_what_it_was() {
        let held = Held {
            id: HostRef("fake-0001".to_owned()),
            acquire: "burst-1".to_owned(),
            name: None,
            offer: "sim_1x_a100".to_owned(),
            zone: None,
            status: js_elastic::HostStatus::Booting,
            ends_unix_seconds: None,
        };
        let refusal = js_elastic::Refusal {
            code: js_elastic::RefusalCode::Capacity,
            reason: js_elastic::Reason::Api("scripted".to_owned()),
        };
        let cases = [
            (
                Acquired::Accepted {
                    held,
                    extras: Vec::new(),
                },
                None,
            ),
            (Acquired::Unknown, Some(Counter::AcquireLost)),
            (
                Acquired::Deferred {
                    wait: Duration::from_secs(30),
                },
                Some(Counter::AcquireThrottled),
            ),
            (Acquired::Refused(refusal), Some(Counter::AcquireRefused)),
        ];
        for (answer, counter) in cases {
            assert_eq!(counted(&answer), counter);
        }
    }

    /// Catches: a preemption counted for a machine the cloud was not running (an id it does
    /// not hold, or one already preempted), and a join switch that does not start as the
    /// pool says or does not switch.
    #[test]
    fn a_preemption_is_counted_once_and_the_join_switch_switches() {
        let cloud = cloud();
        let foreign = offer(&cloud.pool().offers()[0], cloud.shape());
        let id = cloud.fake().add_foreign(None, foreign);
        cloud.preempt(&HostRef("fake-0099".to_owned()));
        assert_eq!(cloud.counters().get(Counter::MachinePreempted), 0);
        cloud.preempt(&id);
        cloud.preempt(&id);
        assert_eq!(cloud.counters().get(Counter::MachinePreempted), 1);
        assert!(cloud.fake().running().is_empty());
        assert!(cloud.joins());
        cloud.set_joins(false);
        assert!(!cloud.joins());
        let silent = ElasticPool::ec2_like("burst", Vec::new(), 1).joins(false);
        assert!(!Cloud::new(&silent, &Shape::gpu_host()).joins());
    }
}
