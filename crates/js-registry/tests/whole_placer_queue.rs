//! A placer that keeps the whole-node queue (`Placer::whole_round`, the engine of
//! `--placement js-sched`): the registry hands it the wait list and its view of the hosts
//! at every pass, shows the marks it answers as its reservations in place of its own
//! pass's, and commits a request on the host set aside for it. The registry is driven on a
//! manual clock; the placer here answers marks from a script and records every round.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::DEFAULT_GRACE_MS;
use js_lease::{ExclusionEnd, Held, Queued, ReservationRecord, Timestamp};
use js_provider::{Candidate, StartAck, StartSpec};
use js_registry::PoolConfig;
use js_registry::{
    Exclusion, MarkedTails, Placer, ProfileLives, Reservation, Restored, Standing, WaitingWhole,
    WholeHost, WholeNodeConfig, WholeRound,
};
use js_types::{ProviderId, RequestId};
use js_usage::ProfileKey;
use support::world::*;
use support::*;

/// A placer that keeps the queue: it books the first host it is shown, answers each
/// presented request the mark the script holds for it, and records every round.
#[derive(Default)]
struct Queue {
    script: Mutex<BTreeMap<RequestId, Reservation>>,
    rounds: Mutex<Vec<WholeRound>>,
}

impl Queue {
    /// Answers `mark` for its request from now on.
    fn answer(&self, mark: Reservation) {
        self.script
            .lock()
            .unwrap()
            .insert(mark.request.clone(), mark);
    }

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
        let script = self.script.lock().unwrap();
        let marks = round.waiting.iter().filter_map(|w| script.get(&w.id));
        Some(marks.cloned().collect())
    }
}

/// A fixed table of P90 minutes by profile name.
struct Table(BTreeMap<&'static str, u32>);

impl ProfileLives for Table {
    fn wall_minutes_p90(&self, key: &ProfileKey) -> Option<u32> {
        self.0.get(key.as_str()).copied()
    }
}

/// Cordons `name` for ten minutes: the registry books nothing on it until then.
fn cordon(w: &World, name: &str) {
    let ten = Duration::from_secs(600);
    w.fx.registry.cordon(&host(name), ten).unwrap();
}

/// `hosts` with the default whole-node settings and the queue placer set.
fn world(hosts: &[&str]) -> (World, Arc<Queue>) {
    let w = World::new(hosts, plain);
    let queue = Arc::new(Queue::default());
    w.fx.registry.set_placer(queue.clone());
    (w, queue)
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

/// Catches: the registry running its own pass beside the placer's (it would assign the
/// quiet `b`), the placer's mark not shown, or the placer handed a request in another
/// shape than the registry holds it. `a` is cordoned and the placer marks it for `r`: the
/// registry shows that mark, holds `a` for `r` (whose start books nothing while `a` is
/// cordoned, and waits), and hands the placer `r` with its wall time, first presentation and mark.
#[test]
fn the_registry_shows_the_placers_marks_and_runs_no_pass_of_its_own() {
    let (mut w, queue) = world(&["a", "b"]);
    cordon(&w, "a");
    let since = w.now();
    let drain = mark("r", "a", since.millis(), false);
    queue.answer(drain.clone());
    // It waits: its start answers that the lease is pending.
    assert_eq!(w.ask(2, "r"), StartAck::Unknown);
    assert_eq!(w.reservations(), std::slice::from_ref(&drain));
    let first = WaitingWhole {
        id: id("r"),
        max_wall: Duration::from_secs(600),
        since,
        give_ups: 0,
        exclusions: BTreeMap::new(),
        mark: None,
    };
    let round = queue.last();
    assert_eq!(round.waiting, std::slice::from_ref(&first));
    assert_eq!(round.settings, WholeNodeConfig::dedicated(4));
    assert!(round.outside.is_empty() & round.lives.is_empty());
    w.step(5_000);
    let again = WaitingWhole {
        mark: Some(drain),
        ..first
    };
    assert_eq!(queue.last().waiting, [again]);
    assert_eq!(w.lease_on("b"), None);
}

/// Catches: a request whose start is not booked on the quiet host the placer set aside for
/// it, or a placer left uninformed once the request has left the wait list (it would keep
/// the request queued). The placer assigns the quiet `b` to `r`: its start commits there.
/// The registry then hands the placer an empty wait list at its next pass, and a request
/// cancelled while waiting leaves the wait list and its mark at once.
#[test]
fn a_request_commits_on_the_host_set_aside_and_then_leaves_the_round() {
    let (mut w, queue) = world(&["a", "b"]);
    assert_eq!(started_on(&w.share(1, 900)), Some("a"));
    queue.answer(mark("r", "b", w.now().millis(), true));
    assert_eq!(w.ask(2, "r"), StartAck::Unknown);
    assert_eq!(w.lease_on("b"), Some(lease(2)));
    let asked = queue.count();
    w.fx.registry.advance();
    assert_eq!(queue.count(), asked + 1);
    assert!(queue.last().waiting.is_empty());
    cordon(&w, "a");
    queue.answer(mark("s", "a", w.now().millis(), false));
    w.ask(3, "s");
    let shown = vec![("s".to_string(), "a".to_string())];
    assert_eq!(names(&w.reservations()), shown);
    assert!(w.cancel("s"));
    assert!(w.reservations().is_empty());
    w.fx.registry.advance();
    assert!(queue.last().waiting.is_empty());
}

/// Catches: a host handed to the placer as able to hold a request when the registry would
/// not let it (tainted, leaving, lost or cordoned), or the reasons read in another order.
/// Each host stands as the registry stands it, in host order, and only the live idle one
/// is open.
#[test]
fn the_round_stands_each_host_as_the_registry_does() {
    let name = ProviderId::new("builders").unwrap();
    let config = PoolConfig::new(name, whole(), world_shape(), 5);
    let config = config
        .with_whole_node(WholeNodeConfig::dedicated(5))
        .unwrap();
    let mut w = World::of(config, &["a", "b", "c", "d", "e"]);
    let queue = Arc::new(Queue::default());
    w.fx.registry.set_placer(queue.clone());
    w.taint("b");
    w.fx.registry.retire(&host("c")).unwrap();
    cordon(&w, "d");
    w.silent.insert("e".to_owned());
    w.step(DEFAULT_GRACE_MS + 5_000);
    w.fx.registry.advance();
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
    assert_eq!(queue.last().hosts, expected);
}

/// Catches: the lives of the booked leases not handed over (the engine's drain cost would
/// read none), or a lease with no keyed run given one. `a`'s lease has a run of a profile
/// the table gives eight minutes and a run of one it does not know; `b`'s has no keyed
/// run.
#[test]
fn the_round_carries_the_expected_life_of_each_booked_lease() {
    let (mut w, queue) = world(&["a", "b"]);
    let table = BTreeMap::from([("slow", 8), ("fast", 1)]);
    w.fx.registry.set_lives(Arc::new(Table(table)));
    w.share(1, 900);
    w.share(2, 900);
    for (run, key) in [("r1", "slow"), ("r2", "unknown"), ("r3", "fast")] {
        let key = ProfileKey::parse(key).unwrap();
        w.fx.registry
            .queue_keyed_run(&host("a"), &lease(1), job(run), Some(key))
            .unwrap();
    }
    w.fx.registry.advance();
    assert_eq!(queue.last().lives, BTreeMap::from([(lease(1), 8)]));
}

/// Catches: a mark put back from the ledger dropped by the placer's pass before its
/// request is presented again, or not handed over (another request could take its host);
/// and the request's restored give-ups, exclusions and mark not handed over once it is
/// presented. `q` held `a` with a give-up and `b` left out for life.
#[test]
fn a_restored_mark_is_held_outside_until_its_request_is_presented() {
    let (mut w, queue) = world(&["a", "b"]);
    let at = w.now().millis();
    let record = ReservationRecord {
        request_id: "q".to_string(),
        held: Some(Held {
            host: host("a"),
            reserved_at: Timestamp::from_millis(at),
            deadline: Timestamp::from_millis(at + 3_600_000),
            assigned: false,
            tail_end: Some(at + 600_000),
        }),
        queued: Some(Queued {
            place: 0,
            since: Timestamp::from_millis(at),
        }),
        give_ups: 1,
        prepare_failures: 0,
        exclusions: vec![js_lease::Exclusion {
            host: host("b"),
            until: ExclusionEnd::Forever,
        }],
        claim_by: None,
    };
    let restored = w.fx.registry.restore_reservations(&[record]);
    assert_eq!(
        restored,
        Restored {
            resumed: 1,
            dropped: 0
        }
    );
    let kept = w.reservations();
    w.step(10_000);
    w.fx.registry.advance();
    let round = queue.last();
    assert!(round.waiting.is_empty());
    let left = Some(Duration::from_millis(590_000));
    assert_eq!(round.outside, MarkedTails::from([(host("a"), left)]));
    assert_eq!(w.reservations(), kept);
    w.ask(3, "q");
    let round = queue.last();
    assert!(round.outside.is_empty());
    let presented = &round.waiting[0];
    assert_eq!(presented.give_ups, 1);
    let excluded = BTreeMap::from([(host("b"), Exclusion::ForLife)]);
    assert_eq!(presented.exclusions, excluded);
    assert_eq!(
        presented.mark.as_ref().map(|m| m.host.clone()),
        Some(host("a"))
    );
    assert!(w.reservations().is_empty());
}
