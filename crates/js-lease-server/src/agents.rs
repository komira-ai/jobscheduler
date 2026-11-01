//! The registering side of the server: the registry, the join tokens, the hello limit
//! and the file of bindings, and how they are put together at start.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use js_agent_wire::InstanceIdentity;
use js_elastic::HostName;
use js_registry::{Credential, OsSessions, Registry};
use js_types::ProviderId;
use tracing::{error, warn};

use crate::adapt::SharedClock;
use crate::agent_state::{self, BindingStore};
use crate::agent_tokens::AgentTokens;
use crate::capacity::CapacitySource;
use crate::config::{AgentSetup, Hosts};
use crate::error::StartupError;
use crate::join_tokens::{Admitted, JoinRefusal, JoinTokens, MintError, Minted, RevokeError};
use crate::layered::LayeredSource;
use crate::rate::HelloLimiter;
use crate::usage_hub::Usage;
use crate::usage_map::limits_of;
use crate::usage_setup::UsageSetup;
use crate::util::blocking;

/// What the agent routes and the tick share. Agent handlers take the registry's lock and
/// nothing else of the server's: the lease table's lock is taken before the registry's
/// (the table calls the provider, which calls the registry), never the reverse.
pub(crate) struct Agents {
    pub(crate) registry: Registry,
    pub(crate) tokens: AgentTokens,
    /// The one-time tokens minted for launched instances.
    pub(crate) joins: JoinTokens,
    /// The one provider the mint route accepts, or `None` for any.
    pub(crate) mint_provider: Option<String>,
    pub(crate) limiter: HelloLimiter,
    /// The server's clock, which bounds a run by the time its lease has left.
    pub(crate) clock: SharedClock,
    /// How many run ids this boot has minted.
    runs_minted: AtomicU64,
    store: BindingStore,
    errors: AtomicU64,
    /// The usage side, unless `--usage-off`. Its lock is a leaf, taken after the registry's
    /// call has returned.
    pub(crate) usage: Option<Arc<Usage>>,
}

/// Where a file of the agent side is kept: beside the lease file at `state_file`, with
/// `suffix` added to its name.
pub(crate) fn state_path(state_file: &Path, suffix: &str) -> PathBuf {
    let mut name = state_file.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    state_file.with_file_name(name)
}

/// What [`assemble`] returns: the capacity source the lease table runs on, and the agent
/// side when it is on.
pub(crate) type Assembled = (Box<dyn CapacitySource>, Option<Arc<Agents>>);

/// The capacity source the lease table runs on, and the agent side when it is on.
///
/// Without agent flags this is the source `--pool-config` names, untouched. With them it
/// is a [`LayeredSource`] over that source (if given) and a registry on `clock`, seeded
/// with the bindings the previous run persisted.
pub(crate) fn assemble(
    hosts: Hosts,
    usage: UsageSetup,
    clock: &SharedClock,
    state_file: &Path,
) -> Result<Assembled, StartupError> {
    match hosts {
        Hosts::Static(source) => Ok((source, None)),
        Hosts::Layered {
            static_pool,
            agents,
        } => {
            let AgentSetup {
                tokens,
                pool,
                mint_provider,
                hello_limit,
                require_host_credential,
                placement,
            } = *agents;
            let path = state_path(state_file, ".agents");
            let joins = JoinTokens::open(
                state_path(state_file, ".joins"),
                clock.clone(),
                Box::new(OsSessions),
            )?
            .requiring_credential(require_host_credential);
            let bindings = agent_state::load(&path)?;
            let booked = pool.shape().resources();
            let limits = limits_of(booked.cpu_millis(), booked.memory_bytes());
            let registry = Registry::new(pool, clock.clone());
            // `--placement js-sched`: the scheduler picks the host of a start.
            let placers = placement.placer();
            placers
                .into_iter()
                .for_each(|placer| registry.set_placer(placer));
            let usage = usage.build(clock, limits, &registry)?;
            // The learned lives of the profiles cost a lease's end in the drain order.
            let curves = usage.iter().filter_map(|usage| usage.curves());
            curves.for_each(|curves| registry.set_lives(curves));
            for (host, binding) in &bindings {
                registry.restore_tainted(host, binding.clone());
            }
            let source = LayeredSource::new(static_pool, registry.clone(), bindings.clone());
            let agents = Agents {
                registry,
                tokens,
                joins,
                mint_provider,
                limiter: HelloLimiter::new(hello_limit, clock.clone()),
                clock: clock.clone(),
                runs_minted: AtomicU64::new(0),
                store: BindingStore::new(path, bindings),
                errors: AtomicU64::new(0),
                usage,
            };
            Ok((Box::new(source), Some(Arc::new(agents))))
        }
    }
}

impl Agents {
    /// The agent side over `registry`, with its files in `dir`, for a test that drives
    /// the routes without a socket.
    #[cfg(test)]
    pub(crate) fn over(registry: Registry, clock: &SharedClock, dir: &Path) -> Self {
        use crate::config::parse_token;
        use crate::rate::HelloLimit;
        let client = parse_token(&[b'c'; 40]).unwrap();
        let tokens = dir.join("agent-tokens");
        std::fs::write(&tokens, [b'j'; 40]).unwrap();
        std::fs::set_permissions(&tokens, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let joins = JoinTokens::open(
            dir.join("joins"),
            clock.clone(),
            Box::new(js_registry::SequentialSessions::new()),
        )
        .unwrap();
        let limit = HelloLimit {
            burst: 5,
            interval: std::time::Duration::from_secs(1),
        };
        Self {
            registry,
            tokens: AgentTokens::open(&tokens, &client).unwrap(),
            joins,
            mint_provider: None,
            limiter: HelloLimiter::new(limit, clock.clone()),
            clock: clock.clone(),
            runs_minted: AtomicU64::new(0),
            store: BindingStore::new(dir.join("bindings"), Default::default()),
            errors: AtomicU64::new(0),
            usage: None,
        }
    }

    /// Lets the hello of `host` register with the one-time token or host credential `who`
    /// (the digest of what the request carried), consuming the token on its first use. With
    /// `issue`, that first use also retires the token for a new host credential (see
    /// [`JoinTokens::admit_with`]). The check and the file write run on the pool for blocking
    /// calls.
    pub(crate) async fn admit_join(
        self: &Arc<Self>,
        who: Credential,
        host: String,
        instance: Option<InstanceIdentity>,
        issue: bool,
    ) -> Result<Admitted, JoinRefusal> {
        let this = Arc::clone(self);
        blocking(move || this.joins.admit_with(&who, &host, instance.as_ref(), issue)).await
    }

    /// Gives back the use that [`Agents::admit_join`] made for `host`, and the credential it
    /// issued, after the registry refused its hello.
    pub(crate) async fn release_join(self: &Arc<Self>, who: Credential, host: String) {
        let this = Arc::clone(self);
        blocking(move || this.joins.release(&who, &host)).await;
    }

    /// Whether the one-time token `who` may heartbeat for `host`; a heartbeat it serves
    /// renews the token's credential. Runs on the pool for blocking calls, since a renewal
    /// writes the file.
    pub(crate) async fn serve_join(
        self: &Arc<Self>,
        who: Credential,
        host: String,
    ) -> Result<(), JoinRefusal> {
        let this = Arc::clone(self);
        blocking(move || this.joins.serves(&who, &host)).await
    }

    /// Mints the one-time join token for the host `name` of the pool `pool`: the in-process
    /// form of `POST /v1/agent/join-tokens`, for the scaler that launches the machine. The
    /// token is bound to the instance `<pool>:<name>`, which is the line the launched
    /// agent's `--instance-file` holds, and lasts `ttl` from now (at most
    /// [`crate::join_tokens::MAX_TTL`]; a longer one is the caller's mistake, the route
    /// refuses it). A second mint for the same pool and name replaces the first unused
    /// token, so a caller mints once per launch and never again for a machine that may
    /// have booted. A blocking call, as it writes the token file: run it on the pool for
    /// blocking calls.
    ///
    /// # Errors
    ///
    /// [`MintError`] as for the route.
    pub(crate) fn mint_for(
        &self,
        pool: &ProviderId,
        name: &HostName,
        ttl: Duration,
    ) -> Result<Minted, MintError> {
        self.joins.mint(Self::instance_of(pool, name), ttl)
    }

    /// Revokes every token and host credential bound to `<pool>:<name>`, used or not:
    /// the in-process form of `DELETE /v1/agent/join-tokens`. Returns how many rows were
    /// dropped; a name with none drops none and succeeds, so a repeat is a no-op. A blocking
    /// call, as [`Agents::mint_for`].
    ///
    /// # Errors
    ///
    /// [`RevokeError::Unsaved`] when the file could not be written; nothing was dropped.
    pub(crate) fn revoke_for(
        &self,
        pool: &ProviderId,
        name: &HostName,
    ) -> Result<usize, RevokeError> {
        self.joins.revoke(&Self::instance_of(pool, name))
    }

    /// The instance a launched host names in its hello: the pool as the provider and the
    /// host's name as the instance id.
    fn instance_of(pool: &ProviderId, name: &HostName) -> InstanceIdentity {
        InstanceIdentity {
            provider: pool.to_string(),
            instance_id: name.to_string(),
        }
    }

    /// A run id no earlier run has had, in this boot or any before it: the lease table's
    /// term of this boot and a count that only grows. The term is persisted with the lease
    /// file and rises at every boot, so ids stay unique across a restart although the run
    /// records themselves are forgotten.
    pub(crate) fn mint_run_id(&self, term: u64) -> String {
        let count = self.runs_minted.fetch_add(1, Ordering::SeqCst);
        format!("run-{term}-{}", count.saturating_add(1))
    }

    /// Whether `run_id` was minted in a boot before the one whose term is `term`: such a
    /// run was held in memory by a server that has since restarted, so it is forgotten
    /// whatever became of its work. An id that is not of the form [`Agents::mint_run_id`]
    /// makes is not forgotten, only unknown.
    pub(crate) fn is_forgotten(run_id: &str, term: u64) -> bool {
        let minted = run_id
            .strip_prefix("run-")
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(minted, _)| minted.parse::<u64>().ok());
        minted.is_some_and(|minted| minted < term)
    }

    /// How many times the agent side could not read or write one of its files since the
    /// server started: the join token file that could not be read again, and the file of
    /// bindings that could not be written.
    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }

    /// What every tick does for the agent side: read the join tokens again and drop the
    /// hello limits of those that left the file, and make the file of bindings current
    /// (a write that failed earlier is tried again here; so is the removal of a host the
    /// registry has forgotten). A token file that cannot be read keeps the tokens in
    /// force, and their limits. The usage side, when there is one, is ticked last.
    pub(crate) async fn refresh(self: &Arc<Self>) {
        let this = Arc::clone(self);
        match blocking(move || this.tokens.reload()).await {
            Ok(()) => self.limiter.retain(&self.tokens.credentials()),
            Err(err) => {
                self.errors.fetch_add(1, Ordering::SeqCst);
                warn!(error = %err, "cannot read the agent token file again; the tokens in force stay");
            }
        }
        self.save_bindings().await;
        if let Some(usage) = &self.usage {
            usage.tick().await;
        }
    }

    /// Makes the file of bindings hold what the registry has bound, on the pool for
    /// blocking calls: the write is synced to disk. A host that was admitted stays
    /// admitted when this fails; the lease on it would then survive a restart without
    /// its binding, until the next tick writes it. True when the file holds what the
    /// registry has bound (a taint report is acknowledged only then).
    pub(crate) async fn save_bindings(self: &Arc<Self>) -> bool {
        let this = Arc::clone(self);
        let saved = blocking(move || this.store.save(|| this.registry.bindings())).await;
        if let Err(err) = &saved {
            self.errors.fetch_add(1, Ordering::SeqCst);
            error!(error = %err, "cannot write the agent state file");
        }
        saved.is_ok()
    }
}

#[cfg(test)]
#[path = "agents_mint_tests.rs"]
mod mint_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an id this server makes, of a term below the current one, is forgotten; an id of
    /// this term or a later one, and any other text, is not. It catches a run id of the
    /// current boot answered `run-forgotten`, and text that is no run id answered so.
    #[test]
    fn only_an_id_of_an_earlier_term_is_forgotten() {
        assert!(Agents::is_forgotten("run-1-1", 2));
        assert!(Agents::is_forgotten("run-3-77", 4));
        assert!(!Agents::is_forgotten("run-2-1", 2));
        assert!(!Agents::is_forgotten("run-3-1", 2));
        for odd in [
            "", "ghost", "run-", "run-1", "run-x-1", "run--1", "xrun-1-1",
        ] {
            assert!(!Agents::is_forgotten(odd, 9), "{odd}");
        }
    }
}
