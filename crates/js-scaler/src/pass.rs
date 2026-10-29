//! One step's working state, and the first half of a step: apply the provider's answers,
//! the list of held machines and the hosts' hellos to the records.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::{Acquired, Held, HostStatus, RefusalCode, Released};

use crate::input::{HostFacts, HostState, Input, Mark, PoolInput};
use crate::record::{Cooldown, CooldownReason, Phase, Record, Records, StoredOffer};
use crate::settings::{Blocked, Settings};
use crate::state::{Action, PlanLine, State};
use crate::time::Millis;

/// The records a step reads and writes, the actions it has decided so far, and the
/// facts it has recorded for a status page.
pub(crate) struct Pass<'a> {
    pub(crate) input: &'a Input,
    pub(crate) now: Millis,
    pub(crate) records: Records,
    pub(crate) quiet: BTreeMap<String, Millis>,
    pub(crate) quiet_now: BTreeSet<String>,
    pub(crate) blocked: BTreeMap<String, Blocked>,
    pub(crate) plan: Vec<PlanLine>,
    pub(crate) refused: u64,
    pub(crate) actions: Vec<Action>,
    /// The offers a provider answered `Capacity` to, which are not chosen again this step.
    pub(crate) declined: Vec<(String, String, Option<String>)>,
}

/// The host named `name`, if the registry knows it.
pub(crate) fn host_named<'a>(input: &'a Input, name: &str) -> Option<&'a HostFacts> {
    input.hosts.iter().find(|host| host.id.as_str() == name)
}

/// The live host named `name`.
fn live_host<'a>(input: &'a Input, name: &str) -> Option<&'a HostFacts> {
    host_named(input, name).filter(|host| host.state == HostState::Live)
}

/// Whether a listed machine is gone or going for good.
fn is_dead(status: &HostStatus) -> bool {
    [HostStatus::Terminated, HostStatus::Preempted].contains(status)
}

/// Whether `held` is the machine of `record`: the same acquire id, name or provider id.
fn listed(record: &Record, held: &Held) -> bool {
    let by_acquire = record
        .acquire
        .as_ref()
        .is_some_and(|acquire| acquire.as_str() == held.acquire);
    let by_name = [
        !record.name.is_empty(),
        held.name.as_deref() == Some(record.name.as_str()),
    ]
    .into_iter()
    .all(|same| same);
    let by_cloud = record.cloud_id.as_deref() == Some(held.id.as_str());
    [by_acquire, by_name, by_cloud].into_iter().any(|same| same)
}

/// Whether a host's announced shape is below the offer its machine was launched from, in
/// cpu, memory, gpus or gpu class.
fn below(offer: &StoredOffer, host: &js_elastic::Offer) -> bool {
    let holds = [
        host.cpu_millis >= offer.cpu_millis,
        host.memory_bytes >= offer.memory_bytes,
        host.gpus >= offer.gpus,
        offer.gpu_class.as_deref().is_none_or(|class| {
            host.gpu_class
                .as_ref()
                .is_some_and(|own| own.as_str() == class)
        }),
    ];
    !holds.into_iter().all(|ok| ok)
}

impl<'a> Pass<'a> {
    pub(crate) fn new(state: State, input: &'a Input, now: Millis) -> Self {
        Self {
            input,
            now,
            records: state.records,
            quiet: state.quiet,
            quiet_now: BTreeSet::new(),
            blocked: BTreeMap::new(),
            plan: Vec::new(),
            refused: state.refused,
            actions: Vec::new(),
            declined: Vec::new(),
        }
    }

    /// The positions of the pool's records.
    pub(crate) fn indices(&self, pool: &str) -> Vec<usize> {
        let all = &self.records.instances;
        (0..all.len()).filter(|&i| all[i].pool == pool).collect()
    }

    /// Moves record `i` to `to`; an unlisted move is counted and changes nothing.
    pub(crate) fn go(&mut self, i: usize, to: Phase) -> bool {
        let moved = self.records.instances[i].go(to, self.now);
        self.refused += u64::from(!moved);
        moved
    }

    /// Starts the release of a machine: its join token is revoked, and from now on every
    /// step asks the provider to release it. Each machine enters `Releasing` once, so
    /// its token is revoked once.
    fn start_release(&mut self, i: usize) {
        let moved = self.go(i, Phase::Releasing);
        let name = self.records.instances[i].name.clone();
        self.actions.extend(moved.then_some(Action::Revoke(name)));
    }

    /// Revokes the token and forgets the network key of a machine that never was.
    fn clean_up(&mut self, i: usize) {
        let name = self.records.instances[i].name.clone();
        self.actions.push(Action::Revoke(name.clone()));
        self.actions.push(Action::ForgetNetwork(name));
    }

    /// The pool launches nothing until its cooldown has run.
    fn cool(&mut self, settings: &Settings, reason: CooldownReason) {
        let until = self.now.plus(settings.boot_fail_cooldown);
        self.records
            .cooldowns
            .insert(settings.pool.clone(), Cooldown { until, reason });
    }

    /// Applies the answers, the list and the hosts to the pool's records.
    pub(crate) fn reconcile(&mut self, pool: &PoolInput) {
        self.apply_acquired(pool);
        self.apply_released(pool);
        if let Some(held) = &pool.held {
            self.apply_list(pool, held);
        }
        self.apply_hosts(pool);
    }

    fn apply_acquired(&mut self, pool: &PoolInput) {
        let input = self.input;
        for result in &input.acquired {
            let found = self.records.instances.iter().position(|record| {
                (record.pool.as_str(), record.acquire.as_ref())
                    == (pool.settings.pool.as_str(), Some(&result.acquire))
            });
            if let Some(i) = found {
                self.on_acquired(pool, i, &result.answer);
            }
        }
    }

    fn on_acquired(&mut self, pool: &PoolInput, i: usize, answer: &Acquired) {
        match answer {
            Acquired::Accepted { held, .. } => {
                let to = match self.records.instances[i].phase {
                    Phase::Failed => Phase::Releasing,
                    _ => Phase::Booting,
                };
                if self.go(i, to) {
                    self.records.instances[i].cloud_id = Some(held.id.0.clone());
                }
            }
            Acquired::Refused(refusal) => self.on_refused(pool, i, refusal.code),
            Acquired::Unknown => {
                self.go(i, Phase::Unresolved);
            }
            Acquired::Deferred { wait } => {
                if self.go(i, Phase::Planned) {
                    self.records.instances[i].retry_at = Some(self.now.plus(*wait));
                }
            }
        }
    }

    fn on_refused(&mut self, pool: &PoolInput, i: usize, code: RefusalCode) {
        if self.go(i, Phase::Refused) {
            self.clean_up(i);
            let record = &self.records.instances[i];
            let offer = (
                record.pool.clone(),
                record.offer.id.clone(),
                record.offer.zone.clone(),
            );
            match code {
                RefusalCode::Capacity => self.declined.push(offer),
                RefusalCode::Quota => self.cool(&pool.settings, CooldownReason::Quota),
                RefusalCode::Denied => self.cool(&pool.settings, CooldownReason::Denied),
                RefusalCode::Shape => self.cool(&pool.settings, CooldownReason::Shape),
            }
        }
    }

    fn apply_released(&mut self, pool: &PoolInput) {
        let input = self.input;
        for result in &input.released {
            let found = self.records.instances.iter().position(|record| {
                (record.pool.as_str(), record.name.as_str())
                    == (pool.settings.pool.as_str(), result.host.as_str())
            });
            if let (Some(i), Released::Gone) = (found, &result.answer) {
                let moved = self.go(i, Phase::Gone);
                if moved {
                    let name = self.records.instances[i].name.clone();
                    self.records.instances[i].gone_at = Some(self.now);
                    self.actions.push(Action::RemoveHost(name.clone()));
                    self.actions.push(Action::ForgetNetwork(name));
                }
            }
        }
    }

    fn apply_list(&mut self, pool: &PoolInput, held: &[Held]) {
        for i in self.indices(&pool.settings.pool) {
            let matched = held
                .iter()
                .find(|each| listed(&self.records.instances[i], each));
            match matched {
                Some(each) => self.on_listed(pool, i, each),
                None => self.on_unlisted(pool, i),
            }
        }
        for each in held {
            self.adopt(pool, each);
        }
    }

    fn on_listed(&mut self, pool: &PoolInput, i: usize, held: &Held) {
        let before = self.records.instances[i].phase;
        self.records.instances[i]
            .cloud_id
            .get_or_insert_with(|| held.id.0.clone());
        let found = match before {
            Phase::Launching | Phase::Unresolved => Some(Phase::Booting),
            Phase::Failed => Some(Phase::Releasing),
            _ => None,
        };
        if let Some(to) = found {
            self.go(i, to);
        }
        match (before, is_dead(&held.status)) {
            (Phase::Booting | Phase::Registered | Phase::Draining, true) => {
                self.start_release(i);
                if before == Phase::Booting {
                    self.cool(&pool.settings, CooldownReason::BootFailed);
                }
            }
            (Phase::Orphan, true) => self.orphan_gone(i),
            _ => {}
        }
    }

    fn on_unlisted(&mut self, pool: &PoolInput, i: usize) {
        let record = &self.records.instances[i];
        let hold = pool.settings.unknown_hold;
        let waited = self.now.since(record.since) >= hold;
        match (record.phase, waited) {
            (Phase::Launching | Phase::Unresolved, true) => {
                self.go(i, Phase::Failed);
                self.clean_up(i);
                self.clean_list(i, hold);
            }
            (Phase::Failed, _) => self.clean_list(i, hold),
            (Phase::Orphan, _) => self.orphan_gone(i),
            _ => {}
        }
    }

    /// Counts a list that shows no instance of a failed acquire. Two of them, at least
    /// `hold` apart, settle it.
    fn clean_list(&mut self, i: usize, hold: std::time::Duration) {
        let now = self.now;
        let record = &mut self.records.instances[i];
        match record.clean_lists {
            0 => {
                record.clean_lists = 1;
                record.last_clean = Some(now);
            }
            1 if record
                .last_clean
                .is_some_and(|last| now.since(last) >= hold) =>
            {
                record.clean_lists = 2;
            }
            _ => {}
        }
    }

    fn orphan_gone(&mut self, i: usize) {
        self.go(i, Phase::Gone);
        self.records.instances[i].gone_at = Some(self.now);
    }

    /// A listed machine no record knows becomes an orphan: counted at its offer's
    /// current price (or the whole hourly cap when the catalog has no price for it),
    /// reported and left alone.
    fn adopt(&mut self, pool: &PoolInput, held: &Held) {
        let settings = &pool.settings;
        let known = self.records.instances.iter().any(|record| {
            [record.pool == settings.pool, listed(record, held)]
                .into_iter()
                .all(|same| same)
        });
        if [!known, !is_dead(&held.status)].into_iter().all(|ok| ok) {
            let offer = pool
                .offers
                .iter()
                .filter(|each| each.id == held.offer)
                .max_by_key(|each| each.zone == held.zone);
            let price = offer
                .and_then(|each| each.price_cents_per_hour)
                .unwrap_or(settings.max_spend_per_hour);
            let shape = offer.map_or_else(
                || StoredOffer::nothing(&held.offer, held.zone.clone()),
                StoredOffer::from_offer,
            );
            self.records.instances.push(Record {
                pool: settings.pool.clone(),
                acquire: js_elastic::AcquireId::new(&held.acquire).ok(),
                name: held.name.clone().unwrap_or_default(),
                offer: shape,
                price_cents_per_hour: price,
                phase: Phase::Orphan,
                since: self.now,
                launched_at: self.now,
                token_expires: None,
                cloud_id: Some(held.id.0.clone()),
                retry_at: None,
                gone_at: None,
                registered: false,
                condemned: false,
                clean_lists: 0,
                last_clean: None,
            });
        }
    }

    fn apply_hosts(&mut self, pool: &PoolInput) {
        let input = self.input;
        for i in self.indices(&pool.settings.pool) {
            let record = &self.records.instances[i];
            let host = live_host(input, &record.name);
            match (record.phase, host) {
                (Phase::Booting, Some(host)) => self.on_hello(pool, i, host),
                (Phase::Booting, None) => self.on_boot_wait(pool, i),
                (Phase::Draining, Some(host)) => self.on_drain_mark(i, host),
                _ => {}
            }
        }
    }

    /// The first hello of a booting machine: it is registered, and condemned if it
    /// announced less than its offer.
    fn on_hello(&mut self, pool: &PoolInput, i: usize, host: &HostFacts) {
        let record = &mut self.records.instances[i];
        let short = below(&record.offer, &host.shape);
        record.registered = true;
        record.condemned = short;
        self.go(i, Phase::Registered);
        if short {
            self.cool(&pool.settings, CooldownReason::BootFailed);
        }
    }

    /// A machine that has not said hello is given up on after `boot_timeout`.
    fn on_boot_wait(&mut self, pool: &PoolInput, i: usize) {
        let waited = self.now.since(self.records.instances[i].since);
        if waited >= pool.settings.boot_timeout {
            self.start_release(i);
            self.cool(&pool.settings, CooldownReason::BootFailed);
        }
    }

    /// Reads the registry's mark back into a draining record: `release_host` took, or the
    /// drain did not (the host was busy), or it is still draining.
    fn on_drain_mark(&mut self, i: usize, host: &HostFacts) {
        match host.mark {
            Some(Mark::Releasing) => self.start_release(i),
            Some(Mark::Draining) => {}
            _ => {
                self.go(i, Phase::Registered);
            }
        }
    }
}
