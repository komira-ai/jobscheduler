//! The real [`Effects`]: the server's registry, job tables and join tokens, the pool's
//! provider and network mode, and where the scaler's records are kept (the files, or the
//! state store).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, CapacityProvider, Demand, EventStream, Held, HostName, HostRef, Offer,
    ProviderError, Released, SchedulerId, Secret,
};
use js_jobs::JobStoreError;
use js_registry::{Busy, HostView};
use js_scaler::{AcquirePlan, Health, Records};
use js_types::{HostId, ProviderId};
use tracing::warn;

use super::kept::ScalerKeeper;
use super::{Call, Effects, JobTables, Launcher};
use crate::agents::Agents;
use crate::checks::Health as Checks;
use crate::cordon::DEFAULT_CORDON;

/// The pool's side of the server, as the shell uses it.
pub(crate) struct Live {
    agents: Arc<Agents>,
    pool: ProviderId,
    scheduler: SchedulerId,
    provider: Arc<dyn CapacityProvider>,
    launcher: Launcher,
    /// Where the records and the hosts the registry would not remove are saved.
    kept: ScalerKeeper,
    checks: Arc<Mutex<Checks>>,
    /// What a job that books the whole host asks a machine to bring: the pool's floor.
    whole_host: Demand,
}

impl Live {
    /// The effects for the pool `pool` of `scheduler`: its machines are launched and
    /// released through `provider` and `launcher`, its tokens, hosts and job tables are
    /// `agents`', its records and the hosts the registry would not remove are kept by `kept`,
    /// its health and offers are what its `checks` found, and a job that books
    /// the whole host asks for `whole_host`, the pool's floor.
    pub(crate) fn new(
        (agents, pool, scheduler): (Arc<Agents>, ProviderId, SchedulerId),
        provider: Arc<dyn CapacityProvider>,
        launcher: Launcher,
        (kept, checks): (ScalerKeeper, Arc<Mutex<Checks>>),
        whole_host: Demand,
    ) -> Self {
        Self {
            agents,
            pool,
            scheduler,
            provider,
            launcher,
            kept,
            checks,
            whole_host,
        }
    }

    /// The name `host` if this scheduler gave it to a machine.
    fn name(&self, host: &str) -> Option<HostName> {
        HostName::parse(&self.scheduler, host)
    }
}

impl Effects for Live {
    fn hosts(&self) -> Vec<HostView> {
        self.agents.registry.hosts()
    }

    /// The queued jobs first and the live allocations after, so that a job placed between
    /// the two reads is read in both, never in neither.
    fn jobs(&self) -> Result<JobTables, JobStoreError> {
        let Some(jobs) = &self.agents.jobs else {
            return Ok(JobTables::default());
        };
        let queued = jobs.queued()?;
        let live = jobs.live_allocations()?;
        Ok(JobTables { queued, live })
    }

    fn whole_host(&self) -> Demand {
        self.whole_host.clone()
    }

    fn holds_work(&self, host: &str) -> bool {
        match (&self.agents.jobs, HostId::new(host)) {
            (Some(jobs), Ok(id)) => jobs
                .booked_on(&id)
                .map_or(true, |booking| booking.allocations > 0),
            _ => false,
        }
    }

    fn checks(&self) -> (Health, Vec<Offer>) {
        let seen = self.checks.lock().unwrap_or_else(PoisonError::into_inner);
        (seen.for_scaler(), seen.offers.clone())
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        self.provider.list()
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        self.provider.unresolved()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        self.provider.is_gone(id)
    }

    fn catalog_fetches(&self) -> u64 {
        self.launcher.catalog_fetches()
    }

    /// Writes the records, then hands the offer cool-downs they hold to the catalog's
    /// cool-down store ([`Launcher::hold`]), so a cool-down a mismatched hello started is in
    /// memory only once it is on disk. Nothing is handed over when the write fails: the step
    /// is dropped and decided again.
    fn save(&self, records: &Records) -> Result<(), String> {
        match self.kept.save(records) {
            Ok(()) => {
                self.launcher.hold(&records.offer_cooldowns());
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    fn save_unremoved(&self, hosts: &BTreeSet<String>) -> Result<(), String> {
        match self.kept.save_unremoved(hosts) {
            Ok(()) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }

    fn mint(&self, host: &str, valid_for: Duration) -> Option<Secret> {
        let name = self.name(host)?;
        match self.agents.mint_for(&self.pool, &name, valid_for) {
            Ok(minted) => Some(Secret::new(minted.token)),
            Err(err) => {
                warn!(%host, ?err, "cannot mint a join token");
                None
            }
        }
    }

    fn acquire(&self, plan: &AcquirePlan, token: Secret) -> Acquired {
        self.launcher.acquire(plan, token)
    }

    fn release(&self, id: &HostRef) -> Released {
        self.provider.release(id)
    }

    fn revoke(&self, host: &str) -> bool {
        let Some(name) = self.name(host) else {
            return true;
        };
        match self.agents.revoke_for(&self.pool, &name) {
            Ok(_) => true,
            Err(err) => {
                warn!(%host, ?err, "cannot revoke the join token of a host");
                false
            }
        }
    }

    fn forget(&self, host: &str) {
        if let Some(name) = self.name(host) {
            self.launcher.forget(&name);
        }
    }

    fn call(&self, call: Call, host: &str) {
        let Ok(id) = HostId::new(host) else {
            return;
        };
        let registry = &self.agents.registry;
        let done = match call {
            Call::Drain => registry.drain(&id),
            Call::Retire => registry.retire(&id),
            Call::Undrain => registry.undrain(&id),
            Call::ReleaseHost => registry.release_host(&id),
            Call::MarkEnding => {
                registry.mark_ending(&id);
                Ok(())
            }
            // The registry holds no host by that id: the same answer as a mark's.
            Call::Cordon => registry
                .cordon(&id, DEFAULT_CORDON)
                .map(drop)
                .map_err(|_| Busy::UnknownHost),
        };
        if let Err(busy) = done {
            warn!(%host, %busy, ?call, "the registry did not do it");
        }
    }

    fn events(&self) -> Option<EventStream> {
        self.provider.events()
    }

    fn remove(&self, host: &str) -> Result<(), Busy> {
        let Ok(id) = HostId::new(host) else {
            return Ok(());
        };
        let registry = &self.agents.registry;
        registry.mark_ending(&id);
        registry.remove(&id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    use js_elastic::{
        AcquireId, AcquireRequest, Artifact, BootStep, CatalogFuture, HostStatus, PriceUnit,
    };
    use js_jobs::Booked;
    use js_jobs::interfere::Interfere;
    use js_lease::{ManualClock, Timestamp};
    use js_registry::{PoolConfig, Registry, SequentialSessions};
    use js_state::fake::Fake;
    use js_types::{ExecMode, Model, ResourceClass, Resources, Shape, accelerator_of};

    use super::*;
    use crate::adapt::SharedClock;
    use crate::agent_api::job_rig::{placed, spec, tables};
    use crate::job_store::ServerJobs;
    use crate::network::DefaultMode;
    use crate::scaler::{RecordsFile, UnremovedFile};

    /// A provider that answers each call with a fixed word and launches nothing.
    struct Stub;

    impl CapacityProvider for Stub {
        fn id(&self) -> js_types::ProviderId {
            js_types::ProviderId::known("stub")
        }

        fn can_launch(&self) -> bool {
            false
        }

        fn capabilities(&self) -> js_elastic::ProviderCaps {
            js_elastic::ProviderCaps::default()
        }

        fn catalog(&self) -> CatalogFuture<'_> {
            Box::pin(std::future::ready(Err(ProviderError::new(
                "the catalog is not read here",
            ))))
        }

        fn capacity(&self) -> js_elastic::Capacity {
            js_elastic::Capacity {
                supplied: 0,
                can_add: Some(0),
            }
        }

        fn host_prep(&self, _offer: &Offer) -> Vec<BootStep> {
            Vec::new()
        }

        fn payload_limit(&self) -> usize {
            1 << 20
        }

        fn acquire(&self, _request: &AcquireRequest) -> Acquired {
            Acquired::Unknown
        }

        fn list(&self) -> Result<Vec<Held>, ProviderError> {
            Ok(Vec::new())
        }

        fn release(&self, _id: &HostRef) -> Released {
            Released::Terminating(HostStatus::Terminating)
        }

        fn unresolved(&self) -> Vec<AcquireId> {
            vec![AcquireId::new("u1").unwrap()]
        }

        fn observe(&self, id: &HostRef) -> js_elastic::Observed {
            match id.as_str() {
                "i-gone" => js_elastic::Observed::Absent,
                _ => js_elastic::Observed::Present(HostStatus::Active),
            }
        }

        fn verify_instance(&self, _hello: &js_elastic::Hello) -> Option<HostRef> {
            None
        }
    }

    fn quiet_logs() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(std::io::sink)
                .try_init();
        });
    }

    fn scheduler() -> SchedulerId {
        SchedulerId::new("sched").unwrap()
    }

    /// The name of the host launched for `acquire`.
    fn name(acquire: &str) -> String {
        let acquire = AcquireId::new(acquire).unwrap();
        HostName::derive(&scheduler(), &acquire).to_string()
    }

    fn dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scaler_live_unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What a whole-host job asks for in these tests.
    fn floor() -> Demand {
        let resources = Resources::new(8000, 16 << 30, 1, Some("nvidia-a10".to_owned())).unwrap();
        Demand {
            accelerator: js_types::AcceleratorNeed::booked(&resources),
            resources,
            disk_bytes: None,
            os: None,
            arch: None,
            isa_level: None,
            exec: None,
            network: js_types::JobNetwork::None,
            host_ports: Vec::new(),
        }
    }

    fn live(name: &str) -> (Live, PathBuf) {
        quiet_logs();
        let dir = dir(name);
        let clock = ManualClock::new(Timestamp::from_millis(5_000_000));
        let shared = SharedClock::new(clock.clone());
        let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a10".to_string()));
        let shape = Shape::new(
            resources.unwrap(),
            BTreeMap::from([("arch".to_string(), "x86_64".to_string())]),
        );
        let pool = ProviderId::new("burst").unwrap();
        let config = PoolConfig::new(pool.clone(), ResourceClass::gpu_host(), shape, 4);
        let registry = Registry::with_sessions(
            config,
            clock,
            SequentialSessions::new(),
            Arc::new(js_sched_placer::place::SchedPlacer::default()),
        );
        let agents = Arc::new(Agents::over(registry, &shared, &dir));
        let launcher = Launcher::new(
            Arc::new(Stub),
            Arc::new(DefaultMode::new(
                "https://sched.example.org:7301",
                Vec::new(),
            )),
            scheduler(),
            "https://sched.example.org:7301".to_owned(),
            BTreeMap::from([(
                "x86_64".to_owned(),
                Artifact {
                    url: "https://sched.example.org:7301/agent".to_owned(),
                    sha256: [1; 32],
                },
            )]),
        );
        let kept = ScalerKeeper::Files {
            records: RecordsFile::new(dir.join("records")),
            unremoved: UnremovedFile::new(dir.join("unremoved")),
        };
        let checks = Arc::new(Mutex::new(Checks::default()));
        let live = Live::new(
            (agents, pool, scheduler()),
            Arc::new(Stub),
            launcher,
            (kept, checks),
            floor(),
        );
        (live, dir)
    }

    /// [`live`] over job tables: those of a fresh store that can fail a read, with a server
    /// holding them.
    fn live_with_jobs(name: &str) -> (Live, Arc<Interfere<Fake>>, Arc<ServerJobs>) {
        let (mut live, _dir) = live(name);
        let (store, jobs) = tables();
        let jobs = Arc::new(jobs);
        let agents = Arc::try_unwrap(live.agents).ok().unwrap();
        live.agents = Arc::new(agents.with_jobs(Arc::clone(&jobs)));
        (live, store, jobs)
    }

    fn offer() -> Offer {
        Offer {
            id: "gpu_1x_a10".to_owned(),
            zone: Some("us-west-1".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            disk_bytes: 100 << 30,
            accelerator: Some(accelerator_of(
                &Model::parse("nvidia-a10").unwrap(),
                1,
                None,
            )),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            isa_level: None,
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
            purchase: None,
            quota: None,
            block: None,
        }
    }

    /// Asserts that the reads and the provider's calls pass through: no host in a new
    /// registry, a pool not yet checked with no offers, the provider's
    /// list, release, acquire, unresolved acquire ids, by-id read and notices as the provider
    /// answers them. It catches a method wired to the wrong call.
    #[tokio::test]
    async fn the_reads_and_the_providers_calls_pass_through() {
        let (live, _dir) = live("pass");
        let acquire = AcquireId::new("a1").unwrap();
        let plan = AcquirePlan {
            name: name("a1"),
            acquire,
            attempt: 0,
            pool: "burst".to_owned(),
            offer: offer(),
            lifetime: None,
        };
        let token = Secret::new("token".to_owned());
        assert_eq!(live.acquire(&plan, token), Acquired::Unknown);
        assert!(live.hosts().is_empty());
        assert_eq!(live.jobs(), Ok(JobTables::default()));
        assert_eq!(live.whole_host(), floor());
        assert!(!live.holds_work("js-nobody"));
        assert_eq!(live.checks(), (Health::Checking, Vec::new()));
        assert!(
            live.provider.catalog().await.is_err(),
            "the checks read the catalog"
        );
        assert_eq!(live.list(), Ok(Vec::new()));
        let id = HostRef("i-1".to_owned());
        assert_eq!(
            live.release(&id),
            Released::Terminating(HostStatus::Terminating)
        );
        assert_eq!(live.unresolved(), [AcquireId::new("u1").unwrap()]);
        assert!(live.is_gone(&HostRef("i-gone".to_owned())));
        assert!(!live.is_gone(&id));
        assert!(live.events().is_none(), "the stand-in sends no notices");
        // The stand-in's calls the shell does not make, so that it is whole.
        assert_eq!(live.provider.id().as_str(), "stub");
        assert!(!live.provider.can_launch());
        assert_eq!(
            live.provider.capabilities(),
            js_elastic::ProviderCaps::default()
        );
        assert_eq!(live.provider.capacity().can_add, Some(0));
        let hello = js_elastic::Hello {
            name: name("a1"),
            instance_id: None,
            identity: None,
        };
        assert_eq!(live.provider.verify_instance(&hello), None);
    }

    /// Asserts that the job tables are read whole, the queued jobs and the live allocations,
    /// and that a read that fails is the store's error, never empty tables. It catches a
    /// scaler that reads a failed read as no demand and no host in use, which would drain
    /// hosts under running jobs.
    #[test]
    fn the_job_tables_are_read_and_a_failed_read_is_an_error() {
        let (live, store, jobs) = live_with_jobs("jobs");
        let (_, running) = placed(&jobs, "running", "js-h1", spec(), Booked::WholeHost);
        let queued = jobs
            .insert_queued(js_jobs::NewJob {
                id: Some(js_jobs::JobId::parse("waits").unwrap()),
                spec: spec(),
                priority: 0,
                client: None,
            })
            .unwrap()
            .record;
        let read = live.jobs().unwrap();
        assert_eq!(read.queued, [queued]);
        assert_eq!(read.live, [running]);
        store.fail_read_after(0);
        assert!(live.jobs().is_err());
        store.fail_read_after(1);
        assert!(live.jobs().is_err());
    }

    /// Asserts that a host holds work while a live job allocation is on it, however
    /// little the registry books, and that one whose bookings cannot be read holds work too, while a
    /// host with none, and text that is no host id, hold none. It catches a release that
    /// does not read the job tables, whose allocations the registry's marks do not see, and
    /// so terminates a machine under a job placed after the tick read the tables.
    #[test]
    fn a_host_with_a_job_allocation_holds_work() {
        let (live, store, jobs) = live_with_jobs("holds");
        placed(&jobs, "running", "js-h1", spec(), Booked::WholeHost);
        assert!(live.holds_work("js-h1"));
        assert!(!live.holds_work("js-h2"));
        assert!(!live.holds_work("Not A Host Id"));
        store.fail_read_after(0);
        assert!(live.holds_work("js-h2"));
    }

    /// Asserts that the records are written whole to their file and that a file that
    /// cannot be written is reported with the system's reason: a directory that is not
    /// there. It catches a save that swallows its error, which would let the shell run a
    /// call whose record is not on disk.
    #[test]
    fn the_records_are_written_and_a_failure_is_reported() {
        let (live, dir) = live("save");
        assert_eq!(live.save(&Records::default()), Ok(()));
        assert!(dir.join("records").is_file());
        let (broken, _) = live_at_missing_dir("save_missing");
        assert!(broken.save(&Records::default()).is_err());
    }

    /// Asserts that the hosts the registry would not remove are written to their own file,
    /// and that a write into a directory that is not there is reported. It catches a save
    /// that swallows its error, which would let the shell go on as if a restart would
    /// retry the removal.
    #[test]
    fn the_unremoved_hosts_are_written_and_a_failure_is_reported() {
        let (live, dir) = live("save_unremoved");
        let hosts = BTreeSet::from(["js-a".to_owned()]);
        assert_eq!(live.save_unremoved(&hosts), Ok(()));
        assert_eq!(
            UnremovedFile::new(dir.join("unremoved")).load().unwrap(),
            hosts
        );
        let (broken, _) = live_at_missing_dir("save_unremoved_missing");
        assert!(broken.save_unremoved(&hosts).is_err());
    }

    /// A live scheduler whose files are in a directory that is not there. `name` is the
    /// test's own: two tests sharing one scratch directory would remove it under each other.
    fn live_at_missing_dir(name: &str) -> (Live, PathBuf) {
        let (mut live, dir) = live(name);
        live.kept = ScalerKeeper::Files {
            records: RecordsFile::new(dir.join("absent").join("records")),
            unremoved: UnremovedFile::new(dir.join("absent").join("unremoved")),
        };
        (live, dir)
    }

    /// Asserts the join token calls: a token is minted for a name this scheduler derived and
    /// not for any other text; a revoke drops it, and a second revoke, one for a name that
    /// is not ours and one for a host that never had a token all succeed; and when the token
    /// file cannot be written a mint makes nothing and a revoke says it did not revoke. It
    /// catches a mint for a made-up name, and a failed revoke read as done, which would let
    /// the shell terminate a machine whose token still works.
    #[test]
    fn tokens_are_minted_and_revoked_and_failures_are_reported() {
        let (live, dir) = live("tokens");
        let launched = name("a1");
        let ttl = Duration::from_secs(600);
        let secret = live.mint(&launched, ttl).unwrap();
        assert!(!secret.expose().is_empty());
        assert!(live.mint("not-a-name", ttl).is_none());
        assert!(live.revoke(&launched));
        assert!(live.revoke(&launched));
        assert!(live.revoke("not-a-name"));
        assert!(live.mint(&launched, ttl).is_some());
        let joins = dir.join("joins");
        std::fs::remove_file(&joins).unwrap();
        std::fs::create_dir(&joins).unwrap();
        assert!(live.mint(&name("a2"), ttl).is_none());
        assert!(!live.revoke(&launched));
    }

    /// Asserts that forgetting a host's network device calls the network mode for a name
    /// this scheduler derived and does nothing for any other text.
    #[test]
    fn a_network_device_is_forgotten_only_for_a_name_of_ours() {
        let (live, _dir) = live("forget");
        live.forget(&name("a1"));
        live.forget("not-a-name");
    }

    /// Asserts each call on the registry for a host it does not hold: the busy answers
    /// are logged and nothing fails, a remove of a host it never held is fine (and so is text that is no host id), a mark of
    /// ending is kept for the host's first hello, a cordon of a host it does not hold is logged,
    /// and text that is no host id is ignored.
    #[test]
    fn registry_calls_for_an_unknown_host_are_harmless() {
        let (live, _dir) = live("calls");
        let host = name("a1");
        for call in [
            Call::Drain,
            Call::Retire,
            Call::Undrain,
            Call::ReleaseHost,
            Call::MarkEnding,
            Call::Cordon,
        ] {
            live.call(call, &host);
            live.call(call, "Not A Host Id");
        }
        assert_eq!(live.remove(&host), Ok(()));
        assert_eq!(live.remove("Not A Host Id"), Ok(()));
    }
}
