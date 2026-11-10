//! The walk over the queued jobs with the scheduler's platform matches held for the pass: it
//! places as the walk that matches every open host again for each job, over generated hosts,
//! leases, ceilings and jobs, and it matches each open host once per platform in a pass.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_wire::{NetworkCeiling, PortRange};
use js_jobs::{DeclaredPort, Isolation, JobId, JobRecord, Needs, NetworkPolicy, NewJob};
use js_types::sched::WorkerId;
use js_types::{Candidate, Held, HostId, JobNetwork, LeaseId, Resources, Shape};

use super::{Decided, OpenHost};
use crate::engine::Engine;
use crate::engine::rig::{Tables, at, engine_over, host, spec_of};

const GIB: u64 = 1 << 30;

/// A generator of test states: xorshift64, seeded.
struct Gen(u64);

impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    /// One of `items`.
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let len = u64::try_from(items.len()).unwrap();
        &items[usize::try_from(self.below(len)).unwrap()]
    }

    /// True one time in `n`.
    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

const ARCHES: [&str; 2] = ["x86_64", "aarch64"];
const OSES: [&str; 2] = ["linux", "macos"];
const LEVELS: [JobNetwork; 3] = [JobNetwork::None, JobNetwork::AllowList, JobNetwork::Open];
const POLICIES: [NetworkPolicy; 3] = [
    NetworkPolicy::None,
    NetworkPolicy::AllowList,
    NetworkPolicy::Open,
];
const PORTS: [u16; 3] = [8000, 8001, 9000];

/// A host named `name` of `cores` cores and `gib` GiB that reports `arch` and `os`, carrying
/// the one-core allocations of the jobs `held-<seq>` for each of `seqs`.
fn candidate(
    name: &str,
    (cores, gib): (u64, u64),
    (arch, os): (&str, &str),
    seqs: &[u64],
) -> Candidate {
    let caps = [("arch", arch), ("os", os)];
    let caps = caps.map(|(key, value)| (key.to_owned(), value.to_owned()));
    let shape = Shape::new(
        Resources::new(cores * 1000, gib * GIB, 0, None).unwrap(),
        caps.into_iter().collect(),
    );
    let leases = seqs.iter().map(|seq| Held {
        lease: LeaseId::new(1, *seq),
        resources: Resources::new(1000, GIB, 0, None).unwrap(),
        whole: false,
    });
    Candidate {
        host: host(name),
        shape,
        leases: leases.collect(),
    }
}

/// Up to six hosts of several sizes and platforms, busy with up to three allocations each,
/// and the hosts open to jobs among them (most), each with a ceiling and host jobs or not.
fn hosts(g: &mut Gen) -> (Vec<Candidate>, BTreeMap<HostId, OpenHost>) {
    let mut seq = 0;
    let mut built = Vec::new();
    let mut open = BTreeMap::new();
    for n in 0..=g.below(6) {
        let name = format!("h{n}");
        let size = (2 + g.below(15), 4 + g.below(61));
        let platform = (*g.pick(&ARCHES), *g.pick(&OSES));
        let leases = g.below(4);
        let seqs: Vec<u64> = (seq + 1..=seq + leases).collect();
        seq += leases;
        built.push(candidate(&name, size, platform, &seqs));
        if !g.one_in(5) {
            let ports = (!g.one_in(2)).then(|| PortRange::new(8000, 8010).unwrap());
            let offer = OpenHost {
                ceiling: NetworkCeiling {
                    level: match g.one_in(2) {
                        true => JobNetwork::Open,
                        false => *g.pick(&LEVELS),
                    },
                    ports: ports.into_iter().collect(),
                },
                host_jobs: g.one_in(2),
                rank: None,
            };
            open.insert(host(&name), offer);
        }
    }
    (built, open)
}

/// Up to twelve queued jobs named `<prefix><n>` in `tables`: of several sizes, some naming an
/// architecture or an operating system, some with host isolation, a network policy and a host
/// port, and now and then one that needs the whole host (which the walk leaves out).
fn jobs(g: &mut Gen, tables: &Tables, prefix: &str) -> Vec<JobRecord> {
    (0..g.below(13))
        .map(|n| {
            let resources =
                Resources::new((1 + g.below(6)) * 1000, (1 + g.below(12)) * GIB, 0, None);
            let needs = match g.one_in(10) {
                true => Needs::WholeHost,
                false => Needs::Shape(resources.unwrap()),
            };
            let mut spec = spec_of(needs, 60 + g.below(3600));
            let constraints = &mut spec.constraints;
            constraints.arch = g.one_in(3).then(|| (*g.pick(&ARCHES)).to_owned());
            constraints.os = g.one_in(3).then(|| (*g.pick(&OSES)).to_owned());
            if g.one_in(8) {
                constraints.isolation = Isolation::Host;
            }
            spec.network = *g.pick(&POLICIES);
            if g.one_in(3) {
                let port = *g.pick(&PORTS);
                spec.ports = vec![DeclaredPort {
                    host: port,
                    container: port,
                }];
            }
            let new = NewJob {
                id: Some(JobId::parse(&format!("{prefix}{n}")).unwrap()),
                spec,
                priority: 0,
                client: None,
            };
            tables.jobs.insert_queued(new).unwrap().record
        })
        .collect()
}

/// What a walk decided: its placements, and the jobs it placed nowhere with whether some open
/// host could run them.
type Walked = (Vec<Decided>, Vec<(JobId, bool)>);

/// What the engine's scheduler books on each of `hosts`.
fn booked(engine: &Engine, hosts: &[Candidate]) -> Vec<Option<js_types::sched::Resources>> {
    let worker = |candidate: &Candidate| WorkerId::new(candidate.host.as_str());
    hosts
        .iter()
        .map(|c| engine.sched().booked(&worker(c)))
        .collect()
}

/// Catches: a match held over the walk that changes which hosts the scheduler offers a job:
/// a host given a job its platform does not satisfy, or a host left out of a job it could
/// take, so a placement moves, appears or goes, beside a host-isolation job's taint and the
/// host ports already held. Over generated hosts (sizes, platforms, allocations, ceilings,
/// host jobs or not, some not open) and two passes of generated jobs, the second over what the
/// first left queued and more, with the first's placements still held: the walk that holds the
/// matches decides exactly what the walk that matches every host again for each job decides,
/// pass by pass, and books the same on every host after each. Over all the states, jobs are
/// placed (some with host isolation, some with a host port), some wait for room and some can
/// run nowhere.
#[test]
fn the_held_walk_places_as_the_walk_matching_each_round_over_generated_states() {
    let mut seen = Seen::default();
    for seed in 1..=96u64 {
        let mut g = Gen(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (built, open) = hosts(&mut g);
        let tables = Tables::new();
        let first = jobs(&mut g, &tables, "a");
        let later = jobs(&mut g, &tables, "b");
        let mut held = engine_over(&built);
        let mut fresh = engine_over(&built);
        let one: Walked = held.walk_jobs(&first, &open, at(1_000));
        assert_eq!(
            one,
            fresh.walk_jobs_matching_each_round(&first, &open, at(1_000)),
            "seed {seed}, first pass"
        );
        assert_eq!(booked(&held, &built), booked(&fresh, &built), "seed {seed}");
        let placed: Vec<&JobId> = one.0.iter().map(|decided| &decided.0).collect();
        let left = first.iter().filter(|job| !placed.contains(&&job.id));
        let queued: Vec<JobRecord> = left.chain(&later).cloned().collect();
        let two: Walked = held.walk_jobs(&queued, &open, at(2_000));
        assert_eq!(
            two,
            fresh.walk_jobs_matching_each_round(&queued, &open, at(2_000)),
            "seed {seed}, second pass"
        );
        assert_eq!(booked(&held, &built), booked(&fresh, &built), "seed {seed}");
        let all: Vec<&JobRecord> = first.iter().chain(&later).collect();
        seen.count(&one, &all);
        seen.count(&two, &all);
    }
    let Seen {
        placed,
        host_isolated,
        with_port,
        for_room,
        nowhere,
    } = seen;
    assert!(
        placed > 0 && host_isolated > 0 && with_port > 0,
        "{placed} {host_isolated} {with_port}"
    );
    assert!(for_room > 0 && nowhere > 0, "{for_room} {nowhere}");
}

/// What the walks of the generated states decided, counted over them all.
#[derive(Default)]
struct Seen {
    placed: usize,
    host_isolated: usize,
    with_port: usize,
    for_room: usize,
    nowhere: usize,
}

impl Seen {
    /// Counts what `walked` decided for the jobs of `jobs`.
    fn count(&mut self, (placements, waiting): &Walked, jobs: &[&JobRecord]) {
        let job = |id: &JobId| *jobs.iter().find(|job| job.id == *id).unwrap();
        for decided in placements {
            let spec = &job(&decided.0).spec;
            self.placed += 1;
            self.host_isolated += usize::from(spec.constraints.isolation == Isolation::Host);
            self.with_port += usize::from(!spec.ports.is_empty());
        }
        for (_, capable) in waiting {
            self.for_room += usize::from(*capable);
            self.nowhere += usize::from(!*capable);
        }
    }
}

/// Four open Linux x86 hosts of sixteen cores, and ten one-core jobs, the even ones asking for
/// `x86_64` (they are placed) and the odd ones for `aarch64` (no host has it: they wait).
fn four_hosts_ten_jobs() -> (Vec<Candidate>, BTreeMap<HostId, OpenHost>, Vec<JobRecord>) {
    let names = ["a", "b", "c", "d"];
    let platform = ("x86_64", "linux");
    let built: Vec<Candidate> = names
        .iter()
        .map(|name| candidate(name, (16, 64), platform, &[]))
        .collect();
    let open = names
        .iter()
        .map(|name| (host(name), OpenHost::default()))
        .collect();
    let tables = Tables::new();
    let resources = Resources::new(1000, GIB, 0, None).unwrap();
    let queued = (0..10)
        .map(|n| {
            let mut spec = spec_of(Needs::Shape(resources.clone()), 600);
            spec.constraints.arch = Some(ARCHES[n % 2].to_owned());
            let new = NewJob {
                id: Some(JobId::parse(&format!("j{n}")).unwrap()),
                spec,
                priority: 0,
                client: None,
            };
            tables.jobs.insert_queued(new).unwrap().record
        })
        .collect();
    (built, open, queued)
}

/// Catches: the walk matching every open host again for each job of a pass (the matches made
/// per round), and a walk that leaves the scheduler holding its matches after it, so the rounds
/// of later lease starts would keep them too. Four hosts and ten jobs of two platforms: a job
/// no open host can run gets no round, so only the five `x86_64` jobs are matched. The held
/// walk matches each host once for their platform, four in all, where the walk that matches
/// each round matches each host for each of them, twenty; both place the same five jobs. The
/// next pass, over the five that wait (no host can run them), matches nothing. After it, each
/// of two rounds outside a walk matches every host again.
#[test]
fn a_pass_matches_each_open_host_once_per_platform() {
    let (built, open, queued) = four_hosts_ten_jobs();
    let mut held = engine_over(&built);
    let mut fresh = engine_over(&built);
    let before = (
        held.sched().match_evaluations(),
        fresh.sched().match_evaluations(),
    );
    let walked = held.walk_jobs(&queued, &open, at(1_000));
    assert_eq!(
        walked,
        fresh.walk_jobs_matching_each_round(&queued, &open, at(1_000))
    );
    assert_eq!(walked.0.len(), 5);
    assert_eq!(walked.1.len(), 5);
    let held_count = held.sched().match_evaluations() - before.0;
    let fresh_count = fresh.sched().match_evaluations() - before.1;
    assert_eq!((held_count, fresh_count), (4, 20));
    let waiting: Vec<JobRecord> = queued.into_iter().skip(1).step_by(2).collect();
    let next = held.sched().match_evaluations();
    let (placed, still) = held.walk_jobs(&waiting, &open, at(2_000));
    assert!(placed.is_empty());
    assert_eq!(still.len(), 5);
    assert_eq!(held.sched().match_evaluations() - next, 0);
    let resources = Resources::new(1000, GIB, 0, None).unwrap();
    let charge = js_sched::Prediction {
        cpu_millis: 1000,
        memory_bytes: GIB,
        cold: false,
        life_minutes: 1,
    };
    for ms in [3_000, 4_000] {
        let request = super::job_request(&waiting[0], &resources).unwrap();
        let operation = held.sched_mut().next_operation();
        let before = held.sched().match_evaluations();
        let wall = (Duration::from_secs(600), charge);
        let placed = held.place_one((operation, request), wall, BTreeSet::new(), at(ms));
        assert!(placed.is_none());
        let _ = held.sched_mut().forget_queued(operation);
        assert_eq!(
            held.sched().match_evaluations() - before,
            4,
            "the round at {ms}"
        );
    }
}
