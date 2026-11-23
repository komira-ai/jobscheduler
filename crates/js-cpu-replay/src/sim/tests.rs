use super::*;
use crate::trace::tests::job;

const GIB: u64 = 1 << 30;

/// One host of `milli` CPU and 64 GiB.
fn one(milli: u64) -> Farm {
    Farm {
        hosts: 1,
        milli,
        bytes: 64 * GIB,
    }
}

fn arm(bookings: Bookings, rule: Rule, limit_permille: u64) -> Arm {
    Arm {
        name: "test",
        bookings,
        rule,
        limit_permille,
    }
}

fn flat(farm: Farm) -> Setup {
    Setup {
        farm,
        shape: Shape::Flat,
        speed: 1,
    }
}

/// A job booked `milli` CPU on the farm.
fn booked(job: Job, milli: u32) -> Job {
    Job {
        booked_milli: milli,
        ..job
    }
}

/// A run of `phases` booked `booking` with class variance `var`, started at `start`.
fn run(booking: u64, var: u64, phases: Vec<Phase>, start: u64) -> Running {
    Running {
        job: 0,
        booking: Booking {
            milli: booking,
            bytes: 0,
        },
        var,
        left: phases[0].ms as f64,
        phases,
        phase: 0,
        start,
        own: 1,
        run: 1,
    }
}

fn phase(ms: u64, milli: u64) -> Phase {
    Phase { ms, milli }
}

const BOOKED: Arm = Arm {
    name: "booked",
    bookings: Bookings::Recorded,
    rule: Rule::Booked,
    limit_permille: 1000,
};

#[test]
fn a_lone_job_runs_its_own_time_and_its_use_is_counted() {
    // Two cores for ten seconds, booked one.
    let jobs = [job(0, 10_000, 20_000, 0)];
    let outcome = replay(&jobs, BOOKED, flat(one(4000)));
    assert_eq!(outcome.waits, vec![0]);
    assert_eq!(outcome.slowed, vec![0]);
    assert_eq!(outcome.last_end_ms, 10_000);
    assert_eq!(outcome.own_ms, 10_000);
    assert_eq!(outcome.used, 2e7);
    assert_eq!((outcome.over_ms, outcome.over_excess), (0, 0));
    assert_eq!(outcome.booked, 10_000_000);
    assert_eq!(outcome.above_booking, 10_000_000);
    // Until the first reading covers it the placer holds the host at its booking (one core)
    // while it asks for two; from then on at its measured two.
    assert_eq!(outcome.under_ms, 5000);
    assert_eq!(outcome.plan_error, 5_000_000);
}

#[test]
fn every_rule_runs_a_lone_job_unslowed() {
    for rule in [
        Rule::Booked,
        Rule::Reserve { z_tenths: 20 },
        Rule::Measured,
        Rule::Curve,
    ] {
        let outcome = replay(
            &[job(0, 10_000, 20_000, 0)],
            arm(Bookings::Recorded, rule, 1000),
            flat(one(4000)),
        );
        assert_eq!(
            (outcome.waits, outcome.slowed),
            (vec![0], vec![0]),
            "{rule:?}"
        );
    }
}

#[test]
fn two_jobs_asking_twice_the_cores_each_take_twice_as_long() {
    // Each asks for the host's four cores for ten seconds; booked nothing, both are placed.
    let jobs = [
        booked(job(0, 10_000, 40_000, 0), 0),
        booked(job(0, 10_000, 40_000, 0), 0),
    ];
    let outcome = replay(&jobs, BOOKED, flat(one(4000)));
    assert_eq!(outcome.waits, vec![0, 0]);
    assert_eq!(outcome.slowed, vec![10_000, 10_000]);
    assert_eq!(outcome.slowed_ms, 20_000);
    assert_eq!(outcome.last_end_ms, 20_000);
    // Two jobs on the host for twenty seconds.
    assert_eq!(outcome.held_ms, 40_000);
    assert_eq!(outcome.over_ms, 20_000);
    assert_eq!(outcome.over_excess, 80_000_000);
    assert_eq!(outcome.used, 8e7);
}

#[test]
fn the_walk_stops_at_the_first_job_no_host_takes_and_a_reading_lags_two_windows() {
    // A takes the host's four cores for ten seconds; B asks for as much and waits; C books
    // nothing and fits, but waits behind B. When A ends the host still reads four cores for
    // two windows (the larger of the last two readings), so B and C start at 20 s.
    let jobs = [
        booked(job(0, 10_000, 40_000, 0), 4000),
        booked(job(0, 10_000, 40_000, 0), 4000),
        booked(job(0, 1000, 0, 0), 0),
    ];
    let outcome = replay(&jobs, BOOKED, flat(one(4000)));
    assert_eq!(outcome.waits, vec![0, 20_000, 20_000]);
}

#[test]
fn measured_load_places_beside_a_job_that_books_more_than_it_uses() {
    // A books six cores and uses one for 100 s; B, booked six, arrives at 20 s.
    let jobs = [
        booked(job(0, 100_000, 100_000, 0), 6000),
        booked(job(20_000, 10_000, 10_000, 0), 6000),
    ];
    // By bookings, B waits for A's end (6 + 6 > 8 cores)...
    let by_bookings = replay(&jobs, BOOKED, flat(one(8000)));
    assert_eq!(by_bookings.waits, vec![0, 80_000]);
    // ...by measurement, one measured core and six booked fit at once.
    let measured = replay(
        &jobs,
        arm(Bookings::Recorded, Rule::Measured, 1000),
        flat(one(8000)),
    );
    assert_eq!(measured.waits, vec![0, 0]);
}

#[test]
fn memory_bookings_hold_a_job_whatever_the_cpu() {
    let farm = Farm {
        bytes: 4 * GIB,
        ..one(8000)
    };
    let two = Job {
        booked_mib: 3072,
        ..booked(job(0, 10_000, 0, 0), 0)
    };
    let outcome = replay(&[two.clone(), two], BOOKED, flat(farm));
    assert_eq!(outcome.waits, vec![0, 10_000]);
}

#[test]
fn a_job_goes_to_the_host_it_fits_most_tightly_and_the_first_on_a_tie() {
    let jobs = [
        booked(job(0, 10_000, 0, 0), 4000),
        booked(job(0, 10_000, 0, 0), 1000),
    ];
    let farm = Farm {
        hosts: 2,
        ..one(8000)
    };
    let mut world = World::new(&jobs, BOOKED, flat(farm));
    world.queue.extend([0, 1]);
    world.place(0);
    // Both hosts were empty for the first: it went to host 0. The second fits host 0 more
    // tightly (5 of 8 cores) than host 1 (1 of 8).
    assert_eq!(world.hosts[0].runs.len(), 2);
    assert!(world.hosts[1].runs.is_empty());
    assert!(world.queue.is_empty());
}

#[test]
fn the_reserve_is_the_bookings_plus_a_multiple_of_the_root_of_the_summed_variances() {
    let host = Host {
        runs: vec![
            run(1000, 1_000_000, vec![phase(1000, 0)], 0),
            run(1000, 1_000_000, vec![phase(1000, 0)], 0),
        ],
        ..Host::default()
    };
    let new = run(1000, 2_000_000, vec![phase(1000, 0)], 0);
    // 3 cores booked, and twice the root of 4 square cores: 7 cores.
    let rule = Rule::Reserve { z_tenths: 20 };
    assert_eq!(host.figure(rule, &new, 7000), Some(7000));
    assert_eq!(host.figure(rule, &new, 6999), None);
    // What the rule holds the host to be using includes the reserve of its two jobs: 2 cores
    // and twice the root of 2 square cores (1.414), 4.828 cores.
    assert_eq!(host.planned(rule), 4828);
}

#[test]
fn a_curve_admits_a_burst_into_another_jobs_quiet_time() {
    let mut late = run(6000, 0, vec![phase(5000, 2000), phase(4000, 1000)], 0);
    late.phase = 1;
    late.left = 500.0;
    let host = Host {
        runs: vec![
            // Quiet for a second, then six cores for two.
            run(6000, 0, vec![phase(1000, 0), phase(2000, 6000)], 0),
            // In its last phase: one core for half a second more.
            late,
            // Busy only after the new job's three seconds.
            run(9000, 0, vec![phase(3000, 0), phase(1000, 9000)], 0),
        ],
        ..Host::default()
    };
    // Six cores for a second, then quiet: at most seven cores at once, though the host's jobs
    // and it book 27.
    let new = run(6000, 0, vec![phase(1000, 6000), phase(2000, 0)], 0);
    assert_eq!(host.peak_with(&new.phases), 7000);
    assert_eq!(host.figure(Rule::Curve, &new, 7000), Some(7000));
    assert_eq!(host.figure(Rule::Curve, &new, 6999), None);
    assert_eq!(host.planned(Rule::Curve), 1000);
}

#[test]
fn the_placer_counts_a_reading_for_the_jobs_that_ran_before_it() {
    let host = Host {
        runs: vec![
            run(3000, 0, vec![phase(1000, 0)], 0),
            run(2000, 0, vec![phase(1000, 0)], 5000),
        ],
        readings: [1000, 4000],
        read_at: 5000,
        ..Host::default()
    };
    assert_eq!(host.measured(), 4000);
    assert_eq!(host.split(), (3000, 2000));
    assert_eq!(host.planned(Rule::Booked), 6000);
    assert_eq!(host.planned(Rule::Measured), 6000);
    let low = Host {
        readings: [0, 1000],
        ..host
    };
    assert_eq!(low.planned(Rule::Booked), 5000);
    assert_eq!(low.planned(Rule::Measured), 3000);
}

#[test]
fn a_quiet_phase_is_not_slowed_and_a_busy_one_is() {
    let mut running = run(0, 0, vec![phase(500, 0), phase(1000, 4000)], 0);
    // Half a second quiet, then half a second at half speed: a quarter of the busy phase.
    assert_eq!(running.step(0.5, 1000.0), (1_000_000.0, None));
    assert_eq!((running.phase, running.left), (1, 750.0));
    // The rest takes 1.5 s at half speed, so it ends in the second tick after this one.
    assert_eq!(running.step(0.5, 1000.0), (2_000_000.0, None));
    assert_eq!(running.step(0.5, 1000.0), (1_000_000.0, Some(500)));
}

#[test]
fn jobs_replayed_as_placed_meet_the_slowdown_of_their_host() {
    // A starts alone; B joins it on the same host (index 7 of one host) at 5 s; each then gets
    // half of what it asks.
    let jobs = [
        job(0, 10_000, 40_000, 0),
        Job {
            wait_ms: 5000,
            host: 7,
            ..job(0, 10_000, 40_000, 0)
        },
    ];
    let outcome = as_placed(&jobs, flat(one(4000)));
    assert_eq!(outcome.waits, vec![0, 5000]);
    assert_eq!(outcome.slowed, vec![5000, 5000]);
    assert_eq!(outcome.last_end_ms, 20_000);
}

#[test]
fn an_idle_farm_skips_to_the_next_arrival_and_a_faster_trace_arrives_sooner() {
    let late = job(3_600_001, 1000, 0, 0);
    let outcome = replay(std::slice::from_ref(&late), BOOKED, flat(one(4000)));
    assert_eq!(outcome.waits, vec![999]);
    assert_eq!(outcome.last_end_ms, 3_602_000);
    let twice = replay(
        &[late],
        BOOKED,
        Setup {
            speed: 2,
            ..flat(one(4000))
        },
    );
    assert_eq!(twice.waits, vec![0]);
    assert_eq!(twice.last_end_ms, 1_801_000);
    assert_eq!(replay(&[], BOOKED, flat(one(4000))), Outcome::default());
}

#[test]
fn learned_bookings_start_cold_and_follow_the_class() {
    // The first run of a class books the cold 22 cores; the second, after the first ended,
    // books its mean of two.
    let jobs = [job(0, 10_000, 20_000, 1), job(20_000, 10_000, 20_000, 1)];
    let outcome = replay(
        &jobs,
        arm(Bookings::Learned, Rule::Booked, 1100),
        flat(Farm::OURS),
    );
    assert_eq!(outcome.booked, 22_000 * 10_000 + 2000 * 10_000);
}

#[test]
fn a_class_spread_is_learned_from_its_last_32_good_runs_and_its_history_from_64() {
    let mut jobs: Vec<Job> = (0..65).map(|at| job(at * 10_000, 2000, 2000, 3)).collect();
    // A failed run and a run under a second teach nothing.
    jobs.push(Job {
        end: End::Failed,
        ..job(700_000, 2000, 2000, 3)
    });
    jobs.push(job(710_000, 500, 500, 3));
    let mut world = World::new(&jobs, BOOKED, flat(one(4000)));
    let order: Vec<(u64, usize)> = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| (job.arrive_ms, index))
        .collect();
    world.run(&order, World::place);
    // Class 3 and the client 0 each hold 32 runs of one core.
    assert_eq!(world.spreads[&3].runs.len(), RING);
    assert_eq!(world.spreads[&0].runs.len(), RING);
    assert_eq!(world.spreads[&3].var(), 0);
    // The decayed estimates keep 64, the newest last.
    assert_eq!(world.histories[&3].len(), HISTORY);
    assert_eq!(world.histories[&3].back().map(|run| run.end), Some(642_000));
    // Runs of one core and four, and of three and three: a variance of 2.5 square cores.
    let spread = Spread {
        runs: VecDeque::from([(1000, 4000), (3000, 3000)]),
    };
    assert_eq!(spread.var(), 2_500_000);
    assert_eq!(Spread::default().var(), 0);
}

#[test]
fn a_burst_falls_at_a_spot_fixed_by_the_trace_index() {
    assert_eq!(spot(0), 0);
    assert_eq!(spot(1), 769);
    let jobs = [Job {
        peak_milli: Some(4000),
        ..job(0, 10_000, 10_000, 0)
    }];
    let burst = Setup {
        shape: Shape::Burst,
        ..flat(one(8000))
    };
    let world = World::new(&jobs, BOOKED, burst);
    let running = world.running(0, 0);
    assert_eq!(running.phases, vec![phase(2500, 4000), phase(7500, 0)]);
    assert_eq!(running.own, 10_000);
}

/// The host of `hosts` (two of eight cores) a queued job booked one core goes to.
fn chosen(hosts: [Host; 2]) -> usize {
    let jobs = vec![booked(job(0, 10_000, 0, 0), 1000)];
    let farm = Farm {
        hosts: 2,
        ..one(8000)
    };
    let arm = arm(Bookings::Recorded, Rule::Booked, 1100);
    let mut world = World::new(&jobs, arm, flat(farm));
    world.hosts = hosts.to_vec();
    world.queue.push_back(0);
    world.place(0);
    world
        .hosts
        .iter()
        .position(|host| host.runs.iter().any(|run| run.own == 10_000))
        .unwrap()
}

/// A host running one job booked `booking`, started before its last two readings of
/// `measured`.
fn holding(booking: u64, measured: u64) -> Host {
    Host {
        runs: vec![run(booking, 0, vec![phase(60_000, 0)], 0)],
        readings: [measured, measured],
        read_at: 5000,
        ..Host::default()
    }
}

#[test]
fn the_score_avoids_a_host_measured_above_its_bookings() {
    // Best fit alone would take host 0 (3 + 1 of 8 cores booked, against 2 + 1); it is
    // measured at three cores where its job books two.
    assert!(holding(2000, 3000).distrusted());
    assert!(!holding(2000, 1000).distrusted());
    assert_eq!(chosen([holding(2000, 3000), holding(2000, 1000)]), 1);
    // Both trusted: the tighter fit.
    assert_eq!(chosen([holding(2000, 1500), holding(2000, 1000)]), 0);
}

#[test]
fn the_score_prefers_a_host_within_its_cores_to_one_in_the_slack() {
    // Host 0 would be at 8.5 of 8 cores (within the 1.1 slack, past 101%); host 1 at 3.
    assert_eq!(chosen([holding(7500, 0), holding(2000, 0)]), 1);
}

#[test]
fn each_jobs_wait_is_kept_by_its_index() {
    let jobs = [
        booked(job(0, 10_000, 40_000, 0), 4000),
        booked(job(0, 10_000, 40_000, 0), 4000),
    ];
    let outcome = replay(&jobs, BOOKED, flat(one(4000)));
    assert_eq!(outcome.waited, vec![0, 20_000]);
}

#[test]
fn a_job_no_empty_host_admits_runs_alone_and_the_replay_ends() {
    // Booked 300 GiB on hosts of 64 GiB, and 10 cores on a host of 4: it waits for an empty
    // farm, then runs alone; the job behind it waits for it.
    let big = Job {
        booked_mib: 300 * 1024,
        ..booked(job(0, 10_000, 10_000, 0), 10_000)
    };
    let jobs = [job(0, 5000, 5000, 0), big, job(0, 1000, 1000, 0)];
    let outcome = replay(&jobs, BOOKED, flat(one(4000)));
    assert_eq!(outcome.alone, 1);
    // The first job ends at 5 s; the big one starts on the empty farm then and runs ten
    // seconds; the last fits beside nothing it holds, so it starts at 15 s.
    assert_eq!(outcome.waited[1], 5000);
    assert_eq!(outcome.waited[2], 15_000);
}

#[test]
fn a_jobs_start_holds_its_booking_asks_for_no_cpu_and_is_not_slowed() {
    // Three seconds of start, then two seconds at the host's four cores, beside a job that
    // asks for all four throughout: only the run is slowed.
    let started = Job {
        start_ms: 3000,
        ..booked(job(0, 2000, 8000, 0), 0)
    };
    let busy = booked(job(0, 20_000, 80_000, 0), 0);
    let outcome = replay(&[started, busy], BOOKED, flat(one(4000)));
    // The run's two seconds at half speed take four: placed at 0, it ends at 7 s.
    assert_eq!(outcome.slowed[0], 2000);
    assert_eq!(outcome.own_ms, 5000 + 20_000);
}

#[test]
fn a_decayed_estimate_weights_recent_runs_and_a_quantile_weights_by_load() {
    let hour = 3_600_000;
    let past = VecDeque::from([
        // An hour old: half the weight of the run that just ended.
        Past {
            end: 0,
            mean: 1000,
            cpu: 1000,
        },
        Past {
            end: hour,
            mean: 4000,
            cpu: 3000,
        },
    ]);
    // The mean: (1000 x 0.5 + 4000 x 1) / 1.5 = 3000.
    assert_eq!(decayed(&past, hour, hour, None), 3000);
    // By load: 500 core-ms at 1 core, 3000 at 4 cores; the 10th percentile is the first run,
    // the 50th already the second.
    assert_eq!(decayed(&past, hour, hour, Some(100)), 1000);
    assert_eq!(decayed(&past, hour, hour, Some(500)), 4000);
    // No CPU at all: the lowest.
    let idle = VecDeque::from([Past {
        end: 0,
        mean: 7,
        cpu: 0,
    }]);
    assert_eq!(decayed(&idle, 0, hour, Some(900)), 7);
}

#[test]
fn decayed_bookings_start_cold_then_follow_the_class() {
    // The first run of a class books the cold 22 cores; the second books the mean of the
    // first, two cores; a third of another class falls back to the client's.
    let jobs = [
        job(0, 10_000, 20_000, 1),
        job(20_000, 10_000, 20_000, 1),
        job(40_000, 10_000, 20_000, 2),
    ];
    let decayed = Bookings::Decayed {
        half_life_ms: 3_600_000,
        quantile_permille: None,
    };
    let outcome = replay(&jobs, arm(decayed, Rule::Booked, 1100), flat(Farm::OURS));
    assert_eq!(outcome.booked, (22_000 + 2000 + 2000) * 10_000);
}
