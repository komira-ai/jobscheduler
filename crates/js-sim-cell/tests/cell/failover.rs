//! The failover family's scenarios: a pool that places over an ordered list of machine
//! providers (design 4.2 and 5.6), a static provider of registered hosts first, then an
//! EC2-like cloud ([`EC2`]), then a Lambda-like one ([`LAMBDA`]), each over its own elastic
//! fake behind a recording wrapper (`ServerSpec::ordered`). `tests/failover.rs` runs each
//! over sixteen seeds; `tests/replay.rs` pins the headline's first seed in the golden file.
//!
//! Every host is a [`Shape::cpu_host`] (8 cores) and every job asks for all of a host's
//! cores ([`JOB_CPU_MILLIS`]), so each host, registered or launched, runs one job at a time
//! and the queue of jobs past the static hosts is demand on the clouds, one machine a job.
//!
//! The headline's numbers are the constants from [`STATIC_HOSTS`] to [`CUT_FOR`]. An
//! elastic pool's hosts beat at the registry's defaults (5 s, held lost after 15 s of
//! silence), which a pool of the server's configuration file does not set: the 20 hosts
//! cut off for 30 s are held lost, and the jobs they ran end lost. Its size, in agent
//! requests a seed: the 60 static hosts beat every 5 s until [`FAULT_AT`] plus
//! [`CUT_FOR`] plus [`HEADLINE_SETTLE`], 900 s (about 10,800), and the 140 machines from
//! their boot, about 125 s, to their release, about 790 s (about 18,600): about 29,400 with
//! the hellos, inside the cell's 30,000.

use std::collections::BTreeMap;
use std::time::Duration;

use js_elastic_fake::{Call, Step};
use js_sim_cell::actions::{
    drawn_agents, exhaust_provider, kill_agents_mid_job, partition_agents, preempt_machine_of,
    quota_refuse, script_provider_by_name, submit_work,
};
use js_sim_cell::{
    Counter, ElasticPool, Expect, HowEnded, OfferSpec, Scenario, ServerSpec, Shape, Work,
};
use js_types::Resources;

/// The pool's name.
pub const POOL: &str = "burst";

/// The first launching provider, billed as EC2 bills.
pub const EC2: &str = "ec2";

/// The second launching provider, billed as Lambda bills.
pub const LAMBDA: &str = "lambda";

/// The headline's registered hosts, of the static provider.
pub const STATIC_HOSTS: usize = 60;

/// The most machines the EC2-like provider holds for the pool: its share.
pub const EC2_MACHINES: u64 = 70;

/// The most machines the Lambda-like provider holds for the pool: its share.
pub const LAMBDA_MACHINES: u64 = 70;

/// The headline's jobs, all submitted at the start.
pub const JOBS: u32 = 400;

/// How many agents running a job die at [`FAULT_AT`].
pub const KILLED: usize = 10;

/// How many agents are cut off from the server at [`FAULT_AT`].
pub const CUT: usize = 20;

/// How long they are cut off.
pub const CUT_FOR: Duration = Duration::from_secs(30);

/// When the headline's faults begin.
pub const FAULT_AT: Duration = Duration::from_secs(90);

/// How long after its death a killed agent's host starts a new process with empty state.
pub const REPLACE_AFTER: Duration = Duration::from_secs(20);

/// How long each job runs once started.
pub const RUN: Duration = Duration::from_secs(300);

/// Each job's CPU: a whole host's cores.
pub const JOB_CPU_MILLIS: u64 = 8_000;

/// How long after its last fault the headline settles: the two waves of jobs (the second
/// ends about 730 s from the start), the machines' idle time (60 s) and their release.
pub const HEADLINE_SETTLE: Duration = Duration::from_secs(780);

/// How long the smaller scenarios settle after their last action.
pub const SETTLE: Duration = Duration::from_secs(900);

/// The `quota_backoff` of each provider.
pub const QUOTA_BACKOFF: Duration = Duration::from_secs(3_600);

/// The `acquire_retry` of each provider.
pub const ACQUIRE_RETRY: Duration = Duration::from_secs(60);

/// The EC2-like provider's cheapest instance type, offered in [`ZONE_A`] and dearer in
/// [`ZONE_B`].
pub const EC2_SMALL: &str = "ec2_small";

/// The EC2-like provider's dearer instance type, in [`ZONE_A`].
pub const EC2_LARGE: &str = "ec2_large";

/// A zone of the EC2-like provider (each zone is its own region, as the server reads them).
pub const ZONE_A: &str = "zone-a";

/// The other zone of the EC2-like provider.
pub const ZONE_B: &str = "zone-b";

/// How many offers (instance type and zone) the EC2-like provider has.
pub const EC2_OFFERS: usize = 3;

/// The Lambda-like provider's one instance type.
pub const LAMBDA_CPU: &str = "lambda_cpu";

/// How many offers the Lambda-like provider has.
pub const LAMBDA_OFFERS: usize = 1;

/// Every host: 8 cores, 16 GiB, no GPU, and no capability the pool's floor lacks.
pub fn shape() -> Shape {
    Shape {
        capabilities: BTreeMap::new(),
        ..Shape::cpu_host()
    }
}

/// The EC2-like provider's offers, [`EC2_OFFERS`] of them, cheapest first.
pub fn ec2_offers() -> Vec<OfferSpec> {
    vec![
        OfferSpec::new(EC2_SMALL, ZONE_A, 100),
        OfferSpec::new(EC2_SMALL, ZONE_B, 105),
        OfferSpec::new(EC2_LARGE, ZONE_A, 140),
    ]
}

/// The Lambda-like provider's offer.
pub fn lambda_offers() -> Vec<OfferSpec> {
    vec![OfferSpec::new(LAMBDA_CPU, "zone-l", 110)]
}

/// The server: `statics` hosts of the `static` provider admitted, then the EC2-like
/// provider of at most `ec2` machines, then the Lambda-like one of at most `lambda`.
pub fn server(statics: usize, ec2: u64, lambda: u64) -> ServerSpec {
    let failover = |pool: ElasticPool| pool.failover(QUOTA_BACKOFF, ACQUIRE_RETRY);
    let providers = vec![
        failover(ElasticPool::ec2_like(EC2, ec2_offers(), ec2)),
        failover(ElasticPool::lambda_like(LAMBDA, lambda_offers(), lambda)),
    ];
    ServerSpec::ordered(POOL, shape(), statics, providers)
}

/// One job: [`RUN`] long, asking for a whole host's cores.
pub fn job() -> Work {
    let needs = Resources::new(JOB_CPU_MILLIS, 1 << 30, 0, None).unwrap_or_default();
    Work::run_for(RUN).needs(needs)
}

/// `scenario` over [`server`] of `statics` registered hosts, each of which says hello, and
/// the two clouds of `ec2` and `lambda` machines.
fn on(scenario: Scenario, statics: usize, (ec2, lambda): (u64, u64)) -> Scenario {
    scenario
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(server(statics, ec2, lambda))
        .hosts(POOL, statics)
}

/// The headline: [`STATIC_HOSTS`] registered hosts, the EC2-like provider limited to
/// [`EC2_MACHINES`] and the Lambda-like one to [`LAMBDA_MACHINES`]; [`JOBS`] jobs of
/// [`RUN`]; at [`FAULT_AT`] [`KILLED`] agents running a job die, each host starting an
/// empty process [`REPLACE_AFTER`] later, and [`CUT`] agents are cut off for [`CUT_FOR`].
pub fn headline() -> Scenario {
    on(
        Scenario::new(
            "two_hundred_hosts_three_providers_kill_ten_agents_and_partition_thirty_seconds",
        ),
        STATIC_HOSTS,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .work(JOBS, job())
    .at(FAULT_AT, kill_agents_mid_job(KILLED, REPLACE_AFTER))
    .at(FAULT_AT, partition_agents(drawn_agents(CUT), CUT_FOR))
    .expect(Expect::all_items_terminal())
    .settle_within(HEADLINE_SETTLE)
}

/// Demand below the static hosts: [`STATIC_HOSTS`] hosts and fewer jobs, submitted once
/// every host has said hello.
pub const BELOW_STATIC: u32 = 40;

/// When the jobs below the static hosts are submitted: after every host's hello and first
/// heartbeat.
pub const SUBMIT_AT: Duration = Duration::from_secs(10);

/// The jobs below the static hosts' number are all placed there: no cloud is asked.
pub fn static_first() -> Scenario {
    on(
        Scenario::new("the_static_pool_fills_before_any_cloud_is_asked"),
        STATIC_HOSTS,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .at(SUBMIT_AT, submit_work(BELOW_STATIC, job()))
    .expect(Expect::acquires(EC2, 0))
    .expect(Expect::acquires(LAMBDA, 0))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(
        BELOW_STATIC as usize,
        HowEnded::Succeeded,
    ))
    .settle_within(SETTLE)
}

/// The jobs of the smaller cloud scenarios.
pub const FEW: u32 = 4;

/// The EC2-like cloud refuses every acquire for capacity: the Lambda-like one serves.
pub fn capacity_falls_through() -> Scenario {
    on(
        Scenario::new("a_capacity_refusal_falls_through_to_the_next_provider"),
        0,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .at(Duration::ZERO, exhaust_provider(EC2))
    .work(FEW, job())
    .expect(Expect::machines_launched(EC2, 0))
    .expect(Expect::machines_launched(LAMBDA, FEW as usize))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(FEW as usize, HowEnded::Succeeded))
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 1)
    .settle_within(SETTLE)
}

/// The jobs of the quota scenario.
pub const QUOTA_JOBS: u32 = 3;

/// The EC2-like cloud refuses its cheapest offer, [`EC2_SMALL`] in [`ZONE_A`], for quota:
/// its other offers serve, and the Lambda-like one is never asked.
pub fn quota_passes_over() -> Scenario {
    on(
        Scenario::new("a_quota_refusal_passes_over_that_instance_type_only"),
        0,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .at(Duration::ZERO, quota_refuse(EC2, EC2_SMALL, ZONE_A))
    .work(QUOTA_JOBS, job())
    .expect(Expect::machines_launched(EC2, QUOTA_JOBS as usize))
    .expect(Expect::acquires(LAMBDA, 0))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(
        QUOTA_JOBS as usize,
        HowEnded::Succeeded,
    ))
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 1)
    .settle_within(SETTLE)
}

/// The EC2-like cloud's first answer is lost, but the call landed: one machine, which
/// joins, and the Lambda-like cloud is never asked.
pub fn unknown_stays() -> Scenario {
    let lost = [Step::Lost { landed: true }];
    on(
        Scenario::new("an_unknown_answer_from_the_first_cloud_does_not_try_the_second"),
        0,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .at(
        Duration::ZERO,
        script_provider_by_name(EC2, Call::Acquire, &lost),
    )
    .work(1, job())
    .expect(Expect::machines_launched(EC2, 1))
    .expect(Expect::machines_joined(EC2, 1))
    .expect(Expect::machines_launched(LAMBDA, 0))
    .expect(Expect::acquires(LAMBDA, 0))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .witness("acquire_lost", |c| c.get(Counter::AcquireLost) >= 1)
    .settle_within(SETTLE)
}

/// When the EC2-like cloud, which with the Lambda-like one has refused every acquire since
/// the start, accepts its next acquire.
pub const RECOVER_AT: Duration = Duration::from_secs(180);

/// How many attempts at most begin before [`RECOVER_AT`] and in the one wait after it: an
/// attempt asks every offer once and then waits at least [`ACQUIRE_RETRY`] before the next.
pub const ATTEMPTS: usize = (RECOVER_AT.as_secs() / ACQUIRE_RETRY.as_secs()) as usize + 2;

/// Both clouds refuse every acquire for capacity; at [`RECOVER_AT`] the EC2-like one takes
/// the next. The job is launched within two retry intervals, and each cloud was asked at
/// most once an offer an attempt.
pub fn recovers_on_retry() -> Scenario {
    on(
        Scenario::new("a_cloud_that_recovers_is_asked_again_on_the_retry_interval"),
        0,
        (EC2_MACHINES, LAMBDA_MACHINES),
    )
    .at(Duration::ZERO, exhaust_provider(EC2))
    .at(Duration::ZERO, exhaust_provider(LAMBDA))
    .work(1, job())
    .at(
        RECOVER_AT,
        script_provider_by_name(EC2, Call::Acquire, &[Step::Pass]),
    )
    .expect_at(
        RECOVER_AT + 2 * ACQUIRE_RETRY,
        Expect::machines_launched(EC2, 1),
    )
    .expect(Expect::machines_launched(EC2, 1))
    .expect(Expect::machines_launched(LAMBDA, 0))
    .expect(Expect::acquires_at_most(EC2, ATTEMPTS * EC2_OFFERS))
    .expect(Expect::acquires_at_most(LAMBDA, ATTEMPTS * LAMBDA_OFFERS))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .witness("acquire_refused", |c| c.get(Counter::AcquireRefused) >= 1)
    .settle_within(SETTLE)
}

/// The Lambda-like cloud's share in the preemption scenario.
pub const PAIR: u64 = 2;

/// When the Lambda-like cloud preempts its first machine, which runs the first job then.
pub const PREEMPT_AT: Duration = Duration::from_secs(200);

/// When the second job of the preemption scenario is submitted.
pub const SECOND_AT: Duration = Duration::from_secs(300);

/// When the client gives up on the preempted machine's job: the merged server ends a job
/// only on its host's word or its absence, and holds the release of a machine whose host
/// holds a job allocation, so that job stays booked until then.
pub const GIVE_UP: Duration = Duration::from_secs(1_000);

/// The EC2-like cloud refuses every acquire, so the Lambda-like one, of [`PAIR`] machines,
/// serves; at [`PREEMPT_AT`] it preempts its first machine, which runs the first job, and
/// at [`SECOND_AT`] a second job is asked for: a second Lambda-like machine is launched for
/// it, and it succeeds. How the first job ends (its client gives up at the latest) is not
/// asserted.
pub fn preempted_in_the_second() -> Scenario {
    on(
        Scenario::new("a_preempted_machine_in_the_second_cloud_is_replaced_without_double_booking"),
        0,
        (EC2_MACHINES, PAIR),
    )
    .at(Duration::ZERO, exhaust_provider(EC2))
    .work(1, job().give_up_after(GIVE_UP))
    .at(PREEMPT_AT, preempt_machine_of(LAMBDA, 0))
    .at(SECOND_AT, submit_work(1, job()))
    .expect(Expect::machines_launched(EC2, 0))
    .expect(Expect::machines_launched(LAMBDA, PAIR as usize))
    .expect(Expect::all_items_terminal())
    .expect(Expect::terminal_count(1, HowEnded::Succeeded))
    .witness("machine_preempted", |c| {
        c.get(Counter::MachinePreempted) >= 1
    })
    .settle_within(SETTLE)
}
