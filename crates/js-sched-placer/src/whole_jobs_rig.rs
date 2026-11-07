//! What the whole-host job tests share: the job tables on the in-memory fake behind the
//! wrapper that fails or races a commit, hosts and an engine over them, and the settings of
//! the passes. Every clock is the test's: milliseconds passed by hand.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use js_jobs::interfere::Interfere;
use js_jobs::{
    Constraints, JobFiles, JobId, JobRecord, JobSpec, JobStore, MarkRecord, Needs, NetworkPolicy,
    NewJob, RandomIds,
};
use js_registry::WholeNodeConfig;
use js_state::fake::{Fake, Flaw};
use js_state::{Clock, ManualClock as StoreClock, StoreTime};
use js_types::sched::FarmTime;
use js_types::{Candidate, Held};
use js_types::{HostId, ImageDigestRef, Labels, LeaseId, Resources, Shape};

use super::whole_jobs::{WholeJobStep, WholeJobs};
use super::{Engine, Settings};

/// `ms` milliseconds on the scheduler's clock.
pub(crate) fn at(ms: u64) -> FarmTime {
    FarmTime::from_millis(ms)
}

/// `ms` milliseconds on the store's clock.
pub(crate) fn store(ms: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_millis(ms))
}

/// The host `name`.
pub(crate) fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// The hosts `names`.
pub(crate) fn hosts(names: &[&str]) -> BTreeSet<HostId> {
    names.iter().map(|name| host(name)).collect()
}

/// A host of 8 cores and 16 GiB named `name`, carrying the one-core leases `1-<seq>` of
/// `seqs`.
pub(crate) fn candidate(name: &str, seqs: &[u64]) -> Candidate {
    let caps = [("arch".to_owned(), "x86_64".to_owned())]
        .into_iter()
        .collect();
    let shape = Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps);
    let leases = seqs.iter().map(|seq| Held {
        lease: LeaseId::new(1, *seq),
        resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
        whole: false,
    });
    Candidate {
        host: host(name),
        shape,
        leases: leases.collect(),
    }
}

/// The hosts `hosts`, each with the leases given.
pub(crate) fn every(hosts: &[(&str, &[u64])]) -> Vec<Candidate> {
    let every = hosts.iter().map(|(name, seqs)| candidate(name, seqs));
    every.collect()
}

/// An engine built at zero over `hosts`, each with the leases given.
pub(crate) fn engine(hosts: &[(&str, &[u64])]) -> Engine {
    Engine::build(Settings::default(), &every(hosts), at(0))
}

/// The settings of the passes: a job waits ten seconds before it marks a busy host, and a
/// mark's deadline is a minute after it is made (reaching it gives nothing up).
pub(crate) fn settings() -> WholeNodeConfig {
    WholeNodeConfig {
        drain_after_ms: 10_000,
        drain_deadline_ms: Some(60_000),
        ..WholeNodeConfig::dedicated(4)
    }
}

/// The spec of a job that asks for `needs` and may run `wall_limit_secs`.
pub(crate) fn spec_of(needs: Needs, wall_limit_secs: u64) -> JobSpec {
    JobSpec {
        image: ImageDigestRef::parse(&format!("team/app@sha256:{}", "a".repeat(64))).unwrap(),
        command: vec!["run".to_owned()],
        args: Vec::new(),
        env: BTreeMap::new(),
        needs,
        constraints: Constraints::default(),
        wall_limit_secs,
        network: NetworkPolicy::None,
        ports: Vec::new(),
        files: JobFiles::default(),
        labels: Labels::default(),
    }
}

/// A mark of `job` on `host`, made at `made_ms`, with its deadline at `deadline_ms`, ending
/// its tail at `tail_ms`, with no exclusion.
pub(crate) fn mark(
    job: &JobRecord,
    host_name: &str,
    (made_ms, deadline_ms): (u64, u64),
    tail_ms: Option<u64>,
) -> MarkRecord {
    let mut mark = MarkRecord::new(
        job.id.clone(),
        host(host_name),
        store(made_ms),
        store(deadline_ms),
    );
    mark.tail_end = tail_ms.map(store);
    mark
}

/// The job tables over the in-memory fake, behind the wrapper that fails or races a commit.
pub(crate) struct Tables {
    /// The store's clock.
    pub(crate) clock: Arc<StoreClock>,
    /// The wrapped store.
    pub(crate) store: Arc<Interfere<Fake>>,
    /// The job tables.
    pub(crate) jobs: JobStore<Interfere<Fake>>,
}

impl Tables {
    /// Empty tables on a clock at zero.
    pub(crate) fn new() -> Self {
        let clock = Arc::new(StoreClock::default());
        let fake = Fake::with_flaw(Arc::clone(&clock), Flaw::None);
        let store = Arc::new(Interfere::new(fake));
        let jobs = JobStore::new(Arc::clone(&store), RandomIds).unwrap();
        Self { clock, store, jobs }
    }

    /// Moves the store's clock to `ms` milliseconds (never back).
    pub(crate) fn set_ms(&self, ms: u64) {
        let now = self.clock.now().as_duration().as_millis();
        let by = u128::from(ms).saturating_sub(now);
        self.clock
            .advance(Duration::from_millis(u64::try_from(by).unwrap()));
    }

    /// Records the queued job `id` that asks for `needs` and may run `wall_limit_secs`, at the
    /// store's time now.
    pub(crate) fn submit_with(&self, id: &str, needs: Needs, wall_limit_secs: u64) -> JobRecord {
        let new = NewJob {
            id: Some(JobId::parse(id).unwrap()),
            spec: spec_of(needs, wall_limit_secs),
            priority: 0,
            client: None,
        };
        self.jobs.insert_queued(new).unwrap().record
    }

    /// Records the queued job `id` that needs the whole host, with a wall limit of ten minutes.
    pub(crate) fn whole(&self, id: &str) -> JobRecord {
        self.submit_with(id, Needs::WholeHost, 600)
    }

    /// Records the queued job `id` that asks for one core and 1 GiB, with a wall limit of ten
    /// minutes.
    pub(crate) fn share(&self, id: &str) -> JobRecord {
        let resources = Resources::new(1000, 1 << 30, 0, None).unwrap();
        self.submit_with(id, Needs::Shape(resources), 600)
    }
}

/// Every host of `names` heard from at `now_ms`, as their heartbeats are.
pub(crate) fn beat(engine: &mut Engine, names: &[&str], now_ms: u64) {
    for name in names {
        engine.heard(&host(name), None, at(now_ms));
    }
}

/// A pass of `engine` at `now_ms` over `queued` and `marks`: the hosts `a` and `b` have said
/// hello and may take a job, the pool has `settings()`, and a mark waits for its host's hello
/// until 15 s. The hosts are heard from first, as a pass follows their heartbeats.
pub(crate) fn run(
    engine: &mut Engine,
    (queued, marks): (&[JobRecord], &[MarkRecord]),
    now_ms: u64,
) -> Vec<WholeJobStep> {
    let both = hosts(&["a", "b"]);
    run_with(engine, (queued, marks), (&both, &both), now_ms)
}

/// As [`run`], for the hosts that said hello and those that may take a job given.
pub(crate) fn run_with(
    engine: &mut Engine,
    (queued, marks): (&[JobRecord], &[MarkRecord]),
    (hello, open): (&BTreeSet<HostId>, &BTreeSet<HostId>),
    now_ms: u64,
) -> Vec<WholeJobStep> {
    beat(engine, &["a", "b"], now_ms);
    let settings = settings();
    let input = WholeJobs {
        queued,
        marks,
        hello,
        open,
        settings: Some(&settings),
        claim_by: at(15_000),
    };
    engine.whole_jobs(&input, at(now_ms))
}
