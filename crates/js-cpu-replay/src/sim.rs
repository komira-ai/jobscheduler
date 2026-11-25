//! The replay: recorded arrivals placed on a farm by one rule, and run.
//!
//! Time moves in ticks of one second. Each tick, every job that has arrived joins the queue,
//! a placement pass walks the queue in arrival order (the walk of the job service: the first
//! job no host takes ends the pass, so no later job passes it), and every host runs its jobs
//! for the tick. Every five seconds each host is read, as an agent's heartbeat reads its
//! actions' `cpu.stat`.
//!
//! A host shares its cores the way a kernel shares them between groups with no CPU limit:
//! while what its jobs ask for is within its cores each gets what it asks; past them each
//! gets the same share of what it asks (`cores / asked`), so each runs slower by that much.
//! A job's phases ([`crate::shape`]) are in its own time; a slowed job takes longer, and the
//! time it lost is its slowdown.
//!
//! The curve mode ([`curve`]) books and packs by what each class learned of its jobs' CPU over
//! their age.

use std::cmp::Reverse;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use js_sizing::Curve;

use crate::shape::{self, Phase, Shape};
use crate::sizing::{Booking, Sizer, day_of};
use crate::trace::{End, Job};
use js_sched_placer::sizes::DEFAULT_COLD_CPU_MILLIS;

pub mod curve;

/// One tick, in milliseconds.
pub const TICK_MS: u64 = 1000;
/// The window a host is read over, in milliseconds (the agent's default heartbeat).
pub const WINDOW_MS: u64 = 5000;
/// How the placer's score ranks a host that admits a job, the least first: how far its figure
/// goes past 101% of its cores, whether it is measured above its bookings, and the reverse of
/// the figure (the tightest fit first).
type Rank = (u64, bool, Reverse<u64>);

/// The rank of a job taken alone onto an empty host: any, as none ranks.
const ALONE: Rank = (0, false, Reverse(0));

/// The runs of a class its spread is learned from: the last 32, as its CPU.
const RING: usize = 32;

/// The farm a replay runs on: hosts of one size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Farm {
    /// How many hosts.
    pub hosts: usize,
    /// The CPU of each, in millicores.
    pub milli: u64,
    /// The memory each lets jobs book, in bytes.
    pub bytes: u64,
}

impl Farm {
    /// Ours: six hosts of 88 threads and 251 GiB, all of it bookable.
    pub const OURS: Farm = Farm {
        hosts: 6,
        milli: 88_000,
        bytes: 251 << 30,
    };
}

/// Where a job's booking comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bookings {
    /// What the job was booked on the farm, as recorded.
    Recorded,
    /// What the placer's sizing books it from the history the replay has seen so far, every
    /// run taught without its peak: the median of its class's runs' means.
    Learned,
    /// [`Bookings::Learned`] with each run taught with its recorded peak, as the placer teaches
    /// it: a class whose peaks are a heavy tail books the 90th percentile of them.
    Tailed,
    /// The placer's memory, and a CPU from the class's last runs (else its fallback's, else
    /// its client's) weighted by `2^(-age / half_life_ms)`, as Autopilot weights its samples:
    /// their weighted mean, or with `quantile_permille` the load-weighted quantile of their
    /// means (each run counts by its CPU time, as Autopilot counts a sample by its load).
    /// Rounded up to a quarter core and held to a quarter of the host, as the placer's.
    Decayed {
        /// The age at which a run counts half, in milliseconds.
        half_life_ms: u64,
        /// The quantile, in thousandths; `None` for the weighted mean.
        quantile_permille: Option<u64>,
    },
    /// [`Bookings::Learned`], and a job of a class that passes the four checks of
    /// `js_sizing::Checks` books its CPU curve over its age instead: each class learns, at
    /// each successful run's end, the run's series as the replay synthesised it
    /// ([`crate::shape`]) in 2 s means. A job's `booking` stays its flat step; the curve
    /// books beside it ([`Rule::Packed`] places by it, and the outcome's `booked` integrates
    /// it). See [`curve`].
    Curve,
}

/// One run a class's decayed estimate is taken over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Past {
    /// When it ended.
    end: u64,
    /// Its mean CPU, in millicores.
    mean: u64,
    /// Its CPU time, in milliseconds.
    cpu: u64,
}

/// The runs a class's decayed estimate keeps: the last 64.
const HISTORY: usize = 64;

/// The decayed estimate of `past` (never empty) at `now` (see [`Bookings::Decayed`]).
fn decayed(past: &VecDeque<Past>, now: u64, half_life_ms: u64, quantile: Option<u64>) -> u64 {
    let weight = |run: &Past| {
        let age = now.saturating_sub(run.end) as f64 / half_life_ms.max(1) as f64;
        0.5f64.powf(age)
    };
    let Some(permille) = quantile else {
        let total: f64 = past.iter().map(weight).sum();
        let sum: f64 = past.iter().map(|run| weight(run) * run.mean as f64).sum();
        return (sum / total) as u64;
    };
    let mut runs: Vec<&Past> = past.iter().collect();
    runs.sort_by_key(|run| run.mean);
    let mut seen = 0.0;
    let cumulative: Vec<f64> = runs
        .iter()
        .map(|run| {
            seen += weight(run) * run.cpu as f64;
            seen
        })
        .collect();
    let goal = seen * permille as f64 / 1000.0;
    let at = cumulative.iter().position(|sum| *sum >= goal);
    runs[at.unwrap_or(runs.len() - 1)].mean
}

/// How a host decides whether it takes one more job; every rule also keeps the memory
/// booked within the host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// The placer today: the larger of the host's measured CPU and the bookings of its jobs
    /// the measurement covers, plus the bookings of the jobs it does not cover yet, plus the
    /// new job's, within the limit.
    Booked,
    /// The bookings of the host's jobs and the new one (each its class's typical CPU), plus
    /// `z_tenths / 10` times the root of the sum of their classes' variances, within the
    /// limit.
    Reserve {
        /// The multiple of the root of the summed variances, in tenths.
        z_tenths: u64,
    },
    /// The host's measured CPU, plus the bookings of the jobs the measurement does not
    /// cover yet, plus the new job's, within the limit.
    Measured,
    /// The CPU every job on the host and the new one will ask for at each moment of the new
    /// job's life, known exactly (an oracle: the best any learned curve could do), within the
    /// limit at every moment.
    Curve,
    /// The curves of the jobs on the host and the new job's, within the limit at every slot
    /// ahead (`docs/design/placement-profiles-live.md` section 3): each running job is charged
    /// what its curve says it still books at that slot, weighted by the share of its class's
    /// runs that reach it (its flat booking when it has no curve), and the host's residual (its
    /// reading beyond its jobs' charges now) is carried to every slot. A host is ranked by
    /// the fullest slot with the new job, without the residual. With no curve it is
    /// [`Rule::Booked`]. See [`curve`].
    Packed,
    /// The placer as it ran before its hosts' readings reached it (komira-ai/jobscheduler#1036):
    /// no reading, so every job is charged the larger of its booking and one core
    /// (`UNMEASURED_CPU_FLOOR_MILLIS` in `js-sched`), and no host is ranked behind another
    /// for its reading.
    Unread,
}

/// What a job is charged on a host that has never been read: at least a core.
const UNREAD_FLOOR_MILLI: u64 = 1000;

/// One way of packing: a rule, its bookings and its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    /// Its name in the report.
    pub name: &'static str,
    /// Where its bookings come from.
    pub bookings: Bookings,
    /// Its rule.
    pub rule: Rule,
    /// Its limit, in thousandths of a host's cores (1100: the default CPU slack of 1.1).
    pub limit_permille: u64,
}

/// What a replay runs with besides the arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// The farm.
    pub farm: Farm,
    /// How the jobs' CPU is spread over their lives.
    pub shape: Shape,
    /// How many times faster the jobs arrive than they did (1: as recorded).
    pub speed: u64,
}

/// What a replay measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// Each job's wait from its arrival to its placement, in milliseconds, in the order
    /// the jobs were placed.
    pub waits: Vec<u64>,
    /// Each job's wait, by trace index; 0 for one not placed.
    pub waited: Vec<u64>,
    /// Each job's slowdown (its wall time less its own), by trace index; 0 for one not run.
    pub slowed: Vec<u64>,
    /// The end of the last job, in milliseconds since the first arrival.
    pub last_end_ms: u64,
    /// The sum of the jobs' own wall times, in milliseconds.
    pub own_ms: u64,
    /// The sum of their slowdowns, in milliseconds.
    pub slowed_ms: u64,
    /// The host-time a host was asked for more CPU than it has, in milliseconds.
    pub over_ms: u64,
    /// The CPU asked for past the hosts' cores, summed over that time, in millicore-ms.
    pub over_excess: u128,
    /// The CPU the jobs got, in millicore-ms.
    pub used: f64,
    /// The jobs no host admitted even with nothing on the farm, each run alone on an empty
    /// host.
    pub alone: usize,
    /// The jobs on the hosts, summed over the host-time, in job-ms.
    pub held_ms: u128,
    /// The host-time the CPU asked for was above what the rule held the host to be using.
    pub under_ms: u64,
    /// The distance between what the rule held each host to be using and what its jobs asked
    /// for, summed over the host-time, in millicore-ms.
    pub plan_error: u128,
    /// What the jobs booked times their wall time, in millicore-ms.
    pub booked: u128,
    /// The CPU the jobs used beyond their booking, in millicore-ms.
    pub above_booking: u128,
    /// The most CPU booked on one host at once, in millicores.
    pub booked_peak: u64,
    /// The jobs started on each host, by host index.
    pub placed: Vec<u64>,
}

/// A job running on a host.
#[derive(Clone, Debug)]
struct Running {
    /// Its index in the trace.
    job: usize,
    /// What it booked.
    booking: Booking,
    /// The curve it books beside its flat `booking`, when its class passes the checks.
    curve: Option<Rc<Curve>>,
    /// Its class's variance of CPU, in millicores squared.
    var: u64,
    /// Its phases.
    phases: Vec<Phase>,
    /// The phase it is in.
    phase: usize,
    /// The milliseconds of its own time left in that phase.
    left: f64,
    /// When it was placed.
    start: u64,
    /// Its own wall time, from its placement: its start and its run.
    own: u64,
    /// Its own run time, from its start.
    run: u64,
}

impl Running {
    /// The CPU it asks for now.
    fn demand(&self) -> u64 {
        self.phases[self.phase].milli
    }

    /// Runs it for `tick` milliseconds at `rate` (the share of what it asks that it gets):
    /// the CPU it got, and the milliseconds into the tick at which it ended, if it did.
    fn step(&mut self, rate: f64, tick: f64) -> (f64, Option<u64>) {
        let mut wall = tick;
        let mut got = 0.0;
        loop {
            let milli = self.phases[self.phase].milli;
            // A phase that asks for nothing is not slowed.
            let speed = if milli == 0 { 1.0 } else { rate };
            let need = self.left / speed;
            if need > wall {
                self.left -= wall * speed;
                got += milli as f64 * wall * speed;
                return (got, None);
            }
            wall -= need;
            got += milli as f64 * self.left;
            self.phase += 1;
            match self.phases.get(self.phase) {
                Some(next) => self.left = next.ms as f64,
                None => return (got, Some((tick - wall).ceil() as u64)),
            }
        }
    }
}

/// A host and what runs on it.
#[derive(Clone, Debug, Default)]
struct Host {
    /// Its jobs.
    runs: Vec<Running>,
    /// The memory they booked.
    bytes: u64,
    /// The CPU its jobs got since its last reading, in millicore-ms.
    used: f64,
    /// Its last two readings (the mean CPU of a window), the newest last.
    readings: [u64; 2],
    /// When it was last read: a job that started before it is covered by the reading.
    read_at: u64,
}

impl Host {
    /// Its measured CPU: the larger of its last two readings, as the placer takes it.
    fn measured(&self) -> u64 {
        self.readings[0].max(self.readings[1])
    }

    /// The bookings of its jobs its last reading covers, and of the others.
    fn split(&self) -> (u64, u64) {
        self.runs.iter().fold((0, 0), |(covered, not), run| {
            if run.start < self.read_at {
                (covered + run.booking.milli, not)
            } else {
                (covered, not + run.booking.milli)
            }
        })
    }

    /// Whether its measured CPU is above the bookings of the jobs the measurement covers: the
    /// placer's score avoids such a host (its key 3, "distrusted").
    fn distrusted(&self) -> bool {
        self.measured() > self.split().0
    }

    /// The bookings of its jobs and `milli` more, plus `z_tenths / 10` times the root of
    /// their classes' variances and `var` more.
    fn reserved(&self, z_tenths: u64, milli: u64, var: u64) -> u64 {
        let booked: u64 = self.runs.iter().map(|run| run.booking.milli).sum();
        let var: u64 = self.runs.iter().map(|run| run.var).sum::<u64>() + var;
        booked + milli + z_tenths * var.isqrt() / 10
    }

    /// The CPU `rule` holds it to be using.
    fn planned(&self, rule: Rule) -> u64 {
        match rule {
            Rule::Booked | Rule::Packed => {
                let (covered, not) = self.split();
                self.measured().max(covered) + not
            }
            Rule::Measured => self.measured() + self.split().1,
            Rule::Reserve { z_tenths } => self.reserved(z_tenths, 0, 0),
            Rule::Curve => self.runs.iter().map(Running::demand).sum(),
            Rule::Unread => self
                .runs
                .iter()
                .map(|run| run.booking.milli.max(UNREAD_FLOOR_MILLI))
                .sum(),
        }
    }

    /// The figure `rule` gives it with `new` on it, or `None` past `limit`.
    fn figure(&self, rule: Rule, new: &Running, limit: u64) -> Option<u64> {
        let figure = match rule {
            Rule::Packed => return self.packed(new, limit),
            Rule::Booked | Rule::Measured => self.planned(rule) + new.booking.milli,
            Rule::Unread => self.planned(rule) + new.booking.milli.max(UNREAD_FLOOR_MILLI),
            Rule::Reserve { z_tenths } => self.reserved(z_tenths, new.booking.milli, new.var),
            Rule::Curve => self.peak_with(&new.phases),
        };
        (figure <= limit).then_some(figure)
    }

    /// The most CPU its jobs and a new job of `phases` ask for at once while the new job
    /// runs, if none is slowed.
    fn peak_with(&self, phases: &[Phase]) -> u64 {
        let end: u64 = phases.iter().map(|phase| phase.ms).sum();
        let mut steps: Vec<(u64, i128)> = Vec::new();
        let mut add = |from: u64, ms: u64, milli: u64| {
            if from < end && milli > 0 {
                steps.push((from, i128::from(milli)));
                steps.push(((from + ms).min(end), -i128::from(milli)));
            }
        };
        for run in &self.runs {
            let mut from = 0;
            for (at, phase) in run.phases.iter().enumerate().skip(run.phase) {
                let ms = if at == run.phase {
                    run.left.ceil() as u64
                } else {
                    phase.ms
                };
                add(from, ms, phase.milli);
                from += ms;
            }
        }
        let mut from = 0;
        for phase in phases {
            add(from, phase.ms, phase.milli);
            from += phase.ms;
        }
        steps.sort_unstable();
        let mut load: i128 = 0;
        let mut peak: i128 = 0;
        for (_, change) in steps {
            load += change;
            peak = peak.max(load);
        }
        u64::try_from(peak).unwrap_or(u64::MAX)
    }
}

/// What a class's recent runs used: each run's mean and peak, in millicores.
#[derive(Clone, Debug, Default)]
struct Spread {
    runs: VecDeque<(u64, u64)>,
}

impl Spread {
    /// The variance of the CPU a run of the class asks for at a moment of its life, in
    /// millicores squared, if each run spent its life at its peak or idle (the largest its
    /// mean and peak allow): the mean of `mean x peak` less the square of the mean of `mean`.
    fn var(&self) -> u64 {
        let n = self.runs.len().max(1) as f64;
        let square: f64 = self
            .runs
            .iter()
            .map(|&(mean, peak)| (mean * peak) as f64)
            .sum();
        let mean: f64 = self.runs.iter().map(|&(mean, _)| mean as f64).sum::<f64>() / n;
        (square / n - mean * mean).max(0.0) as u64
    }
}

/// The world of one replay.
struct World<'a> {
    jobs: &'a [Job],
    arm: Arm,
    setup: Setup,
    hosts: Vec<Host>,
    queue: VecDeque<usize>,
    sizer: Sizer,
    spreads: BTreeMap<u32, Spread>,
    histories: BTreeMap<u32, VecDeque<Past>>,
    outcome: Outcome,
    /// Whether anything that can change a placement happened since the last pass: a job
    /// joined an empty queue, a job ended or the hosts were read.
    dirty: bool,
    /// What the curve mode has learned and waits for.
    curves: curve::Curves,
}

/// Replays `jobs` (a trace, in arrival order) placed by `arm` under `setup`.
#[must_use]
pub fn replay(jobs: &[Job], arm: Arm, setup: Setup) -> Outcome {
    let mut world = World::new(jobs, arm, setup);
    let order: Vec<(u64, usize)> = (0..jobs.len())
        .map(|index| (jobs[index].arrive_ms / setup.speed.max(1), index))
        .collect();
    world.run(&order, World::place);
    world.outcome
}

/// Replays `jobs` as they were placed: each starts when and on the host it started on the
/// farm, whatever any rule says, so the slowdown it is modelled to have met can be set
/// beside the contention it recorded.
#[must_use]
pub fn as_placed(jobs: &[Job], setup: Setup) -> Outcome {
    let arm = Arm {
        name: "as placed",
        bookings: Bookings::Recorded,
        rule: Rule::Booked,
        limit_permille: u64::MAX,
    };
    let mut world = World::new(jobs, arm, Setup { speed: 1, ..setup });
    let mut order: Vec<(u64, usize)> = (0..jobs.len())
        .map(|index| (jobs[index].arrive_ms + jobs[index].wait_ms, index))
        .collect();
    order.sort_unstable();
    world.run(&order, World::start_all);
    world.outcome
}

impl<'a> World<'a> {
    fn new(jobs: &'a [Job], arm: Arm, setup: Setup) -> Self {
        let farm = setup.farm;
        let sizer = Sizer::new(farm.milli, farm.bytes);
        Self {
            jobs,
            arm,
            setup,
            hosts: vec![Host::default(); farm.hosts],
            queue: VecDeque::new(),
            sizer: if arm.bookings == Bookings::Tailed {
                sizer.with_peaks()
            } else {
                sizer
            },
            spreads: BTreeMap::new(),
            histories: BTreeMap::new(),
            outcome: Outcome {
                slowed: vec![0; jobs.len()],
                waited: vec![0; jobs.len()],
                placed: vec![0; farm.hosts],
                ..Outcome::default()
            },
            dirty: false,
            curves: curve::Curves::default(),
        }
    }

    /// Runs until every job of `order` (each `(when it joins the queue, its index)`, in time
    /// order) has joined the queue and ended, placing with `place` at the start of each tick
    /// in which something could change a placement. Under the walk only the first waiting
    /// job decides a pass, so a job that joins behind it changes nothing; the job service
    /// places on a submit and on each heartbeat.
    fn run(&mut self, order: &[(u64, usize)], place: fn(&mut Self, u64)) {
        let mut next = 0;
        let mut now = 0;
        loop {
            while let Some(&(_, index)) = order.get(next).filter(|(at, _)| *at <= now) {
                self.dirty |= self.queue.is_empty();
                self.queue.push_back(index);
                next += 1;
            }
            if self.queue.is_empty() && self.hosts.iter().all(|host| host.runs.is_empty()) {
                let Some(&(at, _)) = order.get(next) else {
                    return;
                };
                // Nothing runs or waits: on to the tick of the next arrival, the hosts idle.
                now = at.div_ceil(TICK_MS) * TICK_MS;
                for host in &mut self.hosts {
                    *host = Host {
                        read_at: now,
                        ..Host::default()
                    };
                }
                continue;
            }
            if self.dirty {
                self.dirty = false;
                place(self, now);
            }
            self.tick(now);
            now += TICK_MS;
            if now % WINDOW_MS == 0 {
                self.dirty = true;
                for host in &mut self.hosts {
                    let reading = (host.used / WINDOW_MS as f64) as u64;
                    host.readings = [host.readings[1], reading];
                    host.used = 0.0;
                    host.read_at = now;
                }
            }
        }
    }

    /// The run `index` would be if it started at `now`.
    fn running(&self, index: usize, now: u64) -> Running {
        let jobs = self.jobs;
        let job = &jobs[index];
        let farm = self.setup.farm;
        let booking = match self.arm.bookings {
            Bookings::Recorded => Booking {
                milli: u64::from(job.booked_milli),
                bytes: u64::from(job.booked_mib) << 20,
            },
            Bookings::Learned | Bookings::Tailed | Bookings::Curve => {
                self.sizer.book(job, day_of(now))
            }
            Bookings::Decayed {
                half_life_ms,
                quantile_permille,
            } => {
                let share = farm.milli / 4 / 250 * 250;
                let learned = [Some(job.class), job.fallback, Some(job.client)]
                    .into_iter()
                    .flatten()
                    .find_map(|class| {
                        let past = self.histories.get(&class)?;
                        Some(decayed(past, now, half_life_ms, quantile_permille))
                    });
                let milli = learned.map_or(DEFAULT_COLD_CPU_MILLIS.min(share), |milli| {
                    milli.div_ceil(250) * 250
                });
                Booking {
                    milli: milli.min(share).max(250),
                    bytes: self.sizer.book(job, day_of(now)).bytes,
                }
            }
        };
        let run = shape::own_wall_ms(job.wall_ms, job.cpu_ms, farm.milli);
        // Its start holds its booking and asks for no CPU.
        let mut phases: Vec<Phase> = [Phase {
            ms: job.start_ms,
            milli: 0,
        }]
        .into_iter()
        .filter(|start| start.ms > 0)
        .collect();
        phases.extend(shape::phases(
            self.setup.shape,
            run,
            job.cpu_ms,
            job.peak_milli,
            farm.milli,
            spot(index),
        ));
        let var = [Some(job.class), job.fallback, Some(job.client)]
            .iter()
            .flatten()
            .find_map(|class| self.spreads.get(class))
            .map_or(0, Spread::var);
        Running {
            job: index,
            booking,
            curve: None,
            var,
            left: phases[0].ms as f64,
            phases,
            phase: 0,
            start: now,
            own: job.start_ms + run,
            run,
        }
    }

    /// The placement pass of the job service: the queue in order, each job to a host its
    /// rule admits it on, until a job no host takes. Of the hosts that admit it, the placer's
    /// score (`Scorer` in `js-sched/src/load.rs`) prefers, in order: the least figure past
    /// 101% of the cores (key 1), a host not measured above its bookings (key 3), and the
    /// host the job fits most tightly, the highest figure (key 5); the lower index on a tie.
    fn place(&mut self, now: u64) {
        let farm = self.setup.farm;
        let limit = farm.milli.saturating_mul(self.arm.limit_permille) / 1000;
        let tolerance = farm.milli + farm.milli / 100;
        while let Some(&index) = self.queue.front() {
            let mut new = self.running(index, now);
            self.curved(&mut new, now);
            let mut best: Option<(Rank, usize)> = None;
            for (at, host) in self.hosts.iter().enumerate() {
                if host.bytes + new.booking.bytes > farm.bytes {
                    continue;
                }
                let Some(figure) = host.figure(self.arm.rule, &new, limit) else {
                    continue;
                };
                let key = (
                    figure.saturating_sub(tolerance),
                    self.arm.rule != Rule::Unread && host.distrusted(),
                    Reverse(figure),
                );
                if best.as_ref().is_none_or(|(least, _)| key < *least) {
                    best = Some((key, at));
                }
            }
            // No host would take it even idle: once the farm is empty it takes an empty host
            // alone, as a booking of a whole host waits for one, rather than holding the walk
            // for ever.
            let alone = best.is_none() && self.stuck(&new, limit);
            self.outcome.alone += usize::from(alone);
            let Some((_, at)) = best.or(alone.then_some((ALONE, 0))) else {
                self.stall(&new, now);
                return;
            };
            self.queue.pop_front();
            self.start(at, new, now);
        }
    }

    /// Whether the farm is empty and still no host would take `new`: its memory is more than
    /// a host's, or the rule refuses it on an empty, unread host.
    fn stuck(&self, new: &Running, limit: u64) -> bool {
        let empty = self.hosts.iter().all(|host| host.runs.is_empty());
        let too_big = new.booking.bytes > self.setup.farm.bytes;
        let refused = Host::default().figure(self.arm.rule, new, limit).is_none();
        empty & (too_big | refused)
    }

    /// Starts every queued job on the host it ran on.
    fn start_all(&mut self, now: u64) {
        while let Some(index) = self.queue.pop_front() {
            let new = self.running(index, now);
            let at = usize::from(self.jobs[index].host) % self.hosts.len();
            self.start(at, new, now);
        }
    }

    /// Starts `new` on host `at` at `now`.
    fn start(&mut self, at: usize, new: Running, now: u64) {
        let jobs = self.jobs;
        let arrived = jobs[new.job].arrive_ms / self.setup.speed.max(1);
        let wait = now.saturating_sub(arrived);
        self.outcome.waits.push(wait);
        self.outcome.waited[new.job] = wait;
        self.curves.placed();
        let host = &mut self.hosts[at];
        host.bytes += new.booking.bytes;
        host.runs.push(new);
        let booked = host.runs.iter().map(|run| run.booking.milli).sum::<u64>();
        self.outcome.booked_peak = self.outcome.booked_peak.max(booked);
        self.outcome.placed[at] += 1;
    }

    /// Runs every host for the tick that starts at `now`.
    fn tick(&mut self, now: u64) {
        let cores = self.setup.farm.milli;
        let mut ended = Vec::new();
        for host in &mut self.hosts {
            let asked: u64 = host.runs.iter().map(Running::demand).sum();
            let planned = host.planned(self.arm.rule);
            let outcome = &mut self.outcome;
            outcome.held_ms += host.runs.len() as u128 * u128::from(TICK_MS);
            outcome.plan_error += u128::from(planned.abs_diff(asked)) * u128::from(TICK_MS);
            if asked > planned {
                outcome.under_ms += TICK_MS;
            }
            if asked > cores {
                outcome.over_ms += TICK_MS;
                outcome.over_excess += u128::from(asked - cores) * u128::from(TICK_MS);
            }
            let rate = if asked > cores {
                cores as f64 / asked as f64
            } else {
                1.0
            };
            let mut at = 0;
            while at < host.runs.len() {
                let (got, end) = host.runs[at].step(rate, TICK_MS as f64);
                host.used += got;
                outcome.used += got;
                match end {
                    Some(ms) => {
                        let run = host.runs.swap_remove(at);
                        host.bytes -= run.booking.bytes;
                        ended.push((run, now + ms));
                    }
                    None => at += 1,
                }
            }
        }
        for (run, end) in ended {
            self.dirty = true;
            self.end(run, end);
        }
    }

    /// Records the end of `run` at `end` and teaches its class.
    fn end(&mut self, run: Running, end: u64) {
        let jobs = self.jobs;
        let job = &jobs[run.job];
        let wall = end - run.start;
        let slowed = wall.saturating_sub(run.own);
        let outcome = &mut self.outcome;
        outcome.slowed[run.job] = slowed;
        outcome.last_end_ms = outcome.last_end_ms.max(end);
        outcome.own_ms += run.own;
        outcome.slowed_ms += slowed;
        let booked = run.booked(wall);
        let used = u128::from(job.cpu_ms) * 1000;
        outcome.booked += booked;
        outcome.above_booking += used.saturating_sub(booked);
        let ran = wall.saturating_sub(job.start_ms);
        self.sizer
            .learn(job, (ran, slowed), run.booking, day_of(end));
        if job.end == End::Ok && run.run >= TICK_MS {
            self.teach_curve(&run, end, slowed);
            let mean = job.cpu_ms * 1000 / run.run;
            let host = self.setup.farm.milli.max(mean);
            let peak = job.peak_milli.map_or(mean, u64::from).clamp(mean, host);
            for class in [Some(job.class), job.fallback, Some(job.client)]
                .into_iter()
                .flatten()
            {
                let spread = self.spreads.entry(class).or_default();
                if spread.runs.len() == RING {
                    spread.runs.pop_front();
                }
                spread.runs.push_back((mean, peak));
                let history = self.histories.entry(class).or_default();
                if history.len() == HISTORY {
                    history.pop_front();
                }
                history.push_back(Past {
                    end,
                    mean,
                    cpu: job.cpu_ms,
                });
            }
        }
    }
}

/// Where in a run its burst falls, from 0 to 999, fixed for a trace index and spread
/// evenly over the indices.
fn spot(index: usize) -> u64 {
    ((index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) % 1000
}

#[cfg(test)]
mod tail_tests;
#[cfg(test)]
mod tests;
