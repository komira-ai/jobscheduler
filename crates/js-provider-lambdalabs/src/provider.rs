//! The provider: [`Elastic`] over [`LambdaApi`], with the cost guard.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::api::LambdaApi;
use crate::clock::Clock;
use crate::config::{ElasticConfig, Mode};
use crate::elastic::{
    AcquireRequest, Acquired, Candidate, Elastic, Extra, GpuRequest, Held, Reason, Refusal,
    RefusalCode, Released,
};
use crate::error::{ApiError, ErrorCode};
use crate::model::{Instance, InstanceId, LaunchRequest, Status};
use crate::options::OptionTable;
use crate::own::{AcquireId, Claim, LabelError, Owner};
use crate::select;

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
    /// Launches that were answered with an instance id, by acquire id, until a list shows
    /// the instance. A list can lag a write, so for a while the instance exists only in
    /// the API's list-to-be: it counts toward `max_machines` and a retry of the same
    /// acquire gets the earlier answer. An entry ends when a list shows the instance or
    /// when the hold runs out.
    answered: BTreeMap<String, Answered>,
}

/// A launch that was answered and that no list shows yet.
struct Answered {
    /// What the answer named.
    held: Held,
    /// When the launch was answered.
    sent: Duration,
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
        }
    }

    fn launching(&self) -> MutexGuard<'_, Launching> {
        self.launching
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Where `request` can launch, best first (see the module docs of the choice): empty
    /// when no type fits or none has capacity, which is a capacity refusal.
    ///
    /// # Errors
    ///
    /// [`ApiError`] when the instance types cannot be had.
    pub fn launch_options(&self, request: &GpuRequest) -> Result<Vec<Candidate>, ApiError> {
        let offers = self.api.instance_types()?;
        Ok(select::candidates(&self.options, &offers, request))
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
            if instance.status == Status::Terminated {
                continue;
            }
            let held = Held::of(&instance);
            match self.owner.claim(&instance) {
                Claim::Foreign => {}
                Claim::Suspect => {
                    tracing::error!(id = %held.id, "an instance carries some of this scheduler's marks, not all");
                    sweep.suspects.push(held);
                }
                Claim::Ours => match instance.status {
                    Status::Terminating | Status::Preempted => sweep.ending.push(held),
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

    /// The launch for `request`, tagged and named.
    fn launch_request(&self, request: &AcquireRequest) -> Result<LaunchRequest, LabelError> {
        let tags = self
            .owner
            .tags(&request.acquire, &request.pool, &request.labels)?;
        Ok(LaunchRequest {
            region: request.candidate.region.clone(),
            instance_type: request.candidate.instance_type.clone(),
            ssh_key: self.config.ssh_key.clone(),
            name: self.owner.name(&request.acquire),
            tags,
        })
    }

    /// Forgets the unknown and the answered launches a list shows or the hold has
    /// outlasted. After the hold a list is trusted: an instance it still hides is not
    /// counted, as with an unknown launch.
    fn settle(&self, gate: &mut Launching, rows: &[Instance], now: Duration) {
        let hold = self.config.unknown_hold;
        gate.unknown.retain(|acquire, sent| {
            let seen = rows
                .iter()
                .any(|row| Owner::acquire_of(row) == Some(acquire.as_str()));
            !seen && now.saturating_sub(*sent) < hold
        });
        gate.answered.retain(|_, launched| {
            let seen = rows.iter().any(|row| row.id == launched.held.id);
            !seen && now.saturating_sub(launched.sent) < hold
        });
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
        Self::refuse(refusal, Reason::Api(code.clone()))
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
            .filter(|(row, claim)| *claim != Claim::Foreign && row.status != Status::Terminated)
            .collect();
        let wanted = request.acquire.as_str();
        let earlier = live.iter().find(|(row, claim)| {
            *claim == Claim::Ours
                && Owner::acquire_of(row) == Some(wanted)
                && !matches!(row.status, Status::Terminating)
        });
        if let Some((row, _)) = earlier {
            return Acquired::Accepted {
                held: Held::of(row),
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
        if let Some(launched) = gate.answered.get(wanted) {
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
        let held = live.len() + gate.unknown.len() + gate.answered.len();
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
            price_cents_per_hour = request.candidate.price_cents_per_hour,
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
            id: first.clone(),
            acquire: wanted.to_owned(),
            name: Some(launch.name.clone()),
            instance_type: launch.instance_type.clone(),
            region: launch.region.clone(),
            status: Status::Booting,
        };
        gate.answered.insert(
            wanted.to_owned(),
            Answered {
                held: held.clone(),
                sent: now,
            },
        );
        let extras = rest
            .iter()
            .map(|id| {
                tracing::error!(%id, "a launch started more than one instance; releasing the extra");
                // A 404 right after a launch may be a read that lags the write, so for
                // an extra it is not evidence of "gone".
                let released = match self.release(id) {
                    Released::Gone => Released::Unknown,
                    other => other,
                };
                Extra {
                    id: id.clone(),
                    released,
                }
            })
            .collect();
        Acquired::Accepted { held, extras }
    }
}

impl Elastic for LambdaProvider {
    fn acquire(&self, request: &AcquireRequest) -> Acquired {
        if !self.config.enabled {
            return Self::refuse(RefusalCode::Denied, Reason::Disabled);
        }
        if self.options.get(&request.candidate.instance_type).is_none() {
            return Self::refuse(RefusalCode::Shape, Reason::NotAllowed);
        }
        let launch = match self.launch_request(request) {
            Ok(launch) => launch,
            Err(why) => return Self::refuse(RefusalCode::Shape, Reason::Label(why)),
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
        let mut gate = self.launching();
        self.launch_locked(&mut gate, request, &launch)
    }

    fn list(&self) -> Result<Vec<Held>, ApiError> {
        Ok(self
            .api
            .list_instances()?
            .iter()
            .filter(|row| self.owner.claim(row) == Claim::Ours)
            .map(Held::of)
            .collect())
    }

    fn release(&self, id: &InstanceId) -> Released {
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
                (Status::Terminating | Status::Terminated, _) | (_, true) => {
                    Released::Terminating(after.status)
                }
                _ => Released::Unknown,
            },
        }
    }
}
