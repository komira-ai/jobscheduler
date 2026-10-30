//! Sim ab: determinism of the placement score. The same state and request give the same
//! worker in any order the workers register in and after a scheduler restart (state rebuilt
//! from the persisted cost records); equal workers are told apart by the hash of the lease
//! and the worker, never by their order; and the score's integer arithmetic gives the same
//! figures on a 16-thread and an 88-thread worker, at the tolerance edge, at truncated
//! lives and at the extremes of `u64`.
//!
//! `kbf` reaches this scoring through `js-sched` once `kbf-sched` becomes a pure
//! re-export of `js-sched`, so there is no copy to keep identical and nothing to compare
//! it against here.
//!
//! Exercises: the tie-breaks; the integer arithmetic.

use std::collections::BTreeMap;

use js_caps::NodeCaps;
use js_sched::{
    Charge, DaemonInstance, EVALUATIONS_PER_WORKER, Event, HORIZON, HostFacts, Input, LeaseFacts,
    LoadConfig, PredictedLoad, Prediction, Recovered, RecoveredLease, SLOTS, Scheduler,
    ScoreConfig, Scorer,
};
use js_types::sched::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId, Qos, Resources,
    StateMachine, WaiterId, WorkKey, WorkerId,
};

const GIB: u64 = 1 << 30;
const TIB: u64 = 1 << 40;

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn names(list: &[&str]) -> Vec<WorkerId> {
    list.iter().map(|n| worker(n)).collect()
}

// ---- the hash, written out again from the encoding the design states ----

/// 64-bit FNV-1a over `len as u32 LE, bytes` for each part.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in (part.len() as u32).to_le_bytes().iter().chain(*part) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// The same fold without the length prefixes, to show what the prefixes are for.
fn fnv1a_unprefixed(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The tie-break hash of a lease and a worker: the lease as `term-seq` text.
fn tie_hash(lease: LeaseId, worker: &str) -> u64 {
    let lease = format!("{}-{}", lease.term, lease.seq);
    fnv1a(&[lease.as_bytes(), worker.as_bytes()])
}

/// The worker of `candidates` with the smallest hash for `lease`.
fn smallest_hash<'a>(lease: LeaseId, candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .min_by_key(|name| tie_hash(lease, name))
        .copied()
        .unwrap()
}

// ---- a fake predicted load: the scorer's own seam ----

#[derive(Clone)]
struct Host {
    facts: HostFacts,
    load: [Charge; SLOTS],
    new: [Charge; SLOTS],
}

impl Host {
    /// A worker of `cpu` millicores and `memory` bytes carrying nothing and asked for
    /// nothing, with a life of one minute.
    fn empty(cpu: u64, memory: u64) -> Self {
        Self {
            facts: HostFacts {
                cpu_capacity_millis: cpu,
                memory_capacity_bytes: memory,
                fit_minutes: 1,
                fallback: false,
                measured_above_predicted: false,
                same_key_millis: 0,
                new_key_millis: 0,
                uncovered_leases: 0,
            },
            load: [Charge::default(); SLOTS],
            new: [Charge::default(); SLOTS],
        }
    }

    /// CPU already carried at every slot, then `f(slot)` replacing it where given.
    fn cpu_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    /// The new lease's CPU charge.
    fn cpu_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.cpu_millis = f(slot);
        }
        self
    }

    fn mem_load(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.load.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    fn mem_new(mut self, f: impl Fn(usize) -> u64) -> Self {
        for (slot, c) in self.new.iter_mut().enumerate() {
            c.memory_bytes = f(slot);
        }
        self
    }

    fn life(mut self, minutes: u8) -> Self {
        self.facts.fit_minutes = minutes;
        self
    }
}

struct Fake {
    lease: LeaseFacts,
    hosts: BTreeMap<WorkerId, Host>,
}

impl Fake {
    fn new(lease: LeaseId, hosts: Vec<(&str, Host)>) -> Self {
        Self {
            lease: LeaseFacts {
                lease,
                cold: false,
                retry: false,
            },
            hosts: hosts.into_iter().map(|(n, h)| (worker(n), h)).collect(),
        }
    }
}

impl PredictedLoad for Fake {
    fn lease(&self) -> LeaseFacts {
        self.lease
    }

    fn load(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].load[slot]
    }

    fn charge(&self, worker: &WorkerId, slot: usize) -> Charge {
        self.hosts[worker].new[slot]
    }

    fn host(&self, worker: &WorkerId) -> HostFacts {
        self.hosts[worker].facts
    }
}

fn pick(fake: &Fake, order: &[&str]) -> String {
    let candidates = names(order);
    Scorer::default()
        .choose(fake, &candidates)
        .expect("a candidate")
        .as_str()
        .to_owned()
}

const PERMUTATIONS: [[usize; 4]; 6] = [
    [0, 1, 2, 3],
    [3, 2, 1, 0],
    [2, 0, 3, 1],
    [1, 3, 0, 2],
    [0, 2, 1, 3],
    [3, 0, 2, 1],
];

fn permuted<'a>(list: &[&'a str; 4], p: &[usize; 4]) -> Vec<&'a str> {
    p.iter().map(|&i| list[i]).collect()
}

// ---- golden vectors ----

/// Vector: a full tie. Four workers with the same capacity and the same load: keys 1 to 5
/// and the first three tie-breaks are equal, so only the hash tells them apart. The worker
/// is the one with the smallest hash of lease and worker, in each of six orders, for 24
/// leases, the choice reports a tie-break, and four workers cost four times 64
/// evaluations.
#[test]
fn a_full_tie_goes_to_the_smallest_hash_in_any_order() {
    let host = || {
        Host::empty(88_000, 512 * GIB)
            .cpu_load(|_| 30_000)
            .cpu_new(|_| 8_000)
    };
    let all = ["w1", "w2", "w3", "w4"];
    let mut chosen = std::collections::BTreeSet::new();
    for seq in 1..=24 {
        let lease = LeaseId::new(1, seq);
        let fake = Fake::new(lease, all.iter().map(|n| (*n, host())).collect());
        let want = smallest_hash(lease, &all);
        chosen.insert(want);
        for p in &PERMUTATIONS {
            let order = permuted(&all, p);
            let candidates = names(&order);
            let refs: Vec<&WorkerId> = candidates.iter().collect();
            let mut scorer = Scorer::default();
            let choice = scorer.choose_among(&fake, &refs).unwrap();
            assert_eq!(choice.worker.as_str(), want, "lease {seq}, order {order:?}");
            assert!(choice.scored && choice.tie_break);
            assert_eq!(scorer.evaluations(), 4 * EVALUATIONS_PER_WORKER);
        }
    }
    assert!(chosen.len() > 1, "equal workers are spread by lease");
}

/// Vector: the hash encoding. The lease is hashed as `term-seq` text and the worker as its
/// name, each behind its length as a `u32` in little-endian bytes. The pinned figure for
/// lease 1-2 and worker `w1` is checked, `(ab, c)` and `(a, bc)` hash differently while
/// the same bytes without the prefixes hash alike, and the scorer's tie-break follows the
/// prefixed hash: for 240 leases and pairs of workers whose names run into the lease's
/// text, the worker it picks is the one with the smaller prefixed hash, and for some of
/// those pairs the unprefixed hash would have picked the other.
#[test]
fn the_hash_encoding_is_length_prefixed() {
    assert_eq!(tie_hash(LeaseId::new(1, 2), "w1"), 0x922e_fe9b_031d_ce50);
    assert_ne!(fnv1a(&[b"ab", b"c"]), fnv1a(&[b"a", b"bc"]));
    assert_eq!(
        fnv1a_unprefixed(&[b"ab", b"c"]),
        fnv1a_unprefixed(&[b"a", b"bc"])
    );
    // `1-23` + `4` and `1-2` + `34` are the same bytes.
    assert_ne!(
        tie_hash(LeaseId::new(1, 23), "4"),
        tie_hash(LeaseId::new(1, 2), "34")
    );
    let pairs = [
        ("4", "34"),
        ("3", "34"),
        ("-3", "3"),
        ("5", "-5"),
        ("x", "xy"),
    ];
    let (mut decided, mut differ) = (0, 0);
    for term in 1..=4 {
        for seq in 1..=60 {
            let lease = LeaseId::new(term, seq);
            let text = format!("{term}-{seq}");
            for (a, b) in pairs {
                let fake = Fake::new(
                    lease,
                    vec![
                        (a, Host::empty(8_000, 32 * GIB)),
                        (b, Host::empty(8_000, 32 * GIB)),
                    ],
                );
                let want = smallest_hash(lease, &[a, b]);
                assert_eq!(pick(&fake, &[a, b]), want, "{text} {a} {b}");
                assert_eq!(pick(&fake, &[b, a]), want, "{text} {b} {a}");
                let naive = |n: &str| fnv1a_unprefixed(&[text.as_bytes(), n.as_bytes()]);
                decided += 1;
                differ +=
                    usize::from((naive(a) < naive(b)) != (tie_hash(lease, a) < tie_hash(lease, b)));
            }
        }
    }
    assert_eq!(decided, 1_200);
    assert!(
        differ > 100,
        "the sweep tells the encodings apart: {differ}"
    );
}

/// Vector: the slack zone at the tolerance. Two 88-thread workers; `a` carries enough
/// that the new lease of 10 cores leaves it over its capacity by 880 millicores, `b` is
/// empty. 880 millicores is exactly the default tolerance (10,000 parts per million of
/// 88,000): ignored, so the fit decides and `a` wins. One millicore more is 10,011 parts
/// per million (the division rounds down): above it, key 1 is positive for `a`, and `b`
/// wins. With the tolerance given as 20,000 the 881 case goes to `a` again.
#[test]
fn overshoot_at_the_tolerance_is_ignored_and_one_millicore_more_is_not() {
    let hosts = |over: u64| {
        vec![
            (
                "a",
                Host::empty(88_000, 512 * GIB)
                    .cpu_load(move |_| 88_000 + over - 10_000)
                    .cpu_new(|_| 10_000),
            ),
            ("b", Host::empty(88_000, 512 * GIB).cpu_new(|_| 10_000)),
        ]
    };
    for order in [["a", "b"], ["b", "a"]] {
        let below = Fake::new(LeaseId::new(1, 2), hosts(880));
        assert_eq!(pick(&below, &order), "a", "at the tolerance");
        let above = Fake::new(LeaseId::new(1, 2), hosts(881));
        assert_eq!(pick(&above, &order), "b", "one millicore past it");
    }
    let wide = ScoreConfig::new(1, 20_000, 250_000).unwrap();
    let above = Fake::new(LeaseId::new(1, 2), hosts(881));
    let candidates = names(&["a", "b"]);
    assert_eq!(
        Scorer::new(wide)
            .choose(&above, &candidates)
            .unwrap()
            .as_str(),
        "a"
    );
    // The same overshoot is slack: a worker over its capacity at a far minute only.
    let far = vec![
        (
            "a",
            Host::empty(88_000, 512 * GIB)
                .cpu_load(|slot| if slot == 25 { 88_881 - 10_000 } else { 0 })
                .cpu_new(|_| 10_000),
        ),
        ("b", Host::empty(88_000, 512 * GIB).cpu_new(|_| 10_000)),
    ];
    assert_eq!(pick(&Fake::new(LeaseId::new(1, 2), far), &["a", "b"]), "b");
}

/// Vector: a 16-thread worker against an 88-thread worker. Residuals are parts per million
/// of each worker's own capacity: with the same share left (a leftover of 37.5% of the
/// cores, 6,000 millicores of one and 33,000 of the other, and memory in the same
/// proportion) the keys tie, the larger worker
/// takes the lease for every one of 40 leases (the hash never decides), where an absolute
/// leftover would have chosen the smaller. And the same absolute overshoot of 800
/// millicores is 50,000 parts per million of the small worker (slack) and 9,090 of the
/// large (inside the tolerance): the large worker takes it.
#[test]
fn a_small_and_a_large_worker_are_compared_by_share() {
    for seq in 1..=40 {
        let tie = vec![
            (
                "small",
                Host::empty(16_000, 16_000_000)
                    .cpu_load(|_| 8_000)
                    .cpu_new(|_| 2_000)
                    .mem_load(|_| 8_000_000)
                    .mem_new(|_| 2_000_000),
            ),
            (
                "large",
                Host::empty(88_000, 88_000_000)
                    .cpu_load(|_| 44_000)
                    .cpu_new(|_| 11_000)
                    .mem_load(|_| 44_000_000)
                    .mem_new(|_| 11_000_000),
            ),
        ];
        let fake = Fake::new(LeaseId::new(1, seq), tie);
        assert_eq!(pick(&fake, &["small", "large"]), "large", "lease {seq}");
        assert_eq!(pick(&fake, &["large", "small"]), "large", "lease {seq}");
        let over = vec![
            (
                "small",
                Host::empty(16_000, 512 * GIB)
                    .cpu_load(|_| 16_800 - 2_000)
                    .cpu_new(|_| 2_000),
            ),
            (
                "large",
                Host::empty(88_000, 512 * GIB)
                    .cpu_load(|_| 88_800 - 2_000)
                    .cpu_new(|_| 2_000),
            ),
        ];
        let fake = Fake::new(LeaseId::new(1, seq), over);
        assert_eq!(pick(&fake, &["small", "large"]), "large", "lease {seq}");
    }
}

/// Vector: a 1 TiB worker, and the `u64` extremes. On 1,000-core, 1 TiB workers `p` is left
/// 976 millicores and 1 GiB (976 parts per million of each) and `q` twice that: `p` fits
/// tighter. A worker with a capacity of `u64::MAX` of both, and one carrying and asked for
/// `u64::MAX` of both, are scored without overflow, and lose. Four workers cost four times
/// 64 evaluations.
#[test]
fn a_terabyte_worker_and_the_u64_extremes_are_scored_exactly() {
    let fake = Fake::new(
        LeaseId::new(1, 2),
        vec![
            (
                "p",
                Host::empty(1_000_000, TIB)
                    .cpu_load(|_| 1_000_000 - 976 - 10_000)
                    .cpu_new(|_| 10_000)
                    .mem_load(|_| TIB - GIB - GIB / 2)
                    .mem_new(|_| GIB / 2),
            ),
            (
                "q",
                Host::empty(1_000_000, TIB)
                    .cpu_load(|_| 1_000_000 - 1_953 - 10_000)
                    .cpu_new(|_| 10_000)
                    .mem_load(|_| TIB - 2 * GIB - GIB / 2)
                    .mem_new(|_| GIB / 2),
            ),
            ("max", Host::empty(u64::MAX, u64::MAX)),
            (
                "over",
                Host::empty(u64::MAX, u64::MAX)
                    .cpu_load(|_| u64::MAX)
                    .cpu_new(|_| u64::MAX)
                    .mem_load(|_| u64::MAX)
                    .mem_new(|_| u64::MAX),
            ),
        ],
    );
    for p in &PERMUTATIONS {
        let order = permuted(&["p", "q", "max", "over"], p);
        let candidates = names(&order);
        let mut scorer = Scorer::default();
        assert_eq!(scorer.choose(&fake, &candidates).unwrap().as_str(), "p");
        assert_eq!(scorer.evaluations(), 4 * EVALUATIONS_PER_WORKER);
    }
}

/// A worker of 88 cores and 88,000,000 bytes for the new lease of 10 cores and 1,000
/// bytes. Memory is a thousand bytes to the millicore and leaves the same share as the CPU
/// does, so the binding and the stranded leftover are equal and the fit is the CPU's.
fn balanced(load: impl Fn(usize) -> u64 + Copy) -> Host {
    Host::empty(88_000, 88_000_000)
        .cpu_load(load)
        .cpu_new(|_| 10_000)
        .mem_load(move |slot| (load(slot) + 10_000) * 1_000 - 1_000)
        .mem_new(|_| 1_000)
}

/// Vector: `L` truncation. Two workers equal but for the load of `a` at one far minute,
/// which takes `a` to a perfect fit there. The fit looks at the leading `L` minutes
/// only, so: with `L` 3 and a difference at minute 3 they tie and the hash decides (the
/// same worker as for equal workers); with `L` 4 `a` fits better and wins. `L` of 0 is 1,
/// and `L` of 255 is 30: a difference at minute 29 counts at 255 and at 30 and not at 29,
/// and a difference at minute 30 (the `H+` neighbour) and in the `H+` slot counts at none.
#[test]
fn the_fit_is_truncated_to_the_life_and_the_life_is_clamped() {
    let host = |at: usize| balanced(move |slot| if slot == at { 70_000 } else { 40_000 });
    let flat = || balanced(|_| 40_000);
    let lease = LeaseId::new(1, 2);
    let by_hash = smallest_hash(lease, &["a", "b"]);
    let case = |at: usize, life: u8| {
        let fake = Fake::new(
            lease,
            vec![("a", host(at).life(life)), ("b", flat().life(life))],
        );
        pick(&fake, &["a", "b"])
    };
    assert_eq!(
        by_hash, "b",
        "the hash does not choose `a`: a win by `a` is the fit's"
    );
    assert_eq!(case(3, 3), by_hash, "minute 3 is outside L = 3");
    assert_eq!(case(3, 4), "a", "minute 3 is inside L = 4");
    assert_eq!(case(0, 0), "a", "L = 0 is 1: minute 0 is scored");
    assert_eq!(case(1, 0), by_hash, "L = 0 is 1: minute 1 is not");
    assert_eq!(case(29, 255), "a", "L = 255 is 30");
    assert_eq!(case(29, 30), "a");
    assert_eq!(case(29, 29), by_hash);
    assert_eq!(case(HORIZON, 255), by_hash, "minute 30 is never in the fit");
    assert_eq!(
        case(SLOTS - 1, 255),
        by_hash,
        "the H+ slot is never in the fit"
    );
}

/// Vector: a thin key scored flat. A key with too few runs is charged flat at every slot
/// and its life is one minute: two workers equal now but for a lower load later are tied
/// (the hash decides) however long the later difference lasts, and a worker over its
/// capacity at minute 10 still loses to one that is not, because the safety keys look at
/// every slot.
#[test]
fn a_thin_key_is_scored_flat_by_the_fit_and_by_every_slot_for_safety() {
    let lease = LeaseId::new(1, 2);
    let host = |load: fn(usize) -> u64| {
        Host::empty(88_000, 512 * GIB)
            .cpu_load(load)
            .cpu_new(|_| 10_000)
            .life(1)
    };
    let later = |slot: usize| if slot >= 1 { 78_000 } else { 40_000 };
    assert_eq!(
        smallest_hash(lease, &["a", "b"]),
        "b",
        "the hash does not choose `a`"
    );
    let fake = Fake::new(lease, vec![("a", host(later)), ("b", host(|_| 40_000))]);
    assert_eq!(pick(&fake, &["a", "b"]), smallest_hash(lease, &["a", "b"]));
    let hot = |slot: usize| if slot == 10 { 85_000 } else { 40_000 };
    let fake = Fake::new(lease, vec![("a", host(hot)), ("b", host(|_| 40_000))]);
    assert_eq!(pick(&fake, &["a", "b"]), "b");
}

/// Vector: the evaluation count. 64 evaluations per scored worker for each request,
/// counted for every candidate and none for a request with one candidate or none.
#[test]
fn evaluations_are_64_per_scored_worker() {
    assert_eq!(EVALUATIONS_PER_WORKER, 64);
    let fake = Fake::new(
        LeaseId::new(1, 2),
        ["a", "b", "c"]
            .into_iter()
            .map(|n| (n, Host::empty(8_000, 32 * GIB)))
            .collect(),
    );
    let mut scorer = Scorer::default();
    assert!(scorer.choose(&fake, &[]).is_none());
    assert_eq!(scorer.evaluations(), 0);
    let _ = scorer.choose(&fake, &names(&["a"]));
    assert_eq!(scorer.evaluations(), 0);
    let _ = scorer.choose(&fake, &names(&["a", "b"]));
    assert_eq!(scorer.evaluations(), 2 * 64);
    let _ = scorer.choose(&fake, &names(&["c", "a", "b"]));
    assert_eq!(scorer.evaluations(), 5 * 64);
}

// ---- through the scheduler ----

fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"])
}

fn node() -> Resources {
    Resources::new(8_000, 32 * GIB)
}

fn at(ms: u64, event: Event) -> Input {
    Input::new(FarmTime::from_millis(ms), event)
}

fn up(s: &mut Scheduler, name: &str) {
    s.apply(at(
        0,
        Event::WorkerUp {
            worker: worker(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: node(),
            caps: caps(),
        },
    ));
}

fn grants(effects: Vec<Effect>) -> Vec<LeaseGrant> {
    effects
        .into_iter()
        .filter_map(|e| match e {
            Effect::Commit(ControlRecord::Lease(grant)) => Some(grant),
            _ => None,
        })
        .collect()
}

fn submit(s: &mut Scheduler, op: u64, cpu: u64) {
    let request = js_sched::Request {
        key: WorkKey::new("load", format!("{op}")),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(cpu, 4 * GIB),
        needs: js_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
    };
    s.apply(at(
        0,
        Event::Submit {
            waiter: WaiterId(op),
            request,
        },
    ));
    s.set_cost(
        OperationId(op),
        Prediction {
            cpu_millis: cpu,
            memory_bytes: 0,
            cold: false,
            life_minutes: 1,
        },
    );
}

/// A scheduler of term `term` with the workers `order` registered in that order and
/// `loads` one-core leases on each worker named, placed by cordoning the others for the
/// round. The grants it committed, and the next operation id.
fn built(term: u64, order: &[&str], loads: &[(&str, u64)]) -> (Scheduler, Vec<LeaseGrant>, u64) {
    let mut s = Scheduler::new(term).with_load_config(LoadConfig::default());
    for name in order {
        up(&mut s, name);
    }
    let (mut all, mut next) = (Vec::new(), 0);
    for (name, count) in loads {
        for other in order.iter().filter(|o| *o != name) {
            s.apply(at(
                0,
                Event::Cordon {
                    worker: worker(other),
                },
            ));
        }
        for _ in 0..*count {
            submit(&mut s, next, 1_000);
            next += 1;
        }
        let placed = grants(s.apply(at(0, Event::Tick)));
        assert!(placed.iter().all(|g| g.worker.as_str() == *name));
        assert_eq!(placed.len() as u64, *count);
        all.extend(placed);
        for other in order.iter().filter(|o| *o != name) {
            s.apply(at(
                0,
                Event::Uncordon {
                    worker: worker(other),
                },
            ));
        }
    }
    (s, all, next)
}

/// The grant one more one-core action gets.
fn one_more(s: &mut Scheduler, op: u64) -> LeaseGrant {
    submit(s, op, 1_000);
    let mut placed = grants(s.apply(at(0, Event::Tick)));
    assert_eq!(placed.len(), 1);
    placed.pop().unwrap()
}

const ORDERS: [[&str; 4]; 3] = [
    ["w1", "w2", "w3", "w4"],
    ["w4", "w2", "w1", "w3"],
    ["w3", "w4", "w2", "w1"],
];

/// The same state and request, workers registered in three orders: the same worker. Three
/// workers hold 1, 3 and 2 leases and the fourth none; the lease goes to the one that fits
/// it best (`w2`, the fullest), whatever the order.
#[test]
fn the_same_state_and_request_choose_the_same_worker_in_any_order() {
    let loads = [("w1", 1), ("w2", 3), ("w3", 2)];
    let mut chosen = Vec::new();
    for order in &ORDERS {
        let (mut s, _, next) = built(1, order, &loads);
        chosen.push(one_more(&mut s, next).worker);
    }
    assert!(chosen.iter().all(|c| c.as_str() == "w2"), "{chosen:?}");
}

/// A scheduler of term 2 rebuilt from `grants` as the store holds them (costs kept), with
/// the workers registered in `order`.
fn restarted(s: &Scheduler, grants: &[LeaseGrant], order: &[&str]) -> Scheduler {
    let mut b = Scheduler::new(2).with_load_config(LoadConfig::default());
    let operations = grants
        .iter()
        .map(|g| Recovered {
            operation: g.operation,
            request: s.request(g.operation).expect("held").clone(),
            waiter: WaiterId(g.operation.0),
            lease: Some(RecoveredLease {
                lease: g.lease,
                worker: g.worker.clone(),
                cost: Some(g.cost),
            }),
            losses: 0,
            fate: None,
        })
        .collect();
    b.recover(2, OperationId(grants.len() as u64), 0, operations);
    for name in order {
        up(&mut b, name);
    }
    b
}

/// After a restart, with the state rebuilt from the persisted cost records and the
/// workers registering in another order, the same request goes to the same worker, and
/// the rebuilt figures are the originals (each worker's left side, key hash included).
#[test]
fn a_restart_rebuilt_from_the_cost_records_chooses_the_same_worker() {
    let loads = [("w1", 1), ("w2", 3), ("w3", 2)];
    let (original, placed, next) = built(1, &ORDERS[0], &loads);
    let expected = one_more(&mut built(1, &ORDERS[0], &loads).0, next);
    for order in &ORDERS {
        let mut rebuilt = restarted(&original, &placed, order);
        for g in &placed {
            assert_eq!(rebuilt.lease_cost(g.lease), Some(g.cost));
            assert_ne!(g.cost.key_hash, 0);
        }
        let got = one_more(&mut rebuilt, next);
        assert_eq!(got.worker, expected.worker, "order {order:?}");
    }
    assert_eq!(expected.worker.as_str(), "w2");
}

/// Four identical idle workers and a new scheduler for each of 100 rounds (the term, and
/// with it the lease, changes; the workers register in a different order each round): the
/// worker chosen is always the one with the smallest hash of lease and worker, and every
/// worker is chosen at least once.
#[test]
fn identical_idle_workers_are_chosen_by_the_smallest_hash_and_all_get_chosen() {
    let all = ["w1", "w2", "w3", "w4"];
    let mut count: BTreeMap<String, u32> = BTreeMap::new();
    for round in 0..100u64 {
        let order = permuted(&all, &PERMUTATIONS[(round % 6) as usize]);
        let (mut s, _, next) = built(round + 1, &order, &[]);
        let got = one_more(&mut s, next);
        assert_eq!(
            got.worker.as_str(),
            smallest_hash(got.lease, &all),
            "round {round}, lease {:?}",
            got.lease
        );
        *count.entry(got.worker.as_str().to_owned()).or_default() += 1;
    }
    assert_eq!(count.len(), 4, "every worker is chosen: {count:?}");
    assert_eq!(count.values().sum::<u32>(), 100);
}
