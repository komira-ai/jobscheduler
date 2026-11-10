//! The failover family: a pool that places over an ordered list of machine providers
//! (design 4.2 and 5.6), a static provider of registered hosts first, then an EC2-like
//! cloud, then a Lambda-like one, under capacity and quota refusals, lost answers, killed
//! agents and a partition. Each scenario runs over [`SEEDS`] seeds under every invariant of
//! `Invariant::ALL`, the four of the walk among them (`provider-order`,
//! `no-second-launch-for-one-acquire`, `quota-pass-over-honoured`,
//! `free-host-beats-acquire`); the scenarios are built in `cell/failover.rs`, where the
//! headline's numbers are named, and the headline's first seed is pinned in the golden
//! file (`tests/replay.rs`, family `failover`). Two negative cases hold the walk's
//! invariants to a hand-built record of the recording wrappers, with no server.
//!
//! Sizes, in agent requests a seed, each at most 30,000: the headline about 29,400 (see
//! `cell/failover.rs`); the static pool's 60 hosts beating every 5 s for 910 s, about
//! 11,000; and each of the others at most four machines beating for about 700 s, at most
//! about 600.

#[path = "cell/failover.rs"]
mod failover;

use js_sim_cell::{
    AcquireAnswer, AcquireSeen, Checker, CloudFacts, EarlierSeen, Invariant, Observations,
    Scenario, Touched,
};
use js_types::sched::FarmTime;

/// How many seeds each scenario runs.
const SEEDS: u64 = 16;

/// How many threads the headline's seeds run on, a quarter each: it is the family's
/// largest scenario by far.
const THREADS: u64 = 4;

/// The headline (no row of section 8; design 5.6 steps 4 and 5, and section 7, "Scaler
/// double launch"): 60 registered hosts, an EC2-like provider of at most 70 machines and a
/// Lambda-like one of at most 70, 200 hosts in all, run 400 jobs of 300 s; at 90 s ten
/// agents running a job die (their hosts start empty processes 20 s later) and 20 agents
/// are cut off for 30 s. Every job reaches a terminal answer; every invariant holds, so no
/// cloud runs more than its share of machines (`instances-within-limits`), nothing is
/// double-booked, a provider is asked only when the static hosts were busy and every
/// provider before it could not serve, and no acquire id has two machines. Red for a walk
/// that skips a provider, asks a cloud while a registered host is free, or launches past a
/// share. Its seeds run on [`THREADS`] threads.
#[test]
fn two_hundred_hosts_three_providers_kill_ten_agents_and_partition_thirty_seconds() {
    on_threads(failover::headline);
}

/// Design 5.6, pass 1 before pass 2 (no row of section 8): with 40 jobs for 60 registered
/// hosts, every job is placed on a registered host and neither cloud's recording wrapper
/// holds an acquire (`acquires`). Red for a scaler that launches for a job a free
/// registered host could take (`free-host-beats-acquire`).
#[test]
fn the_static_pool_fills_before_any_cloud_is_asked() {
    failover::static_first().run_seeds(SEEDS);
}

/// Design 5.6, step 5 (no row of section 8): the EC2-like cloud refuses every acquire for
/// capacity, every offer and zone of it, and the Lambda-like one launches the four
/// machines; each Lambda-like acquire comes after the EC2-like cloud refused every offer
/// (`provider-order`). Witness `acquire_refused`. Red for a walk that stops at the first
/// provider's refusal, and for one that asks the second before the first's last offer.
#[test]
fn a_capacity_refusal_falls_through_to_the_next_provider() {
    failover::capacity_falls_through().run_seeds(SEEDS);
}

/// Design 5.6, step 5, "quota" (no row of section 8): the EC2-like cloud refuses its
/// cheapest instance type in one zone for quota; the three machines are launched by its
/// other offers, the Lambda-like cloud is never asked, and that instance type in that zone
/// is not asked again within its `quota_backoff` (`quota-pass-over-honoured`). Witness
/// `acquire_refused`. Red for a refusal that passes over the whole provider, or none of it.
#[test]
fn a_quota_refusal_passes_over_that_instance_type_only() {
    failover::quota_passes_over().run_seeds(SEEDS);
}

/// Covers section 8, row 34, and section 7, "Provider says unknown": the EC2-like cloud's
/// first answer is lost but the call landed; exactly one machine exists for the job, it
/// joins and runs the job, and the Lambda-like cloud is never asked. The acquire id is
/// never presented to the second cloud and has one machine
/// (`no-second-launch-for-one-acquire`). Witness `acquire_lost`. Red for a scaler that
/// fails over on an unknown answer, or launches again under a new id.
#[test]
fn an_unknown_answer_from_the_first_cloud_does_not_try_the_second() {
    failover::unknown_stays().run_seeds(SEEDS);
}

/// Design 5.6, step 5, `acquire_retry` (no row of section 8): both clouds refuse every
/// acquire for capacity until the EC2-like one takes its next acquire at 180 s; the job's
/// machine is launched within two retry intervals of that, and each cloud was asked at most
/// once an offer an attempt, with at most one attempt a retry interval, counted from the
/// scenario's virtual times (`acquires-at-most`). Witness `acquire_refused`. Red for a
/// request that is never asked again, and for one asked again without waiting.
#[test]
fn a_cloud_that_recovers_is_asked_again_on_the_retry_interval() {
    failover::recovers_on_retry().run_seeds(SEEDS);
}

/// Section 7, "Scaler double launch", for a preemption (no row of section 8): the
/// EC2-like cloud refuses every acquire, so the Lambda-like one, of two machines, serves;
/// at 200 s it preempts its first machine, which runs the first job, and at 300 s a second
/// job is asked for: a second Lambda-like machine replaces the first for it and the job
/// succeeds, with no job on two hosts, no host booked twice and no acquire id with two
/// machines. How the first job ends is not asserted (its client gives up at the latest):
/// the merged server holds the release of a machine whose host holds a job allocation, and
/// ends that job only on the host's word or its absence. Witness `machine_preempted`. Red
/// for a preempted machine whose share is never given back, and for a replacement launched
/// under the preempted machine's acquire id.
#[test]
fn a_preempted_machine_in_the_second_cloud_is_replaced_without_double_booking() {
    failover::preempted_in_the_second().run_seeds(SEEDS);
}

/// The negative case of `provider-order` over a hand-built record: the Lambda-like cloud
/// was asked at 200 while the EC2-like one held none of its 70 machines and had never
/// refused its offer. The checker flags exactly that invariant. Catches a check that cannot
/// fail: a walk that skips the first provider would pass every scenario above.
#[test]
fn a_walk_that_skips_the_first_provider_fails_provider_order() {
    let earlier = EarlierSeen {
        provider: failover::EC2.to_owned(),
        held: 0,
        max: failover::EC2_MACHINES,
        open: vec![(
            failover::EC2_SMALL.to_owned(),
            Some(failover::ZONE_A.to_owned()),
        )],
    };
    let mut asked = seen(200, "burst-1", AcquireAnswer::Unknown);
    asked.earlier = vec![earlier];
    let observations = Observations::quiet()
        .at(FarmTime::from_millis(300))
        .with_cloud(first())
        .with_cloud(second().with_seen(asked));
    assert_eq!(flagged(&observations), [Invariant::ProviderOrder]);
}

/// The negative case of `no-second-launch-for-one-acquire` over a hand-built record: the
/// EC2-like cloud answered the acquire id `burst-1` with no answer at 100, and the
/// Lambda-like cloud, its first provider at its share, was asked under the same id at 200
/// and launched a machine. The checker flags exactly that invariant. Catches a check that
/// cannot fail: a scaler that launches elsewhere after an unknown answer.
#[test]
fn a_launch_after_unknown_fails_no_second_launch_for_one_acquire() {
    let full = EarlierSeen {
        provider: failover::EC2.to_owned(),
        held: failover::EC2_MACHINES,
        max: failover::EC2_MACHINES,
        open: Vec::new(),
    };
    let landed = AcquireAnswer::Accepted {
        machine: "fake-0001".to_owned(),
    };
    let mut again = seen(200, "burst-1", landed);
    again.earlier = vec![full];
    let observations = Observations::quiet()
        .at(FarmTime::from_millis(300))
        .with_cloud(first().with_seen(seen(100, "burst-1", AcquireAnswer::Unknown)))
        .with_cloud(second().with_seen(again));
    assert_eq!(
        flagged(&observations),
        [Invariant::NoSecondLaunchForOneAcquire]
    );
}

/// The EC2-like cloud of the negative cases, the first launching provider.
fn first() -> CloudFacts {
    CloudFacts::new(failover::EC2, failover::EC2_MACHINES)
        .ranked(0, failover::QUOTA_BACKOFF)
        .with_offer(failover::EC2_SMALL, Some(failover::ZONE_A))
}

/// The Lambda-like cloud of the negative cases, the second.
fn second() -> CloudFacts {
    CloudFacts::new(failover::LAMBDA, failover::LAMBDA_MACHINES)
        .ranked(1, failover::QUOTA_BACKOFF)
        .with_offer(failover::LAMBDA_CPU, Some("zone-l"))
}

/// An acquire at `at` ms for the acquire id `acquire`, answered `answer`, with no host open
/// and no provider before it.
fn seen(at: u64, acquire: &str, answer: AcquireAnswer) -> AcquireSeen {
    AcquireSeen {
        at: FarmTime::from_millis(at),
        offer: failover::EC2_SMALL.to_owned(),
        zone: Some(failover::ZONE_A.to_owned()),
        acquire: acquire.to_owned(),
        answer,
        open_hosts: Vec::new(),
        earlier: Vec::new(),
    }
}

/// The invariants one step of a fresh checker flags over `observations`, each once, in
/// order.
fn flagged(observations: &Observations) -> Vec<Invariant> {
    let found = Checker::new().step(observations, &Touched::everything_in(observations));
    let mut flagged: Vec<Invariant> = found.iter().map(|v| v.invariant).collect();
    flagged.dedup();
    flagged
}

/// Runs the seeds of the scenario `make` builds on [`THREADS`] threads, each building its
/// own scenario over its share of the seeds, and fails with the first failure.
fn on_threads(make: fn() -> Scenario) {
    let parts: Vec<_> = (0..THREADS)
        .map(|part| {
            let seeds = part * SEEDS / THREADS..(part + 1) * SEEDS / THREADS;
            std::thread::spawn(move || {
                let ran = make().try_run_seed_range(seeds);
                ran.map(|_| ()).map_err(|failure| failure.to_string())
            })
        })
        .collect();
    for part in parts {
        match part.join() {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => panic!("{failure}"),
            Err(panicked) => std::panic::resume_unwind(panicked),
        }
    }
}
