//! The job rig with live logs: a server whose log sink is a fake object store and whose
//! credentials a fake minter signs ([`logging_rig`]).

use super::*;

/// The store a log sink names, as the agent and the server see it.
pub(crate) const LOG_ENDPOINT: &str = "https://objects.example";

/// The region of [`LOG_ENDPOINT`].
pub(crate) const LOG_REGION: &str = "us-east-1";

/// The bucket of the log sink.
pub(crate) const LOG_BUCKET: &str = "logs";

/// The log prefix of the sink: one the prefix `a/bc/` is outside of.
pub(crate) const LOG_PREFIX: &str = "a/b";

/// How long a host credential is asked to live.
pub(crate) const LOG_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// A server named `name` that holds the job tables and minting through a fake minter, whose
/// hosts are lost after `grace_ms` of silence; the fake minter and the server's grants.
pub(crate) fn logging_rig(name: &str, grace_ms: u64) -> (Rig, Logs) {
    logging_over(name, grace_ms, None, START_MS)
}

/// [`logging_rig`] over the job tables of `tables` (a store a server ran on before) when
/// given (and on its clock), with the clock at `now_ms`: a server rebuilt on the same store,
/// with a new registry and nothing of the log credentials held before. Its registry, server
/// and minter read one manual clock, which [`Rig::advance`] moves; fresh job tables get a
/// clock of their own, which a commit's sync moves and nothing else.
pub(crate) fn logging_over(
    name: &str,
    grace_ms: u64,
    tables: Option<&Rig>,
    now_ms: u64,
) -> (Rig, Logs) {
    let clock = tables.map_or_else(Arc::default, |rig| Arc::clone(&rig.clock));
    let since = js_state::Clock::now(&*clock).as_duration();
    let target = std::time::Duration::from_millis(now_ms);
    clock.advance(target.saturating_sub(since));
    let shared = SharedClock::new(OneClock(Arc::clone(&clock)));
    let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
    let store = FakeStore::new(endpoint.clone(), Arc::new(LogClock(shared.clone())));
    let minter = Arc::new(FakeMinter::new(store));
    let grants = grants_over(
        endpoint,
        Arc::clone(&minter) as Arc<dyn js_logmint::Minter>,
        &shared,
    );
    let extra = Extra {
        grace_ms: Some(grace_ms),
        logs: Some(Arc::clone(&grants)),
        tables: tables.map(|rig| (Arc::clone(&rig.store), Arc::clone(&rig.jobs))),
        store_clock: Some(Arc::default()),
        whole: None,
    };
    let kept = (JobLimits::default(), Retention::DEFAULT);
    let rig = rig_on(name, (true, false), kept, (clock, shared.clone()), extra);
    let logs = Logs {
        minter,
        grants,
        clock: shared,
    };
    (rig, logs)
}

/// The grants of a sink at `endpoint` under [`LOG_PREFIX`], minting with `minter` on `clock`.
pub(crate) fn grants_over(
    endpoint: Endpoint,
    minter: Arc<dyn js_logmint::Minter>,
    clock: &SharedClock,
) -> Arc<LogGrants> {
    let prefix = LogPrefix::new(LOG_PREFIX).unwrap();
    let template = SinkTemplate {
        endpoint,
        prefix: prefix.clone(),
    };
    let scope = Scope {
        bucket: Bucket::new(LOG_BUCKET).unwrap(),
        prefix,
        ttl: LOG_TTL,
    };
    Arc::new(LogGrants::new(template, scope, minter, clock.clone()))
}

/// The log side of a [`logging_rig`].
pub(crate) struct Logs {
    /// The fake minter, which counts the mints per host and can fail or hold them.
    pub(crate) minter: Arc<FakeMinter>,
    /// The grants the server holds.
    pub(crate) grants: Arc<LogGrants>,
    /// The server's clock.
    clock: SharedClock,
}

impl Logs {
    /// Waits until no mint is running; never for a time.
    pub(crate) async fn settle(&self) {
        let grants = Arc::clone(&self.grants);
        blocking(move || grants.settle()).await;
    }

    /// Waits until `n` mints are held by the blocked minter.
    pub(crate) async fn held(&self, n: usize) {
        let minter = Arc::clone(&self.minter);
        blocking(move || minter.wait_for_held(n)).await;
    }

    /// A client of the fake store signing with `grant`, as the agent signs its uploads.
    pub(crate) fn client(&self, grant: &LogGrant) -> LogStore<FakeStore, LogClock> {
        let endpoint = Endpoint::parse(LOG_ENDPOINT, LOG_REGION, LOG_BUCKET, true).unwrap();
        let credential = js_logstore::Credential {
            access_key_id: grant.access_key_id.clone(),
            secret_access_key: grant.secret_access_key.clone(),
            session_token: grant.session_token.clone(),
            expires_at: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(grant.expires_at_ms),
        };
        let clock = LogClock(self.clock.clone());
        LogStore::new(self.minter.store().clone(), clock, endpoint, credential)
    }
}
