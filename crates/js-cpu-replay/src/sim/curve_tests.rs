use super::*;
use crate::shape::{Phase, Shape};
use crate::sim::{Arm, Farm, Rule, Setup};
use crate::sizing::Booking;
use crate::trace::tests::job;
use crate::trace::{End, Job};
use js_sizing::Day;
use js_types::CpuSeries;

const GIB: u64 = 1 << 30;

/// The CPU of the hosts of the tests' farms.
const HOST: u64 = 16_000;

/// Curves booked and packed with a limit of 7200 millicores (450 per mille of a host), a
/// quarter of a host (4000) the most a slot books.
const PACKED: Arm = Arm {
    name: "packed",
    bookings: Bookings::Curve,
    rule: Rule::Packed,
    limit_permille: 450,
};

/// `hosts` hosts of [`HOST`] and 64 GiB, jobs run flat.
fn setup(hosts: usize) -> Setup {
    Setup {
        farm: Farm {
            hosts,
            milli: HOST,
            bytes: 64 * GIB,
        },
        shape: Shape::Flat,
        speed: 1,
        held_out_ms: None,
    }
}

/// The 2 s means of a class of runs of 70 s: 3 cores, and 4 in slot `burst`.
fn burst_at(burst: usize) -> Vec<u32> {
    (0..35).map(|s| if s == burst { 64 } else { 48 }).collect()
}

/// A whole series of `points` at 2 s a point over as many seconds, and the CPU time it adds up
/// to, in milliseconds (a point of 16 is a core for 2 s: 2000 ms).
fn recorded(points: &[u32]) -> (CpuSeries, u64) {
    let cpu_ms = points.iter().map(|point| u64::from(*point)).sum::<u64>() * 125;
    let points: Vec<u16> = points.iter().map(|p| u16::try_from(*p).unwrap()).collect();
    let run_usec = points.len() as u64 * 2_000_000;
    (CpuSeries::new(2, points, run_usec, false).unwrap(), cpu_ms)
}

/// Teaches `class` a clean run of `points` that ended at `ended_ms`.
fn teach(class: &mut Learning, points: &[u32], ended_ms: u64) {
    let (series, cpu_ms) = recorded(points);
    class.teach(Some(&series), cpu_ms, ended_ms, true);
}

/// A class that has learned 24 runs of `points`, one a minute: its checks pass when the
/// points are not constant.
fn taught(points: &[u32]) -> Learning {
    let mut class = Learning::new();
    for run in 1..=24 {
        teach(&mut class, points, run * 60_000);
    }
    class
}

/// A world of `jobs` on `hosts` hosts where classes 1 and 2 book 2 cores flat and have
/// learned to burst at slots 10 (20 s) and 30 (60 s).
fn world(jobs: &[Job], hosts: usize) -> World<'_> {
    let mut world = World::new(jobs, PACKED, setup(hosts));
    for class in [1, 2] {
        let run = Job {
            cpu_ms: 120_000,
            ..job(0, 60_000, 0, class)
        };
        world
            .sizer
            .learn(&run, (60_000, 0), Booking::default(), Day(0));
    }
    world.curves.classes.insert(1, taught(&burst_at(10)));
    world.curves.classes.insert(2, taught(&burst_at(30)));
    world
}

/// Job `index` as it would start at `now`, with its curve.
fn running(world: &mut World<'_>, index: usize, now: u64) -> Running {
    let mut run = world.running(index, now);
    world.curved(&mut run, now);
    run
}

/// A job of 70 s of one core of `class`, arriving at 0.
fn of(class: u32) -> Job {
    job(0, 70_000, 70_000, class)
}

/// A running job booking `booking` with no curve, placed at `start`.
fn flat_run(booking: u64, start: u64) -> Running {
    Running {
        job: 0,
        booking: Booking {
            milli: booking,
            bytes: 0,
        },
        curve: None,
        var: 0,
        phases: vec![Phase { ms: 1000, milli: 0 }],
        phase: 0,
        left: 1000.0,
        start,
        own: 1,
        run: 1,
    }
}

/// Asserts the packing of bursts: of a class that bursts at 20 s, a second job started with the
/// first books its burst on top of the first's (4000 and 4000, past 7200) and no host takes
/// it beside the first, while a job of a class that bursts at 60 s (4000 over the first's
/// 3000 at 20 s, and 3000 over its 3000 at 60 s) fits. The figures are the fullest slot, and
/// the limit is met exactly: 8000 and 7000 fit a limit of 8000 and 7000 and no less.
#[test]
fn two_jobs_bursting_at_the_same_age_do_not_share_a_host_and_bursts_apart_do() {
    let jobs = [of(1), of(1), of(2)];
    let mut world = world(&jobs, 1);
    let first = running(&mut world, 0, 0);
    let same = running(&mut world, 1, 0);
    let apart = running(&mut world, 2, 0);
    let host = Host {
        runs: vec![first],
        ..Host::default()
    };
    assert_eq!(host.packed(&same, 8000), Some(8000));
    assert_eq!(host.packed(&same, 7999), None);
    assert_eq!(host.packed(&apart, 7000), Some(7000));
    assert_eq!(host.packed(&apart, 6999), None);
    // At the limit the arm sets (7200) the same is refused and the other fits, and the rule's
    // figure is the same.
    assert_eq!(host.figure(Rule::Packed, &same, 7200), None);
    assert_eq!(host.figure(Rule::Packed, &apart, 7200), Some(7000));
    // Through the placement pass: the same class is turned away from the one host...
    world.queue.extend([0, 1]);
    world.place(0);
    assert_eq!(world.hosts[0].runs.len(), 1);
    assert_eq!(world.queue, [1]);
    // ...and goes to the second host when there is one, while the other class goes beside it.
    let mut two = self::world(&jobs, 2);
    two.queue.extend([0, 1]);
    two.place(0);
    assert_eq!(two.hosts[0].runs.len(), 1);
    assert_eq!(two.hosts[1].runs.len(), 1);
    let mut beside = self::world(&jobs, 2);
    beside.queue.extend([0, 2]);
    beside.place(0);
    assert_eq!(beside.hosts[0].runs.len(), 2);
    assert!(beside.hosts[1].runs.is_empty());
}

/// Asserts what a running job is charged by its age: a job that has run 40 s is charged at 20
/// s ahead what its class booked at 60 s (3000, as all runs reached it), 40 s after the class's
/// runs end it is charged nothing, and a job still starting is at age 0 until its start has
/// passed: 3000 ahead in the first slot.
#[test]
fn a_running_job_is_charged_from_its_age_and_a_starting_job_is_at_age_zero() {
    let jobs = [of(1)];
    let mut world = world(&jobs, 1);
    let mut run = running(&mut world, 0, 0);
    assert_eq!(
        (run.charge(40_000, 0), run.charge(40_000, 10)),
        (3000, 3000)
    );
    assert_eq!(run.charge(40_000, 15), 0);
    assert_eq!(run.books(10), 4000);
    // Its start takes 5 s: age 0 until 5 s after its placement at 1 s.
    run.start = 1000;
    run.own = 8000;
    run.run = 3000;
    assert_eq!(run.age_ms(6000), 0);
    assert_eq!(run.age_ms(6001), 1);
    assert_eq!(run.charge(6000, 10), 4000);
    assert_eq!(run.charge(26_000, 10), 3000);
    // A job with no curve is charged its booking at every slot.
    let flat = flat_run(1500, 0);
    assert_eq!(
        (
            flat.charge(99_000, 0),
            flat.charge(99_000, 64),
            flat.books(7)
        ),
        (1500, 1500, 1500)
    );
}

/// Asserts the residual: a host whose last readings show more than its jobs' charges keeps the
/// excess at every slot (the fit is the figure and the excess within the limit) while its
/// figure leaves it out, a reading at the charges or below leaves none, and a job placed after
/// the reading is not covered by it.
#[test]
fn a_reading_above_the_charges_is_kept_at_every_slot_and_left_out_of_the_figure() {
    let jobs = [of(1), of(2)];
    let mut world = world(&jobs, 1);
    let first = running(&mut world, 0, 0);
    let apart = running(&mut world, 1, 0);
    let host = |reading: u64, read_at: u64| Host {
        runs: vec![first.clone()],
        readings: [0, reading],
        read_at,
        ..Host::default()
    };
    // The job is charged 3000 now. A reading of 3000 shows nothing beyond it; one more does.
    assert_eq!(host(3000, 1).packed(&apart, 7000), Some(7000));
    assert_eq!(host(3001, 1).packed(&apart, 7000), None);
    assert_eq!(host(3001, 1).packed(&apart, 7001), Some(7000));
    assert_eq!(host(9000, 1).packed(&apart, 13_000), Some(7000));
    assert_eq!(host(9000, 1).packed(&apart, 12_999), None);
    // A reading below the charges leaves no residual below zero.
    assert_eq!(host(1000, 1).packed(&apart, 7000), Some(7000));
    // A reading taken before the job started does not cover it: all 3000 of its charge
    // is on top of the reading, which is no residual.
    assert_eq!(host(9000, 0).packed(&apart, 16_000), Some(7000));
    assert_eq!(host(9000, 0).packed(&apart, 15_999), None);
}

/// Asserts that with every job flat the rule fits what the booked rule fits, on both sides of
/// the limit, and ranks by the booked figure when nothing is measured beyond it: two jobs of 3
/// and 2 cores and a new one of 1.5, measured at the 4.5 cores they and a reading beyond
/// them.
#[test]
fn with_every_job_flat_packed_fits_what_booked_fits() {
    let new = flat_run(1500, 0);
    let host = |readings: [u64; 2]| Host {
        runs: vec![flat_run(3000, 0), flat_run(2000, 0)],
        readings,
        read_at: 1,
        ..Host::default()
    };
    for readings in [[0, 0], [0, 4000], [0, 5000], [5000, 0], [0, 6500]] {
        let host = host(readings);
        for limit in [6499, 6500, 6501, 8499, 8500, 8501, 11_999, 12_000] {
            assert_eq!(
                host.figure(Rule::Packed, &new, limit).is_some(),
                host.figure(Rule::Booked, &new, limit).is_some(),
                "readings {readings:?}, limit {limit}"
            );
        }
        assert_eq!(host.planned(Rule::Packed), host.planned(Rule::Booked));
    }
    // Its figure is the bookings with the new job; the booked rule's carries the reading.
    let read = host([0, 6500]);
    assert_eq!(read.figure(Rule::Packed, &new, 12_000), Some(6500));
    assert_eq!(read.figure(Rule::Booked, &new, 12_000), Some(8000));
}

/// Asserts the wait bound: a job whose curve fits no host is tested flat (2000 at every slot,
/// 6000 at its fullest) from 30 s after it first found none, to the millisecond: at 29 s and
/// 29.999 s it still waits, at 30 s it is placed beside the first job with no curve.
#[test]
fn a_curve_job_waiting_thirty_seconds_at_the_head_is_tested_flat() {
    // The first job's start takes 100 s, so it stays at age 0 throughout.
    let jobs = [
        Job {
            start_ms: 100_000,
            ..of(1)
        },
        of(1),
    ];
    let mut world = world(&jobs, 1);
    world.queue.push_back(0);
    world.place(0);
    assert_eq!(world.hosts[0].runs.len(), 1);
    assert_eq!(world.curves.stalled, None);
    world.queue.push_back(1);
    for now in [0, 29_000, 29_999] {
        world.place(now);
        assert_eq!(world.queue, [1], "at {now}");
        assert_eq!(world.curves.stalled, Some(0), "at {now}");
    }
    world.place(30_000);
    assert!(world.queue.is_empty());
    assert_eq!(world.hosts[0].runs.len(), 2);
    assert!(world.hosts[0].runs[1].curve.is_none());
    assert_eq!(world.hosts[0].runs[1].start, 30_000);
    assert_eq!(world.curves.stalled, None);
}

/// Asserts the bound counts from the first pass that found no host and not from the job's
/// arrival: a job that reaches the head late is tested by its curve first, and bound from then
/// (29 s on is still its curve, 30 s flat); a job that is flat anyway starts no wait.
#[test]
fn the_wait_bound_starts_when_the_job_first_finds_no_host() {
    let jobs = [
        Job {
            start_ms: 100_000,
            ..of(1)
        },
        of(1),
        of(9),
    ];
    let mut world = world(&jobs, 1);
    world.queue.push_back(0);
    world.place(100_000);
    world.queue.push_back(1);
    // Long after its arrival, its first attempt is by its curve.
    world.place(100_000);
    assert_eq!(world.queue, [1]);
    assert_eq!(world.curves.stalled, Some(100_000));
    world.place(129_000);
    assert_eq!(world.queue, [1]);
    world.place(130_000);
    assert!(world.queue.is_empty());
    // A job of an unlearned class is flat: it books 4000 at every slot and, with the host
    // taken, no wait is recorded for it.
    world.queue.push_back(2);
    world.place(131_000);
    assert_eq!(world.queue, [2]);
    assert_eq!(world.curves.stalled, None);
}

/// Asserts a class books its curve only while it passes the four checks: at 23 runs it books
/// flat and at 24 its curve; with four of its last 16 series unusable it still does and with
/// five it books flat again; a class of constant runs has no shape and books flat.
#[test]
fn a_class_that_fails_a_check_books_flat() {
    let jobs = [of(1)];
    let mut world = world(&jobs, 1);
    let books = |world: &mut World<'_>| running(world, 0, 0).curve.is_some();
    let points = burst_at(10);
    let mut class = Learning::new();
    for run in 1..=23 {
        teach(&mut class, &points, run * 60_000);
    }
    assert!(!class.passes());
    world.curves.classes.insert(1, class.clone());
    assert!(!books(&mut world));
    teach(&mut class, &points, 24 * 60_000);
    assert!(class.passes());
    world.curves.classes.insert(1, class.clone());
    assert!(books(&mut world));
    for _ in 0..4 {
        class.series.push(false);
    }
    assert!(class.passes());
    class.series.push(false);
    assert!(!class.passes());
    world.curves.classes.insert(1, class);
    assert!(!books(&mut world));
    // Constant runs: the curve is the flat level, which is no shape.
    let mut constant = Learning::new();
    for run in 1..=30 {
        teach(&mut constant, &[48; 35], run * 60_000);
    }
    assert!(!constant.passes());
    // The job books its flat step beside the curve: 2000, whatever it books.
    assert_eq!(running(&mut world, 0, 0).booking.milli, 2000);
}

/// Asserts the curve is kept while the class and the flat step are the same and made again when
/// either changes: the same curve twice is one curve; a run taught, or a different flat step,
/// makes another, and its slots no run reached book the flat step.
#[test]
fn a_curve_is_kept_until_the_class_or_the_flat_step_changes() {
    let clamp = Clamp {
        least_millis: 250,
        most_millis: 4000,
    };
    let mut class = taught(&burst_at(10));
    let first = class.curve(2000, clamp, 0);
    assert!(Rc::ptr_eq(&first, &class.curve(2000, clamp, 9)));
    assert_eq!(
        (first.booking(10), first.booking(34), first.booking(35)),
        (4000, 3000, 2000)
    );
    let other = class.curve(2500, clamp, 0);
    assert!(!Rc::ptr_eq(&first, &other));
    assert_eq!(
        (
            other.booking(10),
            other.booking(35),
            other.booking(TAIL_SLOT)
        ),
        (4000, 2500, 2500)
    );
    teach(&mut class, &burst_at(10), 25 * 60_000);
    assert!(!Rc::ptr_eq(&other, &class.curve(2500, clamp, 0)));
}

/// Asserts what a job books over its life is its curve times the time in each slot: a job of
/// 10 s in a class with runs of 5 slots (3000, 3000, 4000, 3000, 3000) books 32 000 000
/// millicore-ms where its flat step of 2000 books 20 000 000, a job whose start took 3 s books the first
/// slot's 3000 for them as well, and a job past 128 s books the class's flat step in the tail.
#[test]
fn a_curve_job_books_its_curve_over_its_wall_time() {
    let points = [48, 48, 64, 48, 48];
    let booked = |job: Job, points: &[u32], wall: u64| {
        let jobs = [job];
        let mut world = world(&jobs, 1);
        world.curves.classes.insert(1, taught(points));
        world.run(&[(0, 0)], World::place);
        assert_eq!(world.outcome.last_end_ms, wall);
        world.outcome.booked
    };
    assert_eq!(
        booked(job(0, 10_000, 10_000, 1), &points, 10_000),
        2000 * 16_000
    );
    // A start of 3 s holds the booking of the first slot.
    let started = Job {
        start_ms: 3000,
        ..job(0, 10_000, 10_000, 1)
    };
    assert_eq!(
        booked(started, &points, 13_000),
        3000 * 3000 + 2000 * 16_000
    );
    // 140 s: 34 slots of 3000 and one of 4000, 29 slots of the flat 2000, and 12 s of the tail.
    let long = booked(job(0, 140_000, 140_000, 1), &burst_at(10), 140_000);
    assert_eq!(
        long,
        2000 * (34 * 3000 + 4000) + 2000 * 29 * 2000 + 12_000 * 2000
    );
    // With no curve it books its flat step: the cold class books 4000.
    let flat = [job(0, 10_000, 10_000, 5)];
    let mut world = World::new(&flat, PACKED, setup(1));
    world.run(&[(0, 0)], World::place);
    assert_eq!(world.outcome.booked, 4000 * 10_000);
}

/// The order a replay of `jobs` takes them in: each at its arrival.
fn arrivals(jobs: &[Job]) -> Vec<(u64, usize)> {
    jobs.iter()
        .enumerate()
        .map(|(index, job)| (job.arrive_ms, index))
        .collect()
}

/// Asserts what a replay teaches each class, as the job service's history teaches it: every
/// successful run whose host recorded a whole series that agrees with its CPU time teaches it
/// and counts as usable; a successful run with no series, a partial one or one that disagrees
/// teaches nothing and counts as unusable (one with no series starts its class); a failed run
/// counts as unusable in a class that exists and starts none; a run under a second teaches as
/// any other; and a class of constant runs does not pass the checks. Catches a curve arm that
/// learns a series it is not given, marks every run usable, or starts a class from a failure.
#[test]
fn the_replay_teaches_a_class_what_its_hosts_recorded() {
    let (series, cpu_ms) = recorded(&[32; 5]);
    assert_eq!(cpu_ms, 20_000);
    let with = |job: Job| Job {
        cpu_series: Some(series.clone()),
        ..job
    };
    // Thirty runs of class 1, a minute apart, each 10 s at 2 cores and recorded so.
    let mut jobs: Vec<Job> = (0..30)
        .map(|run| with(job(run * 60_000, 10_000, 20_000, 1)))
        .collect();
    let at = |n: u64| 1_800_000 + n * 60_000;
    jobs.extend([
        // Class 1: no series, a partial one, one that is half the CPU time, and a failure.
        job(at(0), 10_000, 20_000, 1),
        Job {
            cpu_series: CpuSeries::new(2, vec![32; 5], 10_000_000, true),
            ..job(at(1), 10_000, 20_000, 1)
        },
        with(job(at(2), 10_000, 40_000, 1)),
        Job {
            end: End::Failed,
            ..with(job(at(3), 10_000, 20_000, 1))
        },
        // A failure of class 2, which has no run.
        Job {
            end: End::Failed,
            ..with(job(at(4), 10_000, 20_000, 2))
        },
        // Half a second at 2 cores of class 3, its one point a core for a second.
        Job {
            cpu_series: CpuSeries::new(1, vec![16], 500_000, false),
            ..job(at(5), 500, 1000, 3)
        },
        // A success of class 4 with no series.
        job(at(6), 10_000, 20_000, 4),
    ]);
    let mut world = World::new(&jobs, PACKED, setup(1));
    world.run(&arrivals(&jobs), World::place);
    let classes = &world.curves.classes;
    assert_eq!(classes.keys().copied().collect::<Vec<u32>>(), [1, 3, 4]);
    let class = &classes[&1];
    assert_eq!(class.profile.runs(), 30);
    assert_eq!((class.series.runs(), class.series.usable()), (16, 12));
    assert!(!class.passes());
    // The first run of 5 slots at 2 cores: the profile reads 32 sixteenths in slot 0.
    assert_eq!(class.profile.slot_mean(0, 0), Some(32));
    let short = &classes[&3];
    assert_eq!((short.profile.runs(), short.series.usable()), (1, 1));
    let bare = &classes[&4];
    assert_eq!(bare.profile.runs(), 0);
    assert_eq!((bare.series.runs(), bare.series.usable()), (1, 0));
}

/// Asserts that a job of the held-out part teaches its class nothing: of twenty recorded runs
/// of class 1 a minute apart, with the part held out from the eleventh's arrival (at twice the
/// speed, so at 300 s of the replay), the profile and the window hold the first ten, and a
/// failure in the held-out part records no unusable series. Catches a held-out part the
/// profiles go on learning from, which would let them book what they learned on the day they
/// are judged on.
#[test]
fn a_job_of_the_held_out_part_teaches_its_class_nothing() {
    let (series, _) = recorded(&[32; 5]);
    let mut jobs: Vec<Job> = (0..20)
        .map(|run| Job {
            cpu_series: Some(series.clone()),
            ..job(run * 60_000, 10_000, 20_000, 1)
        })
        .collect();
    jobs.push(Job {
        end: End::Failed,
        ..job(1_200_000, 10_000, 20_000, 1)
    });
    let held = Setup {
        speed: 2,
        held_out_ms: Some(300_000),
        ..setup(1)
    };
    let mut world = World::new(&jobs, PACKED, held);
    let order: Vec<(u64, usize)> = arrivals(&jobs)
        .into_iter()
        .map(|(at, index)| (at / 2, index))
        .collect();
    world.run(&order, World::place);
    let class = &world.curves.classes[&1];
    assert_eq!(class.profile.runs(), 10);
    assert_eq!((class.series.runs(), class.series.usable()), (10, 10));
    assert!(world.outcome.ended.iter().all(|end| *end > 0));
}

/// Asserts the bound a run's slowdown is clean under: 9.9% of its run is clean, 10% is not, and a
/// run of no time is read as one of a millisecond. Catches a curve arm that takes every slowed
/// run as unclean (the job service keeps a run slowed by a few percent) or every run as clean.
#[test]
fn a_run_slowed_by_under_a_tenth_is_clean() {
    assert!(clean(0, 10_000));
    assert!(clean(999, 10_000));
    assert!(!clean(1000, 10_000));
    assert!(!clean(1, 0));
    assert!(clean(0, 0));
}

/// Asserts a run that contention slowed is taught as a series and counts as usable but only
/// raises its class: two jobs asking for the host's 4 cores, each slowed by half, are both
/// recorded in the series window and the profile keeps neither of the first, as it has no level
/// to be raised above.
#[test]
fn a_slowed_run_is_usable_but_does_not_lower_its_class() {
    let farm = Setup {
        farm: Farm {
            hosts: 1,
            milli: 4000,
            bytes: 64 * GIB,
        },
        ..setup(1)
    };
    let (series, cpu_ms) = recorded(&[64; 5]);
    assert_eq!(cpu_ms, 40_000);
    let slowed = Job {
        booked_milli: 0,
        cpu_series: Some(series),
        ..job(0, 10_000, 40_000, 1)
    };
    let jobs = [slowed.clone(), slowed];
    let arm = Arm {
        rule: Rule::Booked,
        limit_permille: 100_000,
        ..PACKED
    };
    let mut world = World::new(&jobs, arm, farm);
    world.run(&[(0, 0), (0, 1)], World::place);
    assert_eq!(world.outcome.slowed, [10_000, 10_000]);
    let class = &world.curves.classes[&1];
    assert_eq!(class.series.usable(), 2);
    assert_eq!(class.profile.runs(), 0);
}
