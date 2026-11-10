//! The scale family: how the work the server is asked for grows with the number of hosts
//! and jobs. One scenario, run at two sizes: [`FULL`], 2,000 hosts and 20,000 jobs (a
//! nightly run, `.github/workflows/sim-sweep.yml`), and [`SMALL`], fifty hosts and five
//! hundred jobs, which every pull request runs, so the counts and the bounds on them are
//! covered by the pull request's `test` and `coverage` jobs.
//!
//! The hosts are [`Shape::cpu_host`] hosts (8 cores) beating every [`INTERVAL`], held lost
//! after [`GRACE`]; each job asks for [`JOB_CPU_MILLIS`] (a quarter of a host) and runs
//! [`RUN`], so a host runs four at a time and the jobs run in several waves (2.5 of them at
//! full size). Jobs are submitted in batches of [`Size::batch`] every [`SUBMIT_EVERY`], a
//! rate under what the hosts take, so the queue stays short: the server's cost of a
//! placement grows with the hosts, and a queue of all the jobs at once would measure the
//! queue's depth rather than the hosts'.
//!
//! The cell counts what it can see from outside the server ([`Load`]): the heartbeats and
//! hellos the server answered, the calls the client made, and the changes it read in the
//! job tables. The server's own work has no public count (see `Load`), so each bound below
//! is on a count that bounds that work from above, and is derived from the algorithm, not
//! from a measurement, with the headroom stated:
//!
//! - hellos: one a host, none again (no host is lost, no server restarts): at most `hosts`.
//! - heartbeats (and with them the engine's placement passes, at most one a heartbeat): a
//!   host beats every [`INTERVAL`] from its hello, so at most `hosts * (t / INTERVAL + 2)`
//!   by virtual time `t`. Catches a heartbeat that comes faster than the interval it was
//!   told, or a host that beats twice.
//! - job-table changes (and with them the store commits that wrote the rows): a job is
//!   read queued, placed, starting, running, succeeded: at most five changes each. Catches
//!   a job that is written again and again (a retry loop, a re-placement).
//! - allocation-table changes: booked, releasing, freed: at most three a job; and exactly
//!   one allocation a job, each with an id of its own (so the id script, [`SCRIPT_LEN`] ids,
//!   never wrapped under 20,000 placements). Catches a job placed twice.
//! - client reads: a read a poll of the job, [`POLL_EVERY`] apart, while it is open, and
//!   one of its result; a job is open at most [`LIFE`] (a start waits for at most two
//!   heartbeats, the run, and the end is told at the next heartbeat or two; the rest is
//!   headroom for the queue), so at most `1 + LIFE / POLL_EVERY + 1` a job. Catches a
//!   client call, or a server answer that asks for a call, per host per job.
//! - no job is failed to start.
//!
//! Size, in agent requests a seed: [`SMALL`] beats 50 hosts about 30 times (1,500, with
//! the hellos); [`FULL`] beats 2,000 hosts about 30 times (about 60,000).
//!
//! The negative case, [`a_per_host_call_for_each_job_passes_the_read_bound`], runs a
//! client that makes a call for each host for each job, and shows the read bound is
//! what fails.

use std::cell::Cell as Flag;
use std::rc::Rc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::http::header::AUTHORIZATION;
use js_sim_cell::actions::submit_work;
use js_sim_cell::front::POLL_EVERY;
use js_sim_cell::server::{CLIENT_TOKEN, SCRIPT_LEN};
use js_sim_cell::{
    Answer, CallKind, Cell, ClientEvent, ClientPort, Expect, Front, FrontFactory, ItemId,
    JOB_PREFIX, JobFront, Load, Scenario, ScenarioFailure, ServerSpec, Shape, Work,
};
use js_types::Resources;
use js_types::sched::FarmTime;

/// The heartbeat interval and grace of every host.
const INTERVAL: Duration = Duration::from_secs(30);
const GRACE: Duration = Duration::from_secs(90);

/// What a job asks for: a quarter of a host's cores, so a host runs four.
const JOB_CPU_MILLIS: u64 = 2_000;
const RUN: Duration = Duration::from_secs(60);
const GIVE_UP: Duration = Duration::from_secs(6_000);

/// How often a batch of jobs is submitted.
const SUBMIT_EVERY: Duration = Duration::from_secs(2);

/// How long a job is open, at most, before the client reads its result: it is placed by the
/// pass after a heartbeat (up to 30 s), its start reaches the host on the answer to the
/// next heartbeat (30 s), the run takes 60 s, and the end is told at the next heartbeat (up
/// to 30 s, and 30 s more when the start came late in the beat): 180 s, with 120 s of
/// headroom for a short queue. A run measures about 37 reads a job against the 62 allowed.
const LIFE: Duration = Duration::from_secs(300);

/// The size of one run.
#[derive(Clone, Copy, Debug)]
struct Size {
    hosts: usize,
    jobs: u32,
    /// Jobs submitted every [`SUBMIT_EVERY`].
    batch: u32,
    /// Virtual time to run after the last submit.
    settle: Duration,
}

/// Fifty hosts and five hundred jobs: the pull request's run.
const SMALL: Size = Size {
    hosts: 50,
    jobs: 500,
    batch: 3,
    settle: Duration::from_secs(300),
};

/// Two thousand hosts and twenty thousand jobs: the nightly run.
const FULL: Size = Size {
    hosts: 2_000,
    jobs: 20_000,
    batch: 60,
    settle: Duration::from_secs(300),
};

/// Sixty hosts and as many jobs, for the negative case: with a call for each host for each
/// job, a job makes more than the bound's reads.
const NEGATIVE: Size = Size {
    hosts: 60,
    jobs: 60,
    batch: 6,
    settle: Duration::from_secs(300),
};

/// The most client reads a job makes, as [`bounds`] derives it.
fn reads_a_job() -> u64 {
    1 + LIFE.as_secs() / POLL_EVERY.as_secs() + 1
}

/// What `load` shows past its bounds at virtual time `now` for a run of `size`, or `None`.
fn bounds(size: Size, load: Load, now: FarmTime) -> Option<String> {
    let jobs = u64::from(size.jobs);
    let hosts = size.hosts as u64;
    let beats = hosts * (now.as_millis() / 1_000 / INTERVAL.as_secs() + 2);
    let checks = [
        ("hellos", load.hellos, hosts),
        ("heartbeats", load.heartbeats, beats),
        ("job-changes", load.job_changes, 5 * jobs),
        ("allocation-changes", load.allocation_changes, 3 * jobs),
        ("reads", load.reads, jobs * reads_a_job()),
        ("failed-starts", load.failed_starts, 0),
    ];
    let over = checks.iter().find(|(_, got, most)| got > most);
    let over = over.map(|(name, got, most)| format!("{name} {got} is over {most}"));
    over.or_else(|| {
        (load.allocations != jobs)
            .then(|| format!("allocations {} is not {jobs}", load.allocations))
    })
}

/// A front of the job service that, for each job, makes a read of it for each host as well:
/// the cost a client would add that grows with the hosts. It is for the negative case only.
struct PerHostReads {
    inner: JobFront,
    hosts: usize,
    /// Items submitted and not yet read for every host.
    fresh: Vec<ItemId>,
    /// The answers, the inner front's and these.
    all: Vec<Answer>,
    /// How many of the inner front's answers are in `all`.
    copied: usize,
}

impl Front for PerHostReads {
    fn submit(&mut self, item: ItemId, work: &Work, now: FarmTime) {
        self.fresh.push(item);
        self.inner.submit(item, work, now);
    }

    fn cancel(&mut self, item: ItemId, now: FarmTime) {
        self.inner.cancel(item, now);
    }

    fn next_poll(&self) -> Option<FarmTime> {
        self.inner.next_poll()
    }

    fn poll(&mut self, now: FarmTime, server: &mut dyn ClientPort) -> Vec<(ItemId, ClientEvent)> {
        let heard = self.inner.poll(now, server);
        self.all
            .extend_from_slice(&self.inner.answers()[self.copied..]);
        self.copied = self.inner.answers().len();
        for item in std::mem::take(&mut self.fresh) {
            for _ in 0..self.hosts {
                let request = Request::builder()
                    .method("GET")
                    .uri(format!("/v1/jobs/{JOB_PREFIX}{}", item.0))
                    .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
                    .body(Body::empty())
                    .unwrap_or_default();
                let answer = server.call(request);
                self.all.push(Answer {
                    item,
                    kind: CallKind::Read,
                    at: now,
                    status: answer.status().as_u16(),
                    body: String::new(),
                });
            }
        }
        heard
    }

    fn answers(&self) -> &[Answer] {
        &self.all
    }
}

/// The scenario of `size`: `size.jobs` jobs, submitted in batches, all run to the end, under
/// the invariants and the [`bounds`]. `front` is the client, `None` for the job front;
/// `seen` is given the run's counts when it ends.
fn scenario(size: Size, front: Option<FrontFactory>, seen: &Rc<Flag<Load>>) -> Scenario {
    let spec = ServerSpec::registered_pool("burst", Shape::cpu_host(), size.hosts)
        .heartbeat(INTERVAL, GRACE);
    let needs = Resources::new(JOB_CPU_MILLIS, 1 << 20, 0, None).unwrap_or_default();
    let work = Work::run_for(RUN).needs(needs).give_up_after(GIVE_UP);
    let batches = size.jobs.div_ceil(size.batch);
    let mut scenario = Scenario::new("scale")
        .scratch(env!("CARGO_TARGET_TMPDIR"))
        .server(spec)
        .hosts("burst", size.hosts);
    if let Some(front) = front {
        scenario = scenario.front(front);
    }
    for batch in 0..batches {
        let count = size.batch.min(size.jobs - batch * size.batch);
        let at = SUBMIT_EVERY * batch;
        scenario = scenario.at(at, submit_work(count, work.clone()));
    }
    let seen = Rc::clone(seen);
    scenario
        .expect(Expect::all_items_terminal())
        .expect(Expect::jobs_read(size.jobs as usize))
        .expect(Expect::jobs_ended(size.jobs as usize))
        .expect(Expect::each_job_launched_once())
        .expect(Expect::no_allocation_live())
        .expect_that("bounds", move |cell: &mut Cell| {
            let load = Load::of(cell);
            seen.set(load);
            bounds(size, load, cell.now())
        })
        .settle_within(size.settle)
}

/// Covers the scale of the server's work, over fifty hosts and five hundred jobs (the
/// pull request's size of [`a_run_of_two_thousand_hosts_and_twenty_thousand_jobs_stays_in_its_bounds`]):
/// every job reaches a terminal state, is launched once and holds one allocation of its
/// own, and the counts of the run stay under the bounds derived from the algorithm
/// ([`bounds`]). Catches: work per job or per host that grows (a read, a write, or a
/// heartbeat more than the algorithm makes), and an id script that repeats.
#[test]
fn a_run_of_fifty_hosts_and_five_hundred_jobs_stays_in_its_bounds() {
    let seen = Rc::new(Flag::new(Load::default()));
    scenario(SMALL, None, &seen).run_seeds(1);
    let load = seen.get();
    assert_eq!(load.hellos, 50);
    assert!(load.submits >= 500);
    assert_eq!(load.allocations, 500);
    assert_eq!(load.cancels, 0);
    assert!(load.heartbeats > 0 && load.client_calls() > load.submits);
}

/// Covers the same bounds over two thousand hosts and twenty thousand jobs, run to
/// completion on one seed. It is a nightly run: it takes about an hour of CPU, so it is
/// not in the pull request's set (`.github/workflows/sim-sweep.yml` runs it, by name).
/// Catches: as [`a_run_of_fifty_hosts_and_five_hundred_jobs_stays_in_its_bounds`], at a size
/// where a cost per host per job is what the run is made of, and an id script shorter than
/// the placements.
#[test]
#[ignore = "an hour of CPU: runs nightly in .github/workflows/sim-sweep.yml (job scale)"]
fn a_run_of_two_thousand_hosts_and_twenty_thousand_jobs_stays_in_its_bounds() {
    assert!(SCRIPT_LEN > FULL.jobs as usize);
    let seen = Rc::new(Flag::new(Load::default()));
    scenario(FULL, None, &seen).run_seeds(1);
    assert_eq!(seen.get().allocations, 20_000);
}

/// Covers the bound on reads: a client that reads each job once for each host as well
/// makes `jobs * hosts` reads more than the job front does, which is past the bound of
/// [`reads_a_job`] reads a job, so the run fails on `reads` and nothing else, and the
/// count it reports is the honest run's plus a read for each host for each job. Catches: a
/// bound too loose to see a cost that grows with the hosts.
#[test]
fn a_per_host_call_for_each_job_passes_the_read_bound() {
    let honest = Rc::new(Flag::new(Load::default()));
    scenario(NEGATIVE, None, &honest).run_seeds(1);
    let hosts = NEGATIVE.hosts;
    let front = FrontFactory::new(move |_| {
        Box::new(PerHostReads {
            inner: JobFront::new(),
            hosts,
            fresh: Vec::new(),
            all: Vec::new(),
            copied: 0,
        })
    });
    let seen = Rc::new(Flag::new(Load::default()));
    let failure = scenario(NEGATIVE, Some(front), &seen)
        .try_run_seeds(1)
        .unwrap_err();
    let reads = honest.get().reads + u64::from(NEGATIVE.jobs) * hosts as u64;
    let most = u64::from(NEGATIVE.jobs) * reads_a_job();
    let ScenarioFailure::Unmet { detail, seed, .. } = failure else {
        panic!("the run failed otherwise than on a bound");
    };
    assert_eq!(seed, 0);
    assert_eq!(detail, format!("bounds: reads {reads} is over {most}"));
    assert_eq!(seen.get().reads, reads);
}

/// Covers the bound itself on counts a run cannot reach: each count past its limit is
/// named, the allocations that are not one a job are named, and a run inside every limit
/// is none. Catches: a bound that names the wrong count, or one that never fails.
#[test]
fn the_bounds_name_the_count_that_is_over() {
    let size = Size {
        hosts: 2,
        jobs: 3,
        batch: 1,
        settle: Duration::ZERO,
    };
    let at = FarmTime::from_millis(60_000);
    let fine = Load {
        hellos: 2,
        heartbeats: 8,
        job_changes: 15,
        allocation_changes: 9,
        allocations: 3,
        reads: 3 * reads_a_job(),
        ..Load::default()
    };
    assert_eq!(bounds(size, fine, at), None);
    let over = |load: Load| bounds(size, load, at);
    let beats = Load {
        heartbeats: 9,
        ..fine
    };
    assert_eq!(over(beats).as_deref(), Some("heartbeats 9 is over 8"));
    let hellos = Load { hellos: 3, ..fine };
    assert_eq!(over(hellos).as_deref(), Some("hellos 3 is over 2"));
    let jobs = Load {
        job_changes: 16,
        ..fine
    };
    assert_eq!(over(jobs).as_deref(), Some("job-changes 16 is over 15"));
    let rows = Load {
        allocation_changes: 10,
        ..fine
    };
    assert_eq!(
        over(rows).as_deref(),
        Some("allocation-changes 10 is over 9")
    );
    let failed = Load {
        failed_starts: 1,
        ..fine
    };
    assert_eq!(over(failed).as_deref(), Some("failed-starts 1 is over 0"));
    let few = Load {
        allocations: 2,
        ..fine
    };
    assert_eq!(over(few).as_deref(), Some("allocations 2 is not 3"));
}
