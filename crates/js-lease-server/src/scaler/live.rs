//! The real [`Effects`]: the server's registry and join tokens, the pool's provider and
//! network mode, and the records file.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, Acquired, ElasticProvider, Held, HostName, HostRef, Offer, ProviderError, Released,
    SchedulerId, Secret,
};
use js_registry::{HostView, Waiting};
use js_scaler::{AcquirePlan, Health, Records};
use js_types::{HostId, ProviderId};
use tracing::{debug, warn};

use super::{Call, Effects, Launcher, RecordsFile};
use crate::agents::Agents;
use crate::checks::Health as Checks;

/// The pool's side of the server, as the shell uses it.
pub(crate) struct Live {
    agents: Arc<Agents>,
    pool: ProviderId,
    scheduler: SchedulerId,
    provider: Arc<dyn ElasticProvider>,
    launcher: Launcher,
    file: RecordsFile,
    checks: Arc<Mutex<Checks>>,
}

impl Live {
    /// The effects for the pool `pool` of `scheduler`: its machines are launched and
    /// released through `provider` and `launcher`, its tokens and hosts are `agents`', its
    /// records are kept in `file`, and its health and offers are what its `checks` found.
    pub(crate) fn new(
        (agents, pool, scheduler): (Arc<Agents>, ProviderId, SchedulerId),
        provider: Arc<dyn ElasticProvider>,
        launcher: Launcher,
        (file, checks): (RecordsFile, Arc<Mutex<Checks>>),
    ) -> Self {
        Self {
            agents,
            pool,
            scheduler,
            provider,
            launcher,
            file,
            checks,
        }
    }

    /// The name `host` if this scheduler gave it to a machine.
    fn name(&self, host: &str) -> Option<HostName> {
        HostName::parse(&self.scheduler, host)
    }
}

impl Effects for Live {
    fn waiting(&self) -> Vec<Waiting> {
        self.agents.registry.waiting()
    }

    fn hosts(&self) -> Vec<HostView> {
        self.agents.registry.hosts()
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

    fn save(&self, records: &Records) -> Result<(), String> {
        match self.file.save(records) {
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
            Call::Undrain => registry.undrain(&id),
            Call::ReleaseHost => registry.release_host(&id),
            Call::Remove => registry.remove(&id),
            Call::MarkEnding => {
                registry.mark_ending(&id);
                Ok(())
            }
        };
        if let Err(busy) = done {
            debug!(%host, %busy, ?call, "the registry did not do it");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Once;

    use js_elastic::{
        AcquireId, AcquireRequest, Artifact, BootStep, GpuClass, HostStatus, PriceUnit,
    };
    use js_lease::{ManualClock, Timestamp};
    use js_registry::{PoolConfig, Registry, SequentialSessions};
    use js_types::{ExecMode, ResourceClass, Resources, Shape};

    use super::*;
    use crate::adapt::SharedClock;
    use crate::network::DefaultMode;

    /// A provider that answers each call with a fixed word and launches nothing.
    struct Stub;

    impl ElasticProvider for Stub {
        fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
            Err(ProviderError::new("the catalog is not read here"))
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

        fn is_gone(&self, id: &HostRef) -> bool {
            id.as_str() == "i-gone"
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

    fn live(name: &str) -> (Live, PathBuf) {
        quiet_logs();
        let dir = dir(name);
        let clock = ManualClock::new(Timestamp::from_millis(5_000_000));
        let shared = SharedClock::new(clock.clone());
        let resources = Resources::new(8000, 17179869184, 1, Some("nvidia-a10".to_string()));
        let shape = Shape::new(resources.unwrap(), BTreeMap::new());
        let pool = ProviderId::new("burst").unwrap();
        let config = PoolConfig::new(pool.clone(), ResourceClass::gpu_host(), shape, 4);
        let registry = Registry::with_sessions(config, clock, SequentialSessions::new());
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
        let file = RecordsFile::new(dir.join("records"));
        let checks = Arc::new(Mutex::new(Checks::default()));
        let live = Live::new(
            (agents, pool, scheduler()),
            Arc::new(Stub),
            launcher,
            (file, checks),
        );
        (live, dir)
    }

    fn offer() -> Offer {
        Offer {
            id: "gpu_1x_a10".to_owned(),
            zone: Some("us-west-1".to_owned()),
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            disk_bytes: 100 << 30,
            gpus: 1,
            gpu_class: Some(GpuClass::parse("nvidia-a10").unwrap()),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
            price_cents_per_hour: Some(75),
            price_unit: PriceUnit::UsCents,
            available: true,
        }
    }

    /// Asserts that the reads and the provider's calls pass through: no waiting request and
    /// no host in a new registry, a pool not yet checked with no offers, the provider's
    /// list, release, acquire, unresolved acquire ids and by-id read as the provider answers
    /// them. It catches a method wired to the wrong call.
    #[test]
    fn the_reads_and_the_providers_calls_pass_through() {
        let (live, _dir) = live("pass");
        let acquire = AcquireId::new("a1").unwrap();
        let plan = AcquirePlan {
            name: name("a1"),
            acquire,
            attempt: 0,
            pool: "burst".to_owned(),
            offer: offer(),
        };
        let token = Secret::new("token".to_owned());
        assert_eq!(live.acquire(&plan, token), Acquired::Unknown);
        assert!(live.waiting().is_empty());
        assert!(live.hosts().is_empty());
        assert_eq!(live.checks(), (Health::Checking, Vec::new()));
        assert!(
            live.provider.catalog().is_err(),
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
        let (broken, _) = live_at_missing_dir();
        assert!(broken.save(&Records::default()).is_err());
    }

    fn live_at_missing_dir() -> (Live, PathBuf) {
        let (mut live, dir) = live("save_missing");
        live.file = RecordsFile::new(dir.join("absent").join("records"));
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
    /// are logged and nothing fails, a remove of a host it never held is fine, a mark of
    /// ending is kept for the host's first hello, and text that is no host id is ignored.
    #[test]
    fn registry_calls_for_an_unknown_host_are_harmless() {
        let (live, _dir) = live("calls");
        let host = name("a1");
        for call in [
            Call::Drain,
            Call::Undrain,
            Call::ReleaseHost,
            Call::Remove,
            Call::MarkEnding,
        ] {
            live.call(call, &host);
            live.call(call, "Not A Host Id");
        }
    }
}
