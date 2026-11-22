//! A server opens its job store over a real Postgres store while another server changes jobs
//! without a pause, as the old instance of a start-first update does on a busy farm.
//!
//! Nothing here waits on time. The opener's store reads its clock once at the start of every
//! call, before it takes a connection or a lock (`PostgresStore::with_clock`), so the clock is
//! where the serving server's writes are put: each reading first makes the serving server move
//! one job one step through its life (place it, start it, run it, end it, or queue a new one in
//! place of one that ended), and then answers the shared manual clock. Between any two calls of
//! the open, a job has changed. The writer never pauses, and a count bounds it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_jobs::interfere::{Interfere, Read};
use js_jobs::{Booked, JobEnd, JobId, JobState, JobStore, NewJob, Retention};
use js_state::postgres::{PostgresConfig, PostgresStore, test_database};
use js_state::{Clock, ManualClock, Store, StoreTime};
use js_types::HostId;

use super::{JobSources, JobStoreSetup, ServerIds, ServerJobs, open};
use crate::agent_api::job_rig::spec;
use crate::config::FeedRetention;

/// The jobs the serving server holds when the second server starts.
const JOBS: usize = 60;

/// The most changes the serving server makes: far more than the open needs calls, and few
/// enough that an open which never finishes while jobs change is seen to run out of them.
const STEPS: usize = 600;

/// What one placement books.
fn booked() -> Booked {
    Booked::Shape {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        disk_bytes: 0,
        accelerators: 0,
    }
}

/// The serving server: its own job store over its own connections to the database, and the
/// jobs it holds that have not ended, with their states.
struct Serving {
    jobs: ServerJobs,
    /// The jobs that have not ended, by id.
    live: BTreeMap<JobId, JobState>,
    /// How many changes it may still make.
    left: usize,
    /// How many changes it made, each committed at its first try.
    made: usize,
    /// How many jobs it has queued.
    queued: usize,
}

impl Serving {
    /// Queues a new job.
    fn queue(&mut self) {
        let id = JobId::parse(&format!("busy-{:05}", self.queued)).expect("an id");
        self.queued += 1;
        let new = NewJob {
            id: Some(id.clone()),
            spec: spec(),
            priority: 0,
            client: None,
        };
        self.jobs
            .insert_queued(new)
            .expect("the serving server queues a job");
        self.live.insert(id, JobState::Queued);
    }

    /// Moves its oldest live job (the lowest id, the first the open reads) one step: queued to
    /// placed, placed to starting, starting to running, running to ended; a job that ends is
    /// replaced by a new queued one, so the farm stays as busy. Each change is one commit, and
    /// must hold at its first try: the opener never makes the serving server lose a write.
    fn step(&mut self) {
        let (id, state) = self.live.pop_first().expect("a live job");
        let host = HostId::new("busy-host").expect("a host id");
        let (moved, now) = match state {
            JobState::Queued => (
                self.jobs.place(&id, host, booked()).map(drop),
                JobState::Placed,
            ),
            JobState::Placed => {
                let to = JobState::Starting;
                (self.jobs.transition(&id, state, to).map(drop), to)
            }
            JobState::Starting => {
                let to = JobState::Running;
                (self.jobs.transition(&id, state, to).map(drop), to)
            }
            _ => (
                self.jobs.end_job(&id, JobEnd::Lost).map(drop),
                JobState::Lost,
            ),
        };
        moved.expect("the serving server's change commits at its first try");
        self.made += 1;
        if now.is_ended() {
            self.queue();
        } else {
            self.live.insert(id, now);
        }
    }
}

/// The opener's store clock: each reading first makes the serving server change one job, while
/// it has changes left, then answers `clock`.
struct Busy {
    clock: Arc<ManualClock>,
    serving: Mutex<Serving>,
}

impl Clock for Busy {
    fn now(&self) -> StoreTime {
        let mut serving = self.serving.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(left) = serving.left.checked_sub(1) {
            serving.left = left;
            serving.step();
        }
        self.clock.now()
    }
}

/// Asserts that a server opens its job store over a real Postgres store while the serving
/// server changes a job before every call the open makes (a writer that never pauses, bounded
/// by [`STEPS`]): the open finishes with changes still left to the writer, so it did not need
/// the writer to stop; every change of the serving server committed at its first try, one per
/// call of the open, so its pass kept its pace and lost nothing to the open; the open scanned
/// the job table once; and every job that was waiting before the boot and still waits has its
/// deadline floor of the boot plus the grace. It catches an open that starts over each time a
/// job changes under it (the second server never turns ready on a busy farm, and reads the
/// whole store again every few seconds beside the serving one).
#[test]
fn a_server_opens_its_job_store_while_another_changes_jobs_without_a_pause() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the test URL parses");
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(3600));
    let tables: Arc<dyn Store> = Arc::new(
        PostgresStore::open(&config)
            .expect("the serving server's store opens")
            .with_clock(clock.clone()),
    );
    let jobs = JobStore::new(Arc::new(tables), ServerIds::random())
        .expect("the serving server's job store opens")
        .with_retention(Retention::new(Duration::from_secs(60)).expect("a window"));
    let mut serving = Serving {
        jobs,
        live: BTreeMap::new(),
        left: STEPS,
        made: 0,
        queued: 0,
    };
    for _ in 0..JOBS {
        serving.queue();
    }
    // A first stretch of work, so the open meets jobs in every state: ended ones (whose
    // retention end the raise moves), a running one, and waiting ones.
    for _ in 0..JOBS / 2 {
        serving.step();
    }
    let before = serving.made;
    let at_boot: Vec<(JobId, JobState)> = (0..serving.queued)
        .map(|n| JobId::parse(&format!("busy-{n:05}")).expect("an id"))
        .map(|id| {
            let job = serving.jobs.get_job(&id).expect("reads").expect("present");
            (id, job.state)
        })
        .collect();

    let busy = Arc::new(Busy {
        clock: clock.clone(),
        serving: Mutex::new(serving),
    });
    let opener = Arc::new(Interfere::new(
        PostgresStore::open(&config)
            .expect("the opener's store opens")
            .with_clock(busy.clone()),
    ));
    let setup = JobStoreSetup {
        flag: "--job-listen",
        retention: Retention::DEFAULT,
        feed_retention: FeedRetention::DEFAULT,
        allocation_retention: Retention::ALLOCATION_DEFAULT,
    };
    let boot = clock.now();
    let kept: Arc<dyn Store> = opener.clone();
    let opened = open(
        (std::path::Path::new("unused/leases.toml"), Some(kept)),
        setup,
        &JobSources::default(),
    )
    .expect("the job store opens while the serving server changes jobs");

    // The writer stops here: the checks below read through the opener's store, whose clock
    // would otherwise change jobs under them.
    let (left, made) = {
        let mut serving = busy.serving.lock().unwrap_or_else(PoisonError::into_inner);
        (std::mem::take(&mut serving.left), serving.made)
    };
    assert!(
        left > 0,
        "the open needed the serving server to stop: all {} changes made before it finished",
        made - before
    );
    let whole = Read::Scan(js_jobs::keys::JOB.to_owned());
    let reads = opener.take_reads();
    let scans = reads.iter().filter(|read| **read == whole).count();
    assert_eq!(scans, 1, "the open scanned the job table {scans} times");

    // Each job waiting at the boot that waits still has the boot's floor, and each job ended at
    // the boot has its retention end raised to the same instant.
    let floor = Some(boot.saturating_add(js_jobs::BOOT_GRACE));
    let waiting = [JobState::Queued, JobState::Placed, JobState::Starting];
    let now: Vec<(js_jobs::JobRecord, JobState)> = at_boot
        .into_iter()
        .map(|(id, then)| {
            let job = opened.jobs.get_job(&id).expect("reads").expect("present");
            (job, then)
        })
        .collect();
    let still_waiting: Vec<(JobId, Option<StoreTime>)> = now
        .iter()
        .filter(|(job, then)| waiting.contains(then) & waiting.contains(&job.state))
        .map(|(job, _)| (job.id.clone(), job.deadline_floor))
        .collect();
    let ended: Vec<(JobId, Option<StoreTime>)> = now
        .iter()
        .filter(|(_, then)| then.is_ended())
        .map(|(job, _)| (job.id.clone(), job.retention_ends))
        .collect();
    assert!(
        !still_waiting.is_empty() & !ended.is_empty(),
        "no waiting or no ended job to judge: {still_waiting:?} {ended:?}"
    );
    let raised = |rows: &[(JobId, Option<StoreTime>)]| -> Vec<(JobId, Option<StoreTime>)> {
        rows.iter().map(|(id, _)| (id.clone(), floor)).collect()
    };
    assert_eq!(
        still_waiting,
        raised(&still_waiting),
        "waiting jobs without the floor"
    );
    assert_eq!(
        ended,
        raised(&ended),
        "ended jobs without the raised retention end"
    );

    // The checks read the clock with no change left, and changed nothing.
    let serving = busy.serving.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        serving.made, made,
        "a reading with no change left changed a job"
    );
}
