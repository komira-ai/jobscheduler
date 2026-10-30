//! The provider: [`ElasticProvider`] over [`LambdaApi`], with the cost guard.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_elastic::{
    AcquireId, AcquireRequest, Acquired, BootStep, CheckResult, Claim, Demand, ElasticProvider,
    Extra, Held, HostRef, HostStatus, Offer, ProviderError, Reason, Refusal, RefusalCode, Released,
    Secret,
};

use crate::api::LambdaApi;
use crate::clock::Clock;
use crate::config::{ElasticConfig, Mode};
use crate::error::{ApiError, ErrorCode};
use crate::host;
use crate::model::{Instance, InstanceId, LaunchRequest, SshKey};
use crate::options::OptionTable;
use crate::own::{LabelError, Owner};
use crate::select;

mod checks;

/// The most bytes of boot payload this provider carries: the user data of a launch.
///
/// UNVERIFIED. Lambda's published API description says the user data "cannot exceed 1MB in
/// size", and does not say whether that is 10^6 or 2^20 bytes, whether it counts the text
/// as sent (JSON escapes included) or what the check does at the edge. This is the smaller
/// reading, 10^6 bytes of the payload itself. No call has been made to see where the API
/// refuses; the first live launch is the check, and a payload the API refuses after this
/// accepts it comes back as a refusal, not a launch.
pub const PAYLOAD_LIMIT: usize = 1_000_000;

/// What the launch lock protects: the guard's memory. Holding the lock across the list
/// and the launch is what lets one launch run at a time.
#[derive(Default)]
struct Launching {
    /// When the last launch was sent.
    last_launch: Option<Duration>,
    /// Launches sent that got no answer, by acquire id, with the time they were sent.
    /// An entry ends when a list shows the instance, when the launch is answered, or
    /// when the hold runs out.
    unknown: BTreeMap<String, Duration>,
    /// Instances a launch answered with, until a list shows them as ours. A list can lag
    /// a write, so for a while an instance exists only in the API's list-to-be: it counts
    /// toward `max_machines` and a retry of the same acquire gets the earlier answer.
    /// An entry never ends on the clock alone, because an instance we may own must not
    /// leave the count while it might exist. It ends when a list shows a row of that id
    /// that is ours, or when the probe made once the hold has run out finds the
    /// instance terminated or (a trusted 404) gone. A probe that finds it, or fails,
    /// holds the entry for another period.
    answered: Vec<Answered>,
}

/// An instance a launch answered with that no list has shown as ours yet.
struct Answered {
    /// What the answer named.
    held: Held,
    /// The same instance's id, in the form the API takes.
    id: InstanceId,
    /// Whether this is the instance the launch is answered with on a retry. A launch
    /// asks for one instance; any other id it returns is an extra, counted all the same.
    primary: bool,
    /// When the instance is read by id next: the launch time plus the hold, then
    /// the time of each probe plus the hold.
    due: Duration,
}

/// Why the firewall or the ssh key of a launch could not be settled. Nothing was launched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// A call to the API failed.
    #[error("{0}")]
    Api(#[from] ApiError),
    /// The scheduler id is too long for the ruleset's name (64 characters at most).
    #[error("firewall: the scheduler id is too long for a ruleset name of 64 characters")]
    FirewallName,
    /// The ruleset of our name holds rules, so it is not the one that lets nothing in.
    #[error("firewall: the ruleset {name} holds {rules} rules; it must hold none")]
    FirewallRules {
        /// The ruleset's name.
        name: String,
        /// How many rules it holds.
        rules: usize,
    },
    /// The scheduler id is too long for the name of a key to make (64 characters at most).
    #[error("provider.ssh_key: the scheduler id is too long to name a key; name one")]
    KeyName,
    /// The account has several keys and none is named.
    #[error("provider.ssh_key: account has {count} keys; name one")]
    SshKeys {
        /// How many keys the account has.
        count: usize,
    },
}

impl PrepareError {
    /// The answer to an acquire that stopped here. A failed call that may or may not have
    /// reached the API is [`Acquired::Deferred`] by `retry`, not [`Acquired::Unknown`]:
    /// no launch was sent.
    fn answer(&self, retry: Duration) -> Acquired {
        match self {
            Self::Api(error) => match LambdaProvider::failed(error) {
                Acquired::Unknown => Acquired::Deferred { wait: retry },
                definite => definite,
            },
            Self::FirewallName => {
                LambdaProvider::refuse(RefusalCode::Shape, Reason::Firewall(self.to_string()))
            }
            Self::FirewallRules { .. } => {
                LambdaProvider::refuse(RefusalCode::Denied, Reason::Firewall(self.to_string()))
            }
            Self::KeyName | Self::SshKeys { .. } => {
                LambdaProvider::refuse(RefusalCode::Denied, Reason::SshKey(self.to_string()))
            }
        }
    }
}

/// The key a launch holds, as [`LambdaProvider::pick_key`] settles it.
enum KeyPick {
    /// A key the account has, by name.
    Use(String),
    /// The account has none: the key to make, by name.
    Make(String),
}

/// What a sweep found. Nothing is terminated by it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sweep {
    /// Ours, working or booting, with no booking: nobody is waiting for them.
    pub orphans: Vec<Held>,
    /// Ours, and terminating or preempted.
    pub ending: Vec<Held>,
    /// Some of our marks, not all. Never touched; they count toward `max_machines`.
    pub suspects: Vec<Held>,
}

/// The Lambda provider for one scheduler.
pub struct LambdaProvider {
    api: Arc<dyn LambdaApi>,
    clock: Arc<dyn Clock>,
    config: ElasticConfig,
    options: OptionTable,
    owner: Owner,
    launching: Mutex<Launching>,
    /// Whether a list has succeeded: a 404 on a read by id is trusted only after one.
    probed: AtomicBool,
    dry_run_logged: AtomicBool,
    /// The name of the ssh key every launch holds, once settled.
    key: Mutex<Option<String>>,
}

impl LambdaProvider {
    /// A provider that calls `api`, reads time from `clock`, may use the instance types
    /// of `options` and obeys `config`.
    pub fn new(
        api: Arc<dyn LambdaApi>,
        clock: Arc<dyn Clock>,
        options: OptionTable,
        config: ElasticConfig,
    ) -> Self {
        let owner = Owner::new(config.scheduler.clone());
        Self {
            api,
            clock,
            config,
            options,
            owner,
            launching: Mutex::new(Launching::default()),
            probed: AtomicBool::new(false),
            dry_run_logged: AtomicBool::new(false),
            key: Mutex::new(None),
        }
    }

    fn launching(&self) -> MutexGuard<'_, Launching> {
        self.launching
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Where `demand` can launch, best first (see the module docs of the choice): empty
    /// when no offer fits or none has capacity, which is a capacity refusal.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the instance types cannot be had.
    pub fn launch_options(&self, demand: &Demand) -> Result<Vec<Offer>, ApiError> {
        let types = self.api.instance_types()?;
        Ok(select::candidates(
            &select::offers(&self.options, &types, self.config.regions.as_deref()),
            demand,
        ))
    }

    /// The id of this scheduler's ruleset that lets nothing in, in `region`: found by the
    /// name `js-<scheduler>-noinbound`, or created with no rules when the region has none.
    /// Read again before every launch, never remembered: a rule added since the last
    /// launch must not ride along on the next one, and one list call is cheap next to a
    /// launch. Every launch attaches it, because a ruleset can only be attached at launch.
    /// A ruleset of that name that holds rules is refused, never used.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when the scheduler id is too long for the name, a call fails, or
    /// the ruleset holds rules.
    pub fn ensure_firewall(&self, region: &str) -> Result<String, PrepareError> {
        let name = format!("js-{}-noinbound", self.config.scheduler.as_str());
        if name.len() > 64 {
            return Err(PrepareError::FirewallName);
        }
        let found = self
            .api
            .list_firewall_rulesets()?
            .into_iter()
            .find(|ruleset| ruleset.name == name && ruleset.region == region);
        let ruleset = match found {
            Some(ruleset) => ruleset,
            None => self.api.create_firewall_ruleset(region, &name)?,
        };
        if ruleset.rules > 0 {
            return Err(PrepareError::FirewallRules {
                name,
                rules: ruleset.rules,
            });
        }
        Ok(ruleset.id)
    }

    /// The name of the one ssh key every launch holds. Lambda takes exactly one name and
    /// the key is never used to log in, because the ruleset lets nothing in. The key is,
    /// in this order: the one named in the configuration (not looked up: a name the account
    /// does not hold is refused by the API at launch); the account's key named
    /// `js-<scheduler>`; the account's only key; or, on an account with no key, a key made
    /// under the name `js-<scheduler>` by the API, of which neither half is kept. An
    /// account with several keys and none of that name is an error that asks for a name.
    /// Settled once and kept.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] when a call fails, the account has several keys and none to pick,
    /// or the key to make cannot be named.
    pub fn ssh_key(&self) -> Result<String, PrepareError> {
        if let Some(name) = &self.config.ssh_key {
            return Ok(name.clone());
        }
        let mut chosen = self.key.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(name) = chosen.as_ref() {
            return Ok(name.clone());
        }
        let keys = self.api.list_ssh_keys()?;
        let name = match self.pick_key(&keys)? {
            KeyPick::Use(name) => name,
            KeyPick::Make(ours) => self.api.create_ssh_key(&ours)?.name,
        };
        *chosen = Some(name.clone());
        Ok(name)
    }

    /// Which of the account's `keys` a launch holds when no key is configured: the one
    /// named `js-<scheduler>`, else the only key, else (on an account with none) a key to
    /// make under that name. Calls nothing.
    fn pick_key(&self, keys: &[SshKey]) -> Result<KeyPick, PrepareError> {
        let ours =
            Some(format!("js-{}", self.config.scheduler.as_str())).filter(|name| name.len() <= 64);
        let named = keys.iter().find(|key| Some(&key.name) == ours.as_ref());
        match (named, keys, ours) {
            (Some(key), _, _) => Ok(KeyPick::Use(key.name.clone())),
            (None, [only], _) => Ok(KeyPick::Use(only.name.clone())),
            (None, [], Some(ours)) => Ok(KeyPick::Make(ours)),
            (None, [], None) => Err(PrepareError::KeyName),
            (None, many, _) => Err(PrepareError::SshKeys { count: many.len() }),
        }
    }

    /// Lists what this scheduler has launched or half-launched and reports it. Report
    /// only: nothing is terminated. `booked` are the acquire ids somebody waits for.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the list cannot be had whole.
    pub fn sweep(&self, booked: &[AcquireId]) -> Result<Sweep, ApiError> {
        let mut sweep = Sweep::default();
        for instance in self.api.list_instances()? {
            if instance.status == HostStatus::Terminated {
                continue;
            }
            let held = held_of(&instance);
            match self.owner.claim(&instance) {
                Claim::Foreign => {}
                Claim::Suspect => {
                    tracing::error!(id = %held.id, "an instance carries some of this scheduler's marks, not all");
                    sweep.suspects.push(held);
                }
                Claim::Ours => match instance.status {
                    HostStatus::Terminating | HostStatus::Preempted => sweep.ending.push(held),
                    _ if booked.iter().any(|b| b.as_str() == held.acquire) => {}
                    _ => sweep.orphans.push(held),
                },
            }
        }
        Ok(sweep)
    }

    fn refuse(code: RefusalCode, reason: Reason) -> Acquired {
        Acquired::Refused(Refusal { code, reason })
    }

    /// The launch for `request` in `zone`, tagged and named.
    fn launch_request(
        &self,
        request: &AcquireRequest,
        zone: &str,
    ) -> Result<LaunchRequest, LabelError> {
        let tags = self
            .owner
            .tags(&request.acquire, &request.pool, &request.labels)?;
        // The key, the ruleset and the user data are put in when the launch is to be sent
        // (`prepare`): a dry run builds this much and calls nothing.
        Ok(LaunchRequest {
            region: zone.to_owned(),
            instance_type: request.offer.id.clone(),
            ssh_key: String::new(),
            name: request.name.to_string(),
            tags,
            user_data: None,
            firewall_ruleset_ids: Vec::new(),
        })
    }

    /// Completes `launch` for sending: the ssh key, the no-inbound ruleset of its region
    /// and the payload as user data (none when the payload is empty). Makes the account's
    /// ruleset or key when it has none, so it is not called in a dry run. The answer that
    /// stops the launch, if something does.
    fn prepare(&self, launch: &mut LaunchRequest, request: &AcquireRequest) -> Option<Acquired> {
        let retry = self.config.launch_interval;
        match self.ssh_key() {
            Ok(name) => launch.ssh_key = name,
            Err(why) => return Some(why.answer(retry)),
        }
        match self.ensure_firewall(&launch.region) {
            Ok(id) => launch.firewall_ruleset_ids.push(id),
            Err(why) => return Some(why.answer(retry)),
        }
        launch.user_data = match request.payload.is_empty() {
            true => None,
            false => Some(Secret::new(request.payload.expose().to_owned())),
        };
        Self::unprotected(launch)
    }

    /// The refusal of a launch that carries no firewall ruleset: it would have the
    /// account's default rules, and a ruleset can only be attached at launch. Never sent.
    fn unprotected(launch: &LaunchRequest) -> Option<Acquired> {
        launch.firewall_ruleset_ids.is_empty().then(|| {
            Self::refuse(
                RefusalCode::Shape,
                Reason::Firewall("a launch needs a firewall ruleset".to_owned()),
            )
        })
    }

    /// Forgets the unknown launches a list shows or the hold has outlasted (after the
    /// hold a list is trusted), and the answered instances that [`Self::still_held`]
    /// no longer holds.
    fn settle(&self, gate: &mut Launching, rows: &[Instance], now: Duration) {
        let hold = self.config.unknown_hold;
        gate.unknown.retain(|acquire, sent| {
            let seen = rows
                .iter()
                .any(|row| Owner::acquire_of(row) == Some(acquire.as_str()));
            !seen && now.saturating_sub(*sent) < hold
        });
        gate.answered
            .retain_mut(|launched| self.still_held(launched, rows, now));
    }

    /// Whether the answered instance still counts. A list row of its id that is ours ends
    /// it; a row with fewer marks does not (it is not read as ours, and a row with none
    /// is filtered out of the live set). When the hold has run out the instance is read
    /// by id: found and not terminated, or any error, keeps it for another hold; only
    /// a terminated reading or a 404 that [`Self::trust_absent`] accepts ends it.
    fn still_held(&self, launched: &mut Answered, rows: &[Instance], now: Duration) -> bool {
        let listed_ours = rows
            .iter()
            .any(|row| row.id == launched.id && self.owner.claim(row) == Claim::Ours);
        if listed_ours {
            return false;
        }
        if now < launched.due {
            return true;
        }
        let id = &launched.id;
        let keep = match self.api.get_instance(id) {
            Ok(found) => found.status != HostStatus::Terminated,
            Err(error) if self.trust_absent(&error) => false,
            Err(error) => {
                tracing::warn!(%error, %id, "the probe of an unlisted instance failed; still counting it");
                true
            }
        };
        if keep {
            tracing::warn!(%id, "an answered instance is still not listed as ours; still counting it");
            launched.due = now + self.config.unknown_hold;
        }
        keep
    }

    /// The answer of the API to a launch that did not succeed.
    fn failed(error: &ApiError) -> Acquired {
        let ApiError::Api { status, code, .. } = error else {
            return Acquired::Unknown;
        };
        // A definite answer is a client error. A server error with a familiar code may
        // be a gateway speaking after the launch landed.
        let refusal = match ((400..500).contains(status), code) {
            (false, _) => return Acquired::Unknown,
            (true, ErrorCode::InsufficientCapacity) => RefusalCode::Capacity,
            (true, ErrorCode::QuotaExceeded) => RefusalCode::Quota,
            (
                true,
                ErrorCode::InvalidParameters
                | ErrorCode::ObjectDoesNotExist
                | ErrorCode::FileSystemInWrongRegion,
            ) => RefusalCode::Shape,
            (
                true,
                ErrorCode::InvalidApiKey
                | ErrorCode::Forbidden
                | ErrorCode::AccountInactive
                | ErrorCode::InvalidAddress,
            ) => RefusalCode::Denied,
            (true, _) => return Acquired::Unknown,
        };
        Self::refuse(refusal, Reason::Api(code.to_string()))
    }

    /// Trusts a 404 on a read by id only after a list has worked: a key that cannot see
    /// instances must not read them as gone.
    fn trust_absent(&self, error: &ApiError) -> bool {
        if !error.is_absent() {
            return false;
        }
        if self.probed.load(Ordering::SeqCst) {
            return true;
        }
        let worked = self.api.list_instances().is_ok();
        self.probed.store(worked, Ordering::SeqCst);
        worked
    }

    fn launch_locked(
        &self,
        gate: &mut Launching,
        request: &AcquireRequest,
        launch: &LaunchRequest,
    ) -> Acquired {
        let rows = match self.api.list_instances() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "no launch: the instances cannot be listed");
                return Acquired::Unknown;
            }
        };
        let now = self.clock.now();
        self.settle(gate, &rows, now);
        let live: Vec<(&Instance, Claim)> = rows
            .iter()
            .map(|row| (row, self.owner.claim(row)))
            .filter(|(row, claim)| *claim != Claim::Foreign && row.status != HostStatus::Terminated)
            .collect();
        let wanted = request.acquire.as_str();
        let earlier = live.iter().find(|(row, claim)| {
            *claim == Claim::Ours
                && Owner::acquire_of(row) == Some(wanted)
                && !matches!(row.status, HostStatus::Terminating)
        });
        if let Some((row, _)) = earlier {
            return Acquired::Accepted {
                held: held_of(row),
                extras: Vec::new(),
            };
        }
        if let Some(sent) = gate.unknown.get(wanted) {
            // An earlier launch of this acquire got no answer and no list shows it yet:
            // it may have landed. A second launch would be a second instance under the
            // same acquire id, and one `max_machines` does not count.
            let wait = (*sent + self.config.unknown_hold).saturating_sub(now);
            tracing::warn!(
                ?wait,
                "no launch: an earlier launch of this acquire is unresolved"
            );
            return Acquired::Deferred { wait };
        }
        if let Some(launched) = gate
            .answered
            .iter()
            .find(|a| a.primary && a.held.acquire == wanted)
        {
            // The launch of this acquire was answered and no list shows the instance yet
            // (the caller may have crashed before it recorded the answer): give the
            // answer again rather than launch a second instance.
            tracing::warn!(id = %launched.held.id, "an earlier launch of this acquire is not listed yet; answering again");
            return Acquired::Accepted {
                held: launched.held.clone(),
                extras: Vec::new(),
            };
        }
        let half = live
            .iter()
            .filter(|(_, claim)| *claim == Claim::Suspect)
            .count();
        if half > 0 {
            tracing::error!(
                count = half,
                "no launch: instances carry some of this scheduler's marks, not all"
            );
            return Self::refuse(RefusalCode::Denied, Reason::HalfMarked { count: half });
        }
        // An answered instance a list shows as a suspect is in `live` already.
        let unlisted = gate
            .answered
            .iter()
            .filter(|a| !live.iter().any(|(row, _)| row.id == a.id))
            .count();
        let held = live.len() + gate.unknown.len() + unlisted;
        let max_machines = self.config.max_machines.get();
        if held >= max_machines {
            tracing::warn!(held, max_machines, "no launch: max_machines is reached");
            return Self::refuse(
                RefusalCode::Denied,
                Reason::AtMaxMachines { held, max_machines },
            );
        }
        if let Some(last) = gate.last_launch {
            let due = last + self.config.launch_interval;
            if now < due {
                return Acquired::Deferred { wait: due - now };
            }
        }
        gate.last_launch = Some(now);
        gate.unknown
            .insert(request.acquire.as_str().to_owned(), now);
        tracing::info!(
            instance_type = %launch.instance_type,
            region = %launch.region,
            name = %launch.name,
            price_cents_per_hour = ?request.offer.price_cents_per_hour,
            "launching one instance"
        );
        let ids = match self.api.launch(launch) {
            Ok(ids) => ids,
            Err(error) => {
                let answer = Self::failed(&error);
                if matches!(answer, Acquired::Refused(_)) {
                    gate.unknown.remove(wanted);
                }
                tracing::warn!(%error, "the launch failed");
                return answer;
            }
        };
        let Some((first, rest)) = ids.split_first() else {
            tracing::warn!("the launch answered success with no instance");
            return Acquired::Unknown;
        };
        gate.unknown.remove(wanted);
        let held = Held {
            id: first.host_ref(),
            acquire: wanted.to_owned(),
            name: Some(launch.name.clone()),
            offer: launch.instance_type.clone(),
            zone: Some(launch.region.clone()),
            status: HostStatus::Booting,
        };
        let due = now + self.config.unknown_hold;
        gate.answered.push(Answered {
            held: held.clone(),
            id: first.clone(),
            primary: true,
            due,
        });
        // Every id the launch returned counts from the answer on, the extras too.
        for id in rest {
            gate.answered.push(Answered {
                held: Held {
                    id: id.host_ref(),
                    ..held.clone()
                },
                id: id.clone(),
                primary: false,
                due,
            });
        }
        let extras = rest
            .iter()
            .map(|id| {
                tracing::error!(%id, "a launch started more than one instance; releasing the extra");
                // A 404 right after a launch may be a read that lags the write, so for
                // an extra it is not evidence of "gone".
                let released = match self.release_instance(id) {
                    Released::Gone => Released::Unknown,
                    other => other,
                };
                Extra {
                    id: id.host_ref(),
                    released,
                }
            })
            .collect();
        Acquired::Accepted { held, extras }
    }
}

impl ElasticProvider for LambdaProvider {
    fn catalog(&self) -> Result<Vec<Offer>, ProviderError> {
        let types = self.api.instance_types().map_err(provider_error)?;
        Ok(select::offers(
            &self.options,
            &types,
            self.config.regions.as_deref(),
        ))
    }

    fn checks(&self) -> Vec<CheckResult> {
        self.live_checks()
    }

    fn host_prep(&self, offer: &Offer) -> Vec<BootStep> {
        host::steps(self.config.scratch_gib, offer.disk_bytes)
    }

    fn payload_limit(&self) -> usize {
        PAYLOAD_LIMIT
    }

    // `request.lifetime` is not read: Lambda has no run-duration limit, and `shutdown -h` on
    // the instance leaves billing on. So on Lambda nothing bounds the cost if the scheduler
    // is dead; only `max_age` with a live scheduler does. The server-dead bound needs a
    // provider whose cloud enforces the lifetime.
    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        if !self.config.enabled {
            return Self::refuse(RefusalCode::Denied, Reason::Disabled);
        }
        let (Some(zone), Some(_)) = (
            request.offer.zone.as_deref(),
            self.options.get(&request.offer.id),
        ) else {
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        };
        if !self.owner.owns_name(&request.name) {
            return Self::refuse(RefusalCode::Shape, Reason::Name);
        }
        let (len, limit) = (request.payload.len(), self.payload_limit());
        if len > limit {
            return Self::refuse(RefusalCode::Shape, Reason::PayloadTooLarge { len, limit });
        }
        let in_regions = self
            .config
            .regions
            .as_ref()
            .is_none_or(|only| only.iter().any(|region| region == zone));
        if !in_regions {
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        }
        let mut launch = match self.launch_request(request, zone) {
            Ok(launch) => launch,
            Err(why) => return Self::refuse(RefusalCode::Shape, Reason::Label(why.to_string())),
        };
        if self.config.mode == Mode::DryRun {
            // Once at info level, then quietly: a pool asks again every tick.
            match self.dry_run_logged.swap(true, Ordering::SeqCst) {
                false => tracing::info!(
                    instance_type = %launch.instance_type,
                    region = %launch.region,
                    name = %launch.name,
                    tags = launch.tags.len(),
                    "dry run: a launch would be sent"
                ),
                true => tracing::debug!(name = %launch.name, "dry run: a launch would be sent"),
            }
            return Self::refuse(RefusalCode::Denied, Reason::DryRun);
        }
        if let Some(answer) = self.prepare(&mut launch, request) {
            return answer;
        }
        let mut gate = self.launching();
        self.launch_locked(&mut gate, request, &launch)
    }

    fn list(&self) -> Result<Vec<Held>, ProviderError> {
        Ok(self
            .api
            .list_instances()
            .map_err(provider_error)?
            .iter()
            .filter(|row| self.owner.claim(row) == Claim::Ours)
            .map(held_of)
            .collect())
    }

    fn release(&self, id: &HostRef) -> Released {
        match InstanceId::new(id.as_str()) {
            Ok(id) => self.release_instance(&id),
            // Not an id of this API, so not an instance of it and not ours: a definite
            // answer, never "gone" and never one to retry. Nothing is asked of the API.
            Err(_) => Released::Refused(Claim::Foreign),
        }
    }

    fn unresolved(&self) -> Vec<AcquireId> {
        let gate = self.launching();
        let ids = gate.unknown.keys();
        ids.filter_map(|id| AcquireId::new(id).ok()).collect()
    }

    fn is_gone(&self, id: &HostRef) -> bool {
        let Ok(id) = InstanceId::new(id.as_str()) else {
            return false;
        };
        match self.api.get_instance(&id) {
            Ok(_) => false,
            Err(error) => self.trust_absent(&error),
        }
    }
}

impl LambdaProvider {
    /// Terminates one instance, if it is ours, and reads it back.
    fn release_instance(&self, id: &InstanceId) -> Released {
        let first = match self.api.get_instance(id) {
            Ok(found) => found,
            Err(error) if self.trust_absent(&error) => return Released::Gone,
            Err(_) => return Released::Unknown,
        };
        match self.owner.claim(&first) {
            Claim::Ours => {}
            other => return Released::Refused(other),
        }
        // Tags can change between this read and the terminate below. The window is one
        // request wide and is accepted.
        let confirmed = match self.api.terminate(id) {
            Ok(ids) => ids.contains(id),
            Err(error) => {
                tracing::warn!(%error, %id, "terminate failed; reading the instance back");
                false
            }
        };
        match self.api.get_instance(id) {
            Err(error) if self.trust_absent(&error) => Released::Gone,
            Err(_) => Released::Unknown,
            Ok(after) => match (&after.status, confirmed) {
                (HostStatus::Terminating | HostStatus::Terminated, _) | (_, true) => {
                    Released::Terminating(after.status)
                }
                _ => Released::Unknown,
            },
        }
    }
}

fn provider_error(error: ApiError) -> ProviderError {
    ProviderError::new(error.to_string())
}

/// The scheduler's view of an instance.
pub(crate) fn held_of(instance: &Instance) -> Held {
    Held {
        id: instance.id.host_ref(),
        acquire: Owner::acquire_of(instance).unwrap_or_default().to_owned(),
        name: instance.name.clone(),
        offer: instance.instance_type.clone(),
        zone: Some(instance.region.clone()),
        status: instance.status.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a launch with no firewall ruleset is refused as a shape error and
    /// that one with a ruleset passes: the guard that keeps a launch from going out under
    /// the account's default rules.
    #[test]
    fn a_launch_without_a_ruleset_is_refused_as_shape() {
        let mut launch = LaunchRequest {
            region: "r".to_owned(),
            instance_type: "t".to_owned(),
            ssh_key: "k".to_owned(),
            name: "n".to_owned(),
            tags: Vec::new(),
            user_data: None,
            firewall_ruleset_ids: Vec::new(),
        };
        assert_eq!(
            LambdaProvider::unprotected(&launch),
            Some(Acquired::Refused(Refusal {
                code: RefusalCode::Shape,
                reason: Reason::Firewall("a launch needs a firewall ruleset".to_owned()),
            }))
        );
        launch.firewall_ruleset_ids.push("rs".to_owned());
        assert_eq!(LambdaProvider::unprotected(&launch), None);
    }
}
