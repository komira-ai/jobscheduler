//! One step's working state, and the first half of a step: apply the provider's answers,
//! the list of held machines and the hosts' hellos to the records.

use std::collections::{BTreeMap, BTreeSet};

use js_elastic::{Acquired, Extra, Held, HostStatus, RefusalCode, Released};

use crate::input::{AcquireResult, HostFacts, HostState, Input, Mark, PoolInput};
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
    /// Since when each silent host was silent at the last step, and at this one.
    silent: BTreeMap<String, Millis>,
    pub(crate) silent_now: BTreeMap<String, Millis>,
    /// The machines released this step on their host's silence.
    pub(crate) silent_released: Vec<String>,
    /// Since when each machine was missing from a complete view, at the last step and now.
    missing: BTreeMap<String, Millis>,
    pub(crate) missing_now: BTreeMap<String, Millis>,
    /// Since when each booting machine was unheard from on complete views, at the last
    /// step and now.
    unheard: BTreeMap<String, Millis>,
    pub(crate) unheard_now: BTreeMap<String, Millis>,
    /// Records for the machines an acquire answer named besides the one wanted, added when
    /// the lists have been read.
    extras: Vec<Record>,
    /// How many of the step's random values the launches so far have used.
    pub(crate) drawn: usize,
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
/// cpu, memory, gpus or gpu class. A host whose shape is not known is not below anything.
fn below(offer: &StoredOffer, host: Option<&js_elastic::Offer>) -> bool {
    host.is_some_and(|host| {
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
    })
}

impl<'a> Pass<'a> {
    pub(crate) fn new(state: State, input: &'a Input, now: Millis) -> Self {
        let last = state.records.last_step;
        // The first step of a scaler read from records: it was down since the step that
        // wrote them, and no host could say hello meanwhile, so each count moves forward
        // by that span and holds only what the scaler observed. Records that name no such
        // step give no span to take away, so the counts begin again now.
        let after_downtime = |times: BTreeMap<String, Millis>| -> BTreeMap<String, Millis> {
            if !state.restored {
                return times;
            }
            times
                .into_iter()
                .map(|(name, since)| {
                    let moved = last.map_or(now, |last| since.plus(now.since(last)).min(now));
                    (name, moved)
                })
                .collect()
        };
        let silent = after_downtime(state.silent);
        let missing = after_downtime(state.missing);
        Self {
            input,
            now,
            records: state.records,
            quiet: state.quiet,
            quiet_now: BTreeSet::new(),
            silent,
            silent_now: BTreeMap::new(),
            silent_released: Vec::new(),
            missing,
            missing_now: BTreeMap::new(),
            unheard: state.unheard,
            unheard_now: BTreeMap::new(),
            extras: Vec::new(),
            drawn: 0,
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
    pub(crate) fn clean_up(&mut self, i: usize) {
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
        self.records.instances.append(&mut self.extras);
    }

    fn apply_acquired(&mut self, pool: &PoolInput) {
        let input = self.input;
        for result in &input.acquired {
            let found = self.records.instances.iter().position(|record| {
                (record.pool.as_str(), record.acquire.as_ref())
                    == (pool.settings.pool.as_str(), Some(&result.acquire))
            });
            if let Some(i) = found {
                self.on_acquired(pool, i, result);
            }
        }
    }

    fn on_acquired(&mut self, pool: &PoolInput, i: usize, result: &AcquireResult) {
        match &result.answer {
            Acquired::Accepted { held, extras } => {
                let to = match self.records.instances[i].phase {
                    Phase::Failed => Phase::Releasing,
                    _ => Phase::Booting,
                };
                if self.go(i, to) {
                    self.records.instances[i].cloud_id = Some(held.id.0.clone());
                    self.record_extras(i, extras);
                }
            }
            Acquired::Refused(refusal) => self.on_refused(pool, i, refusal.code),
            Acquired::Unknown => {
                self.go(i, Phase::Unresolved);
            }
            // A deferral answers one call. One for an earlier attempt than the record's is
            // a replay of an answer already acted on: planning the launch again would send
            // a second token for a call that may have gone out.
            Acquired::Deferred { wait } if result.attempt == self.records.instances[i].attempt => {
                if self.go(i, Phase::Planned) {
                    self.records.instances[i].retry_at = Some(self.now.plus(*wait));
                }
            }
            Acquired::Deferred { .. } => {}
        }
    }

    /// Records the machines an accepted launch named besides the one wanted, as the
    /// provider reported them: one it released is a gone record (billed like a boot that
    /// failed, which can only overstate), one it could not confirm released is an orphan
    /// that is counted until a list stops showing it. They carry the launch's pool, offer
    /// and price, and no acquire id or name, so a list matches them by the provider's id.
    fn record_extras(&mut self, i: usize, extras: &[Extra]) {
        let launch = self.records.instances[i].clone();
        for extra in extras {
            let released = matches!(extra.released, Released::Gone);
            self.extras.push(Record {
                acquire: None,
                name: String::new(),
                phase: if released { Phase::Gone } else { Phase::Orphan },
                since: self.now,
                launched_at: self.now,
                token_expires: None,
                cloud_id: Some(extra.id.0.clone()),
                retry_at: None,
                attempt: 0,
                gone_at: released.then_some(self.now),
                registered: false,
                condemned: false,
                clean_lists: 0,
                last_clean: None,
                ..launch.clone()
            });
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
                self.confirm_gone(i);
            }
        }
    }

    /// The machine is confirmed gone: the host is removed from the registry and its network
    /// device forgotten, once. Both are asked of the shell a single time: a failed forget is
    /// logged and does not block (the device is ephemeral and drops when offline), and the
    /// registry removes the host itself when it has heard nothing for its retention
    /// (`forget_lost`), so a repeat could only repeat a no-op.
    fn confirm_gone(&mut self, i: usize) {
        if self.go(i, Phase::Gone) {
            let name = self.records.instances[i].name.clone();
            self.records.instances[i].gone_at = Some(self.now);
            self.actions.push(Action::RemoveHost(name.clone()));
            self.actions.push(Action::ForgetNetwork(name));
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
            (Phase::Orphan, false) => {
                let record = &mut self.records.instances[i];
                record.clean_lists = 0;
                record.last_clean = None;
            }
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
            (Phase::Orphan, _) => {
                self.clean_list(i, hold);
                if self.records.instances[i].clean_lists >= 2 {
                    self.orphan_gone(i);
                }
            }
            _ => {}
        }
    }

    /// Counts a list that shows no instance of a failed acquire or of an orphan. Two of
    /// them, at least `hold` apart, settle it: a late list proves little, so one list that
    /// lacks a machine does not make it gone.
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
    /// reported and left alone. A record that is gone or was refused does not know it: a
    /// machine such a record described, listed running, is counted again as a new orphan
    /// and the old record keeps the time it was billed.
    fn adopt(&mut self, pool: &PoolInput, held: &Held) {
        let settings = &pool.settings;
        let known = self.records.instances.iter().any(|record| {
            [
                record.pool == settings.pool,
                ![Phase::Gone, Phase::Refused].contains(&record.phase),
                listed(record, held),
            ]
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
                .filter(|price| *price > 0)
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
                attempt: 0,
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
            let host = host_named(input, &record.name);
            match (record.phase, host) {
                (Phase::Booting, Some(host)) => self.on_hello(pool, i, host),
                (Phase::Booting, None) => self.on_boot_wait(pool, i),
                (Phase::Registered | Phase::Draining, host) => self.on_held(pool, i, host),
                _ => {}
            }
        }
    }

    /// The first hello of a booting machine: it is registered, and condemned if it
    /// announced less than its offer. A host the registry shows has said hello whatever it
    /// has done since, so one that is silent now is registered too and then held.
    fn on_hello(&mut self, pool: &PoolInput, i: usize, host: &HostFacts) {
        let record = &mut self.records.instances[i];
        let short = below(&record.offer, host.shape.as_ref());
        record.registered = true;
        record.condemned = short;
        self.go(i, Phase::Registered);
        if short {
            self.cool(&pool.settings, CooldownReason::BootFailed);
        }
    }

    /// The instant a count of complete views that miss something began, given the instant
    /// it began at an earlier step. A count starts at the first complete view; an incomplete
    /// view proves nothing either way, so it leaves a count that has begun as it was and
    /// starts none. The caller drops the count at any step on which the host is shown.
    fn counted_since(&self, seen: Option<Millis>) -> Option<Millis> {
        seen.or(self.input.registry_complete.then_some(self.now))
    }

    /// A machine that has not said hello is given up on `boot_timeout` after its launch,
    /// the instant its join token was minted from, and only once its host has also been
    /// unheard from on complete registry views for the pool's boot hold
    /// ([`Settings::boot_hold`]). A hello shows the host and ends the count. The hold is
    /// what keeps a machine launched before a restart, whose agent is waiting to retry
    /// against a registry that has just loaded, from being given up on at the first
    /// complete view; on an incomplete view its host may have said hello and be holding
    /// work. The machine is held and counted until then.
    fn on_boot_wait(&mut self, pool: &PoolInput, i: usize) {
        let record = &self.records.instances[i];
        let name = record.name.clone();
        let waited = self.now.since(record.launched_at);
        let since = self.counted_since(self.unheard.get(&name).copied());
        self.unheard_now.extend(since.map(|since| (name, since)));
        let due = [
            waited >= pool.settings.boot_timeout,
            self.input.registry_complete,
            since.is_some_and(|since| self.now.since(since) >= pool.settings.boot_hold()),
        ];
        if due.into_iter().all(|ok| ok) {
            self.start_release(i);
            self.cool(&pool.settings, CooldownReason::BootFailed);
        }
    }

    /// A registered or draining machine and what the registry shows of its host.
    ///
    /// * A live host: its mark is read back ([`Pass::on_mark`]).
    /// * A host the provider says it no longer has (`not_found`) and the registry does not
    ///   hear from: revoked, forgotten and removed, and gone.
    /// * A host the registry no longer shows: it forgot it (`forget_lost`), so it holds no
    ///   lease, and the machine is still billed and still counted; it is released.
    /// * A host the registry shows but does not hear from: held and counted while the
    ///   registry still holds its leases ([`Pass::on_silent`]).
    fn on_held(&mut self, pool: &PoolInput, i: usize, host: Option<&HostFacts>) {
        let name = self.records.instances[i].name.clone();
        let not_found = self.input.not_found.contains(&name);
        match (host, not_found) {
            (Some(host), _) if host.state == HostState::Live => self.on_mark(i, host.mark),
            (_, true) => {
                self.start_release(i);
                self.confirm_gone(i);
            }
            (None, false) => self.on_forgotten(pool, i, name),
            (Some(host), false) => self.on_silent(pool, i, host),
        }
    }

    /// A registered or draining machine whose host the registry shows but does not hear
    /// from. It is held and counted, and `silent` says since when, until the host books
    /// nothing and has been silent for `unknown_hold`; only then does the machine cost
    /// money for nothing. A host that holds a lease, a run or a stream, or is set aside
    /// for work (`Held`, `Reserved`), is never released, whatever else the snapshot says;
    /// except that once the registry has given up the host's leases
    /// ([`HostFacts::lease_given_up`]) the runs and streams it lists are stale and do not
    /// count, and only a `Held` or `Reserved` mark keeps it. Otherwise:
    ///
    /// * the registry marks the host `Releasing` (it booked nothing when it did): the
    ///   machine is released and named in `silent_released` for the shell to log;
    /// * the registry's lease grace has given up the leases
    ///   ([`HostFacts::lease_given_up`]): the host is drained and marked for release, as
    ///   an idle one is, and the registry checks it is quiet when it runs those actions.
    ///   Its mark is read back on a later step (`Releasing` above, or `Draining` again if
    ///   the registry found it busy, when both are asked once more).
    fn on_silent(&mut self, pool: &PoolInput, i: usize, host: &HostFacts) {
        let name = self.records.instances[i].name.clone();
        let hold = pool.settings.unknown_hold;
        let ripe = self
            .silent
            .get(&name)
            .is_some_and(|since| self.now.since(*since) >= hold);
        // A run a silent host lists is no work once the registry has given up its lease:
        // the job can no longer report, and the registry keeps the listing until the host
        // is forgotten. Only the booked lease, or a mark, keeps the host.
        let working = [
            (host.busy, host.lease_given_up) == (true, false),
            matches!(host.mark, Some(Mark::Held | Mark::Reserved)),
        ];
        let due = [ripe, !working.into_iter().any(|work| work)];
        let marked = host.mark == Some(Mark::Releasing);
        match (due.into_iter().all(|ok| ok), marked, host.lease_given_up) {
            (false, _, _) | (true, false, false) => {}
            (true, true, _) => {
                self.start_release(i);
                self.silent_released.push(name);
            }
            (true, false, true) => {
                if self.records.instances[i].phase == Phase::Registered {
                    self.go(i, Phase::Draining);
                }
                self.actions.push(Action::Drain(name.clone()));
                self.actions.push(Action::ReleaseHost(name));
            }
        }
    }

    /// A machine the registry does not show. It is released only when the view is complete
    /// and the machine has been missing from every complete view since one at least
    /// `unknown_hold` ago: the registry may have restarted or been read in part, and
    /// releasing on that would cut work. Any step on which the host is shown starts the
    /// count over; an incomplete view leaves it as it was and adds nothing to it. The time
    /// the count began is in the records, so it survives a restart of the scaler. Until then
    /// the machine is held and counted.
    fn on_forgotten(&mut self, pool: &PoolInput, i: usize, name: String) {
        let since = self.counted_since(self.missing.get(&name).copied());
        self.missing_now.extend(since.map(|since| (name, since)));
        let due = [
            self.input.registry_complete,
            since.is_some_and(|since| self.now.since(since) >= pool.settings.unknown_hold),
        ];
        if due.into_iter().all(|ok| ok) {
            self.start_release(i);
        }
    }

    /// Reads the registry's mark on a live host back into its record. `Releasing` means the
    /// registry books nothing on the host and nothing clears the mark, so the machine is
    /// released, whatever the scaler had last done with it. For a draining record, a mark
    /// that is still `Draining` changes nothing, and any other means the drain did not
    /// take (the host was busy) or was undone, so the host is registered again.
    fn on_mark(&mut self, i: usize, mark: Option<Mark>) {
        match (self.records.instances[i].phase, mark) {
            (_, Some(Mark::Releasing)) => self.start_release(i),
            (Phase::Draining, Some(Mark::Draining)) => {}
            (Phase::Draining, _) => {
                self.go(i, Phase::Registered);
            }
            _ => {}
        }
    }

    /// Notes since when each registered or draining machine whose host the registry does
    /// not hear from, or does not show, has been silent.
    pub(crate) fn track_silent(&mut self, pool: &PoolInput) {
        let input = self.input;
        for i in self.indices(&pool.settings.pool) {
            let record = &self.records.instances[i];
            let silent =
                host_named(input, &record.name).is_some_and(|host| host.state != HostState::Live);
            let held = [Phase::Registered, Phase::Draining].contains(&record.phase);
            if [silent, held].into_iter().all(|ok| ok) {
                let name = record.name.clone();
                let since = self.silent.get(&name).copied().unwrap_or(self.now);
                self.silent_now.insert(name, since);
            }
        }
    }
}
