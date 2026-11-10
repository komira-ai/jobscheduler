//! One elastic pool's cloud in the cell: the elastic fake, on the cell's clock, behind a
//! recording [`CapacityProvider`] that the server's scaler is handed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_agent_wire::Phase;
use js_elastic::PriceUnit;
use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, Capacity, CapacityProvider, CatalogFuture,
    CheckResult, EventStream, Held, Hello, HostRef, Observed, Offer, ProviderCaps, ProviderError,
    RefusalCode, Released,
};
use js_elastic_fake::{ElasticFake, FakeClock, PROVIDER_ID, Step};
use js_registry::{HostState, HostView, Registry};
use js_types::sched::FarmTime;
use js_types::{ExecMode, HostId, Model, ProviderId, accelerator_of};

use super::record::{
    AcquireAnswer, AcquireSeen, CloudCall, CloudEvent, EarlierSeen, ReleaseAnswer,
};
use super::{ElasticPool, OfferSpec};
use crate::counters::{Counter, Counters};
use crate::server::{EPOCH_MS, Shape, virtual_time};

/// An offer of a cloud by its id and zone.
type OfferKey = (String, Option<String>);

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
///
/// A cloud of a server of [`crate::ServerSpec::ordered`] is one machine provider of the
/// pool ([`Cloud::ordered`]): it is named, under the provider id of its name, and knows the
/// clouds of the providers before it. At each acquire the wrapper also writes down what it
/// sees then ([`AcquireSeen`]): the hosts the server's registry holds open to a job (once
/// the server node has handed it the registry, [`Cloud::watch`]) and how each earlier
/// cloud stands. [`Cloud::quota_refuse`] makes the fake refuse, for `quota`, every acquire
/// of an offer in a zone from then on: the fake's own script takes a step for the next call
/// whatever its offer, so the wrapper queues that step just before such an acquire, and the
/// fake answers it.
#[derive(Clone)]
pub struct Cloud {
    pool: ElasticPool,
    shape: Shape,
    fake: ElasticFake,
    /// The provider id of a named cloud ([`Cloud::ordered`]); `None` for the cloud of a
    /// server of one elastic pool, which keeps the fake's own.
    id: Option<ProviderId>,
    /// Its place among the pool's launching providers, from 0.
    rank: usize,
    /// The clouds of the providers before it, first to last.
    earlier: Vec<Cloud>,
    /// The offers, by id and zone, every acquire of which the fake refuses for `quota`.
    quota: Arc<Mutex<BTreeSet<OfferKey>>>,
    /// The server's registry, once the server node has handed it over.
    registry: Arc<Mutex<Option<Registry>>>,
    /// What the wrapper saw at each acquire.
    seen: Arc<Mutex<Vec<AcquireSeen>>>,
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
            id: None,
            rank: 0,
            earlier: Vec::new(),
            quota: Arc::default(),
            registry: Arc::default(),
            seen: Arc::default(),
            record: Arc::default(),
            armed: Arc::default(),
            crashed: Arc::default(),
            counters: Arc::default(),
            joins: Arc::new(AtomicBool::new(pool.joined())),
        }
    }

    /// The cloud of `pool`, as [`Cloud::new`], as the machine provider of `pool`'s name that
    /// the pool asks after the providers of `earlier` (first to last): under the provider id
    /// of that name, or the fake's own when the name is not an id (the server then refuses
    /// the second of two such providers in one pool).
    #[must_use]
    pub fn ordered(pool: &ElasticPool, shape: &Shape, earlier: &[Cloud]) -> Self {
        let id = ProviderId::new(pool.name()).unwrap_or(PROVIDER_ID);
        Self {
            id: Some(id),
            rank: earlier.len(),
            earlier: earlier.to_vec(),
            ..Self::new(pool, shape)
        }
    }

    /// The machine provider's name, for a cloud of [`Cloud::ordered`]; `None` for the
    /// cloud of a server of one elastic pool.
    #[must_use]
    pub fn named(&self) -> Option<&str> {
        self.id.is_some().then(|| self.pool.name())
    }

    /// Its place among the pool's launching providers, from 0.
    #[must_use]
    pub fn rank(&self) -> usize {
        self.rank
    }

    /// Makes the fake refuse every acquire of offer `offer` in `zone` for `quota` from now
    /// on.
    pub fn quota_refuse(&self, offer: &str, zone: Option<&str>) {
        let key = (offer.to_owned(), zone.map(str::to_owned));
        self.quotas().insert(key);
    }

    /// Hands the wrapper the server's registry (`None` for a server without one), whose
    /// open hosts it reads at each acquire from now on.
    pub(crate) fn watch(&self, registry: Option<Registry>) {
        *self.registry.lock().unwrap_or_else(PoisonError::into_inner) = registry;
    }

    /// What the wrapper saw at each acquire after the first `skip`.
    pub(crate) fn seen_after(&self, skip: usize) -> Vec<AcquireSeen> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.iter().skip(skip).cloned().collect()
    }

    fn quotas(&self) -> MutexGuard<'_, BTreeSet<OfferKey>> {
        self.quota.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How this cloud stands for a later provider at an acquire made now: what it holds, its
    /// share, and its offers whose latest answer was not a definite refusal.
    fn standing(&self) -> EarlierSeen {
        let truth = self.fake.truth();
        let shown: BTreeSet<&str> = truth.iter().map(|held| held.acquire.as_str()).collect();
        let calls = self.calls();
        let mut last: BTreeMap<&str, &AcquireAnswer> = BTreeMap::new();
        let mut words: BTreeMap<(&str, Option<&str>), (FarmTime, &AcquireAnswer)> = BTreeMap::new();
        for call in &calls {
            if let CloudEvent::Acquire {
                offer,
                zone,
                acquire,
                answer,
            } = &call.event
            {
                last.insert(acquire.as_str(), answer);
                words.insert((offer.as_str(), zone.as_deref()), (call.at, answer));
            }
        }
        let waiting = last.iter().filter(|(acquire, answer)| {
            let open = matches!(answer, AcquireAnswer::Unknown | AcquireAnswer::Deferred(_));
            open & !shown.contains(*acquire)
        });
        let held = truth.len() + waiting.count();
        let (now, backoff) = (self.now(), self.pool.quota_backoff());
        let definite = |spec: &&OfferSpec| {
            let word = words.get(&(spec.id.as_str(), spec.zone.as_deref()));
            let ago = |at: &FarmTime| now.saturating_duration_since(*at);
            word.is_some_and(|(at, answer)| refusal_holds(answer, ago(at), backoff))
        };
        let open = self.pool.offers().iter().filter(|spec| !definite(spec));
        EarlierSeen {
            provider: self.pool.name().to_owned(),
            held: u64::try_from(held).unwrap_or(u64::MAX),
            max: self.pool.max_instances(),
            open: open
                .map(|spec| (spec.id.clone(), spec.zone.clone()))
                .collect(),
        }
    }

    /// The hosts of the server's registry open to a new job now ([`open`]).
    fn open_hosts(&self) -> Vec<HostId> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let hosts = registry.as_ref().map(Registry::hosts).unwrap_or_default();
        hosts.into_iter().filter(open).map(|host| host.id).collect()
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
        self.0.id.clone().unwrap_or_else(|| self.0.fake.id())
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
        let (offer, zone) = (&request.offer.id, &request.offer.zone);
        let refused = self.0.quotas().contains(&(offer.clone(), zone.clone()));
        if refused {
            self.0.fake.script_next(Step::Refuse(RefusalCode::Quota));
        }
        let open_hosts = self.0.open_hosts();
        let earlier = self.0.earlier.iter().map(Cloud::standing).collect();
        let answer = self.0.fake.acquire(request);
        if let Some(counter) = counted(&answer) {
            self.0.counted().bump(counter);
        }
        let recorded = AcquireAnswer::of(&answer);
        let seen = AcquireSeen {
            at: self.0.now(),
            offer: offer.clone(),
            zone: zone.clone(),
            acquire: request.acquire.as_str().to_owned(),
            answer: recorded.clone(),
            open_hosts,
            earlier,
        };
        self.0
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seen);
        self.0.note(CloudEvent::Acquire {
            offer: offer.clone(),
            zone: zone.clone(),
            acquire: request.acquire.as_str().to_owned(),
            answer: recorded,
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

/// Whether `answer`, given `ago` before now, still rules its offer out for a later
/// provider: a `capacity` refusal does until the offer is asked again, a `quota` one for
/// `backoff`; nothing else does.
fn refusal_holds(answer: &AcquireAnswer, ago: Duration, backoff: Duration) -> bool {
    match answer {
        AcquireAnswer::Refused(RefusalCode::Capacity) => true,
        AcquireAnswer::Refused(RefusalCode::Quota) => ago < backoff,
        _ => false,
    }
}

/// Whether the registry holds `host` open to a new job: live, serving, heard since its
/// hello, not tainted, cordoned or set aside, and not releasing a run of before.
fn open(host: &HostView) -> bool {
    let live = matches!(host.state, HostState::Live);
    let serving = matches!(host.phase, Phase::Serving);
    let clear = host.taint.is_none() & host.mark.is_none() & host.cordon_until.is_none();
    live & serving & clear & host.heard_since_hello & !host.releasing
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

    /// Catches: an earlier provider read as unable to serve for an answer that is no
    /// definite refusal (a machine, a lost answer, a deferral, a refusal for `shape` or
    /// `denied`), a `capacity` refusal that lapses, and a `quota` refusal that holds past its
    /// backoff or ends before it.
    #[test]
    fn only_capacity_and_a_fresh_quota_refusal_rule_an_offer_out() {
        let backoff = Duration::from_secs(600);
        let (fresh, stale) = (Duration::from_secs(599), Duration::from_secs(600));
        let long = Duration::from_secs(86_400);
        let refused = AcquireAnswer::Refused;
        let cases = [
            (refused(RefusalCode::Capacity), long, true),
            (refused(RefusalCode::Quota), fresh, true),
            (refused(RefusalCode::Quota), stale, false),
            (refused(RefusalCode::Shape), Duration::ZERO, false),
            (refused(RefusalCode::Denied), Duration::ZERO, false),
            (AcquireAnswer::Unknown, Duration::ZERO, false),
            (AcquireAnswer::Deferred(backoff), Duration::ZERO, false),
            (
                AcquireAnswer::Accepted {
                    machine: "fake-0001".to_owned(),
                },
                Duration::ZERO,
                false,
            ),
        ];
        for (answer, ago, holds) in cases {
            assert_eq!(refusal_holds(&answer, ago, backoff), holds, "{answer:?}");
        }
    }

    /// Catches: a cloud of an ordered server that keeps the fake's provider id (the server
    /// refuses two providers of one id in a pool), a name or a place lost, and a cloud of a
    /// server of one elastic pool that is named or takes another id than the fake's.
    #[test]
    fn an_ordered_cloud_is_named_and_placed_after_the_clouds_before_it() {
        let first = cloud();
        assert_eq!((first.named(), first.rank()), (None, 0));
        assert_eq!(first.provider().id(), PROVIDER_ID);
        let pool = ElasticPool::lambda_like("lambda", Vec::new(), 1);
        let second = Cloud::ordered(&pool, &Shape::gpu_host(), &[first]);
        assert_eq!((second.named(), second.rank()), (Some("lambda"), 1));
        assert_eq!(second.provider().id().as_str(), "lambda");
        let odd = ElasticPool::lambda_like("Not An Id", Vec::new(), 1);
        let odd = Cloud::ordered(&odd, &Shape::gpu_host(), &[]);
        assert_eq!(odd.provider().id(), PROVIDER_ID);
        assert!(odd.seen_after(0).is_empty());
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
