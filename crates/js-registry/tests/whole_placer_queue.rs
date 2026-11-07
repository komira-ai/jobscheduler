//! A placer that keeps the whole-node queue (`Placer::whole_round` and the `whole_*` calls,
//! the engine of `--placement js-sched`): the registry hands it its view of the hosts at
//! every pass, shows the marks it holds ([`Placer::whole_reservations`]) as its
//! reservations, commits a request on the host set aside for it and tells it when a
//! request leaves the wait list. The registry is driven on a manual clock; the placer here
//! answers presentations and marks from a script and records every round.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{DEFAULT_GRACE_MS, Heartbeat, Taint};
use js_lease::{ManualClock, Timestamp};
use js_provider::{Candidate, Pool, StartAck, StartSpec};
use js_registry::{
    DEFAULT_PREPARE_TIMEOUT_MS, MarkedTails, Placer, PoolConfig, Presented, ProfileLives,
    RegisteredProvider, Registry, Reservation, SequentialSessions, Standing, WholeHost,
    WholeNodeConfig, WholeRound,
};
use js_types::{Clock, LeaseId, ProviderId, Request, RequestId};
use js_usage::ProfileKey;
use support::world::*;
use support::*;

/// A placer that keeps the queue: it books the first host it is shown, lets each presented
/// request wait, holds the marks the script gives it, and records every round and every
/// request taken off its wait list.
#[derive(Default)]
struct Queue {
    /// The marks the placer holds now ([`Placer::whole_reservations`]).
    held: Mutex<Vec<Reservation>>,
    /// What the placer answers each round with, which the registry does not take as marks.
    answered: Mutex<Vec<Reservation>>,
    rounds: Mutex<Vec<WholeRound>>,
    cancelled: Mutex<Vec<RequestId>>,
}

impl Queue {
    /// The last round the registry handed over.
    fn last(&self) -> WholeRound {
        self.rounds.lock().unwrap().last().cloned().unwrap()
    }

    /// How many rounds the registry handed over.
    fn count(&self) -> usize {
        self.rounds.lock().unwrap().len()
    }
}

impl Placer for Queue {
    fn pick(&self, _: &StartSpec, _: &[Candidate], _: &MarkedTails) -> Option<usize> {
        Some(0)
    }

    fn whole_round(&self, round: &WholeRound) -> Option<Vec<Reservation>> {
        self.rounds.lock().unwrap().push(round.clone());
        Some(self.answered.lock().unwrap().clone())
    }

    fn whole_reservations(&self) -> Option<Vec<Reservation>> {
        Some(self.held.lock().unwrap().clone())
    }

    fn whole_present(
        &self,
        _: &RequestId,
        _: LeaseId,
        _: &Request,
        _: bool,
        _: Duration,
    ) -> Presented {
        Presented::Waiting(0)
    }

    fn whole_cancel(&self, id: &RequestId) -> bool {
        self.cancelled.lock().unwrap().push(id.clone());
        true
    }
}

/// A fixed table of P90 minutes by profile name.
struct Table(BTreeMap<&'static str, u32>);

impl ProfileLives for Table {
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
        self.0.get(key.as_str()).copied()
    }
}

/// A registry of up to four hosts, placed by a [`Queue`], on a manual clock.
struct Rig {
    registry: Registry,
    provider: RegisteredProvider,
    clock: ManualClock,
    queue: Arc<Queue>,
    /// Each joined host's epoch and session secret.
    joined: BTreeMap<String, (u64, String)>,
}

impl Rig {
    /// A dedicated pool of the default whole-node settings with `hosts` joined.
    fn new(hosts: &[&str]) -> Self {
        let name = ProviderId::new("builders").unwrap();
        let config = PoolConfig::new(name, whole(), pool_shape(), 5);
        let config = config
            .with_whole_node(WholeNodeConfig::dedicated(5))
            .unwrap();
        let clock = ManualClock::new(Timestamp::from_millis(1_000_000));
        let queue = Arc::new(Queue::default());
        let sessions = SequentialSessions::new();
        let registry = Registry::with_sessions(config, clock.clone(), sessions, queue.clone());
        let provider = registry
            .provider()
            .with_classes(vec![share_class(), whole()]);
        let mut rig = Self {
            registry,
            provider,
            clock,
            queue,
            joined: BTreeMap::new(),
        };
        hosts.iter().for_each(|name| rig.join(name));
        rig
    }

    /// Says hello for `name`, keeping its epoch and session.
    fn join(&mut self, name: &str) {
        let welcome = self.registry.hello(&hello(name), &cred("t")).unwrap();
        self.joined
            .insert(name.to_owned(), (welcome.epoch, welcome.session));
    }

    /// A heartbeat of `name` in its current epoch and session.
    fn message(&self, name: &str) -> Heartbeat {
        let (epoch, session) = self.joined[name].clone();
        Heartbeat {
            session,
            ..heartbeat(name, epoch)
        }
    }

    /// Sends `message` as `name`'s heartbeat.
    fn send(&self, message: &Heartbeat) {
        self.registry.heartbeat(message, &cred("t")).unwrap();
    }

    /// Moves the clock on by `ms`.
    fn step(&self, ms: u64) {
        self.clock.advance(Duration::from_millis(ms));
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Starts whole-node request `name` for lease `sequence`.
    fn ask(&mut self, sequence: u64, name: &str) -> StartAck {
        let spec = World::whole_spec(sequence, Some(name), 600);
        self.provider.start(spec)
    }

    /// The lease booked on `name`, if any.
    fn lease_on(&self, name: &str) -> Option<LeaseId> {
        self.provider.booked().get(&host(name)).copied()
    }

    /// Cordons `name` for ten minutes: the registry books nothing on it until then.
    fn cordon(&self, name: &str) {
        let ten = Duration::from_secs(600);
        self.registry.cordon(&host(name), ten).unwrap();
    }
}

/// A mark of request `name` on `on`, made at `at` ms with a deadline a minute later.
fn mark(name: &str, on: &str, at: u64, assigned: bool) -> Reservation {
    Reservation {
        request: id(name),
        host: host(on),
        at: Timestamp::from_millis(at),
        deadline: Timestamp::from_millis(at + 60_000),
        assigned,
        tail_end: (!assigned).then_some(at + 600_000),
    }
}

/// Catches: the registry adopting the marks a placer answers its round with instead of the
/// ones it holds (the registry would show `b` set aside for `decoy`), the placer's held
/// mark not shown, or the placer handed a request, a mark held outside or lives of the
/// registry's own. `a` is cordoned and the placer holds a mark of `r` on it: the registry
/// shows that mark alone, and hands the placer a round with the pool's whole-node settings
/// and nothing waiting, held outside or living.
#[test]
fn the_registry_shows_the_marks_the_placer_holds_and_hands_it_no_request() {
    let mut rig = Rig::new(&["a", "b"]);
    rig.cordon("a");
    let drain = mark("r", "a", rig.now().millis(), false);
    *rig.queue.held.lock().unwrap() = vec![drain.clone()];
    *rig.queue.answered.lock().unwrap() = vec![mark("decoy", "b", rig.now().millis(), false)];
    assert_eq!(rig.ask(2, "r"), StartAck::Unknown);
    assert_eq!(rig.registry.reservations(), [drain]);
    let round = rig.queue.last();
    assert_eq!(round.settings, WholeNodeConfig::dedicated(5));
    assert!(round.waiting.is_empty());
    assert!(round.outside.is_empty());
    assert!(round.lives.is_empty());
    assert_eq!(rig.lease_on("b"), None);
}

/// Catches: a request whose start is not booked on the quiet host the placer holds for it,
/// or a placer left uninformed once the request is placed (it would keep the request
/// queued), or a cancel the registry answers itself. The placer holds the quiet `b` for
/// `r` (`a` carries a lease): the start of `r` commits there, and the placer is told `r`
/// left its wait list. A cancel of `s` is handed to the placer and answered as it answers.
#[test]
fn a_request_commits_on_the_host_set_aside_and_the_placer_is_told_it_left() {
    let mut rig = Rig::new(&["a", "b"]);
    assert_eq!(
        started_on(&rig.provider.start(World::share_spec(1, 900))),
        Some("a")
    );
    *rig.queue.held.lock().unwrap() = vec![mark("r", "b", rig.now().millis(), true)];
    assert_eq!(rig.ask(2, "r"), StartAck::Unknown);
    assert_eq!(rig.lease_on("b"), Some(lease(2)));
    assert_eq!(*rig.queue.cancelled.lock().unwrap(), [id("r")]);
    let asked = rig.queue.count();
    rig.registry.advance();
    assert_eq!(rig.queue.count(), asked + 1);
    assert!(rig.registry.cancel_request(&id("s")));
    assert_eq!(*rig.queue.cancelled.lock().unwrap(), [id("r"), id("s")]);
}

/// Catches: a host handed to the placer as able to hold a request when the registry would
/// not let it (tainted, leaving, lost or cordoned), or the reasons read in another order.
/// Each host stands as the registry stands it, in host order, and only the live idle one
/// is open.
#[test]
fn the_round_stands_each_host_as_the_registry_does() {
    let mut rig = Rig::new(&["a", "b", "c", "d", "e"]);
    let mut tainted = rig.message("b");
    tainted.taint = Some(Taint {
        reason: "check".into(),
        run_id: None,
        check: None,
    });
    rig.send(&tainted);
    rig.registry.retire(&host("c")).unwrap();
    rig.cordon("d");
    // Every host but `e` is heard from again just before `e` has been silent for the grace.
    rig.step(DEFAULT_GRACE_MS - 1_000);
    for name in ["a", "b", "c", "d"] {
        rig.send(&rig.message(name));
    }
    rig.step(6_000);
    rig.registry.advance();
    rig.joined.remove("e");
    let standing = [
        ("a", Standing::Live),
        ("b", Standing::Tainted),
        ("c", Standing::Leaving),
        ("d", Standing::Cordoned),
        ("e", Standing::Lost),
    ];
    let expected: Vec<WholeHost> = standing
        .iter()
        .map(|(name, standing)| WholeHost {
            id: host(name),
            standing: *standing,
            // Only the live host, which carries nothing, would take a whole-node lease.
            open: *standing == Standing::Live,
        })
        .collect();
    assert_eq!(rig.queue.last().hosts, expected);
}

/// Catches: the lives of the booked leases not handed over (the engine's drain cost would
/// read none), or a lease with no keyed run given one. `a`'s lease has a run of a profile
/// the table gives eight minutes and a run of one it does not know; `b`'s has no keyed
/// run.
#[test]
fn the_round_carries_the_expected_life_of_each_booked_lease() {
    let mut rig = Rig::new(&["a", "b"]);
    let table = BTreeMap::from([("slow", 8), ("fast", 1)]);
    rig.registry.set_lives(Arc::new(Table(table)));
    rig.provider.start(World::share_spec(1, 900));
    rig.provider.start(World::share_spec(2, 900));
    for (run, key) in [("r1", "slow"), ("r2", "unknown"), ("r3", "fast")] {
        let key = ProfileKey::parse(key).unwrap();
        rig.registry
            .queue_keyed_run(&host("a"), &lease(1), job(run), Some(key))
            .unwrap();
    }
    rig.registry.advance();
    assert_eq!(rig.queue.last().lives, BTreeMap::from([(lease(1), 8)]));
}

/// Catches: a placer that does not time a prepare (`prepare_overdue` answers `None`) left
/// without the registry's own deadline: a lease committed from the placer's hold on `b` and
/// reported ready, whose agent then says hello again, fails at the registry's
/// `prepare_timeout` counted from that hello, not from the commit.
#[test]
fn a_placer_that_does_not_time_a_prepare_leaves_the_deadline_to_the_registry() {
    let mut rig = Rig::new(&["a", "b"]);
    *rig.queue.held.lock().unwrap() = vec![mark("r", "b", rig.now().millis(), true)];
    assert_eq!(rig.ask(3, "r"), StartAck::Unknown);
    assert_eq!(rig.lease_on("b"), Some(lease(3)));
    let mut ready = rig.message("b");
    ready.exclusive_ready = Some(lease(3).to_string());
    rig.send(&ready);
    // The agent falls silent past the grace and says hello again: its preparation is void.
    rig.step(DEFAULT_GRACE_MS + 5_000);
    rig.join("b");
    rig.step(DEFAULT_PREPARE_TIMEOUT_MS - 1);
    rig.registry.advance();
    assert_eq!(rig.lease_on("b"), Some(lease(3)));
    rig.step(1);
    rig.registry.advance();
    assert_eq!(rig.lease_on("b"), None);
}
