//! The second half of a step: which waiting requests are already covered, what to drain,
//! release and launch, and what the step leaves behind.

use std::collections::BTreeSet;
use std::time::Duration;

use js_elastic::{AcquireId, HostName, HostRef, Offer};

use crate::caps::{billed_until, check, choose, nearest_miss};
use crate::input::{HostState, Input, Mark, PoolInput, Waiting};
use crate::pass::{Pass, host_named};
use crate::record::{CooldownReason, Phase, Record, Records};
use crate::settings::{Blocked, Health, LaunchMode, Settings};
use crate::state::{AcquirePlan, Action, PlanLine, State};

/// The phases of a machine that is on its way and so covers a request that fits it.
const COMING: [Phase; 4] = [
    Phase::Planned,
    Phase::Launching,
    Phase::Unresolved,
    Phase::Booting,
];

/// What the requests of this step have already been matched to, one request to one host
/// or record, across all the pools.
#[derive(Default)]
pub(crate) struct Claims {
    hosts: BTreeSet<String>,
    records: BTreeSet<usize>,
}

/// The actions that start a launch: the join token, valid for `valid_for`, then the
/// acquire.
fn launch_actions(record: &Record, valid_for: Duration) -> impl Iterator<Item = Action> {
    let mint = Action::Mint {
        host: record.name.clone(),
        valid_for,
    };
    let acquire = record.acquire.clone().map(|acquire| {
        Action::Acquire(Box::new(AcquirePlan {
            acquire,
            attempt: record.attempt,
            pool: record.pool.clone(),
            name: record.name.clone(),
            offer: record.offer.to_offer(record.price_cents_per_hour),
        }))
    });
    [Some(mint), acquire].into_iter().flatten()
}

impl Pass<'_> {
    /// The second half of a step for one pool, once every request has been matched to what
    /// covers it: notes the silent hosts, takes back the hosts whose undrain was lost,
    /// finishes the drains, drains the idle and asks for every release.
    pub(crate) fn settle(&mut self, pool: &PoolInput, claims: &Claims) {
        self.track_silent(pool);
        self.heal_undrain(pool, claims);
        self.advance_draining(pool);
        self.drain_idle(pool, claims);
        self.release(pool);
    }

    /// Matches requests greedily, in queue order, to a free host that fits and is not set
    /// aside, then to a machine on its way whose offer fits. A request left over is
    /// uncovered.
    pub(crate) fn cover<'w>(
        &self,
        pool: &PoolInput,
        waiting: Vec<&'w Waiting>,
        claims: &mut Claims,
    ) -> Vec<&'w Waiting> {
        let name = pool.settings.pool.as_str();
        let mut uncovered = Vec::new();
        for request in waiting {
            let host = self.input.hosts.iter().find(|host| {
                let condemned = self.records.instances.iter().any(|record| {
                    (record.condemned, record.name.as_str()) == (true, host.id.as_str())
                });
                [
                    host.pool == name,
                    host.state == HostState::Live,
                    host.mark.is_none(),
                    !host.busy,
                    !condemned,
                    !claims.hosts.contains(host.id.as_str()),
                    host.shape.fit(&request.demand).is_ok(),
                ]
                .into_iter()
                .all(|ok| ok)
            });
            if let Some(host) = host {
                claims.hosts.insert(host.id.as_str().to_owned());
                continue;
            }
            let record = self
                .records
                .instances
                .iter()
                .enumerate()
                .position(|(i, record)| {
                    [
                        record.pool == name,
                        COMING.contains(&record.phase),
                        !claims.records.contains(&i),
                        record.offer.to_offer(0).fit(&request.demand).is_ok(),
                    ]
                    .into_iter()
                    .all(|ok| ok)
                });
            if let Some(i) = record {
                claims.records.insert(i);
                continue;
            }
            uncovered.push(request);
        }
        uncovered
    }

    /// Forgets the launches the provider told to wait that no request waits for any more:
    /// no waiting request was matched to them. Nothing was sent for one, so its token is
    /// revoked and its network key forgotten and the record simply goes; kept, it would be
    /// retried at its time and launch a machine for nobody, or, in a pool that does not
    /// launch, stay counted for good.
    ///
    /// A record whose acquire id the provider holds an unresolved call for
    /// ([`PoolInput::unresolved`]) is kept even when nobody waits for it: an earlier call
    /// of the same acquire may have landed, so a machine may exist, and the retry at the
    /// record's time (the same acquire id, which the provider answers for what it
    /// launched) is what settles it. Dropped, the machine would show up later as an orphan.
    ///
    /// Runs once every request has been matched, before any record is added or removed, so
    /// the positions in `claims` still hold.
    pub(crate) fn drop_unwanted(&mut self, claims: &Claims) {
        let unwanted: Vec<usize> = (0..self.records.instances.len())
            .filter(|i| {
                let record = &self.records.instances[*i];
                let unresolved = self.input.pools.iter().any(|pool| {
                    [
                        pool.settings.pool == record.pool,
                        record
                            .acquire
                            .iter()
                            .any(|acquire| pool.unresolved.contains(acquire)),
                    ]
                    .into_iter()
                    .all(|ok| ok)
                });
                [
                    record.phase == Phase::Planned,
                    !claims.records.contains(i),
                    !unresolved,
                ]
                .into_iter()
                .all(|ok| ok)
            })
            .collect();
        for i in unwanted.into_iter().rev() {
            self.clean_up(i);
            self.records.instances.remove(i);
        }
    }

    /// Takes back a draining host that an uncovered request fits: reuse beats a launch. A
    /// registered host the registry still shows draining (an earlier undrain was lost) is
    /// taken back the same way, with no change to its record.
    pub(crate) fn undrain<'w>(
        &mut self,
        pool: &PoolInput,
        uncovered: Vec<&'w Waiting>,
        claims: &mut Claims,
    ) -> Vec<&'w Waiting> {
        let input = self.input;
        let name = pool.settings.pool.as_str();
        let mut left = Vec::new();
        for request in uncovered {
            let hit = self.records.instances.iter().position(|record| {
                let serves = host_named(input, &record.name).is_some_and(|host| {
                    [
                        host.state == HostState::Live,
                        host.mark == Some(Mark::Draining),
                        host.shape.fit(&request.demand).is_ok(),
                    ]
                    .into_iter()
                    .all(|ok| ok)
                });
                [
                    record.pool == name,
                    [Phase::Draining, Phase::Registered].contains(&record.phase),
                    !record.condemned,
                    !claims.hosts.contains(&record.name),
                    serves,
                ]
                .into_iter()
                .all(|ok| ok)
            });
            if let Some(i) = hit {
                let host = self.records.instances[i].name.clone();
                if self.records.instances[i].phase == Phase::Draining {
                    self.go(i, Phase::Registered);
                }
                claims.hosts.insert(host.clone());
                self.actions.push(Action::Undrain(host));
            } else {
                left.push(request);
            }
        }
        left
    }

    /// Undoes a drain the scaler took back and the registry never heard of: a registered
    /// host the registry shows draining is asked to undrain again, every step, until the
    /// mark is gone. Without this the host is marked, takes no work, is not idle (it has a
    /// mark) and is never released, yet is paid for.
    fn heal_undrain(&mut self, pool: &PoolInput, claims: &Claims) {
        let input = self.input;
        let stuck: Vec<String> = self
            .indices(&pool.settings.pool)
            .into_iter()
            .map(|i| &self.records.instances[i])
            .filter(|record| {
                let marked = host_named(input, &record.name).is_some_and(|host| {
                    (host.state, host.mark) == (HostState::Live, Some(Mark::Draining))
                });
                [
                    record.phase == Phase::Registered,
                    marked,
                    !claims.hosts.contains(&record.name),
                ]
                .into_iter()
                .all(|ok| ok)
            })
            .map(|record| record.name.clone())
            .collect();
        self.actions.extend(stuck.into_iter().map(Action::Undrain));
    }

    /// For each draining host whose mark the registry shows: ask `release_host` while it
    /// is quiet, take the host back if work has reached it.
    fn advance_draining(&mut self, pool: &PoolInput) {
        let input = self.input;
        let draining: Vec<usize> = self
            .indices(&pool.settings.pool)
            .into_iter()
            .filter(|&i| self.records.instances[i].phase == Phase::Draining)
            .collect();
        for i in draining {
            let name = self.records.instances[i].name.clone();
            let marked = host_named(input, &name)
                .filter(|host| (host.state, host.mark) == (HostState::Live, Some(Mark::Draining)));
            if let Some(host) = marked {
                if host.busy {
                    self.go(i, Phase::Registered);
                    self.actions.push(Action::Undrain(name));
                } else {
                    self.actions.push(Action::ReleaseHost(name));
                }
            }
        }
    }

    /// Drains a registered host that has been quiet for `idle` and that no request is
    /// matched to. A condemned host is drained as soon as it is quiet.
    fn drain_idle(&mut self, pool: &PoolInput, claims: &Claims) {
        let input = self.input;
        let registered: Vec<usize> = self
            .indices(&pool.settings.pool)
            .into_iter()
            .filter(|&i| self.records.instances[i].phase == Phase::Registered)
            .collect();
        for i in registered {
            let record = &self.records.instances[i];
            let name = record.name.clone();
            let condemned = record.condemned;
            let quiet = host_named(input, &name).is_some_and(|host| {
                [
                    host.state == HostState::Live,
                    host.mark.is_none(),
                    !host.busy,
                ]
                .into_iter()
                .all(|ok| ok)
            });
            if [quiet, !claims.hosts.contains(&name)]
                .into_iter()
                .all(|ok| ok)
            {
                let since = *self.quiet.entry(name.clone()).or_insert(self.now);
                let ripe = self.now.since(since) >= pool.settings.idle;
                if [condemned, ripe].into_iter().any(|ok| ok) {
                    self.go(i, Phase::Draining);
                    // A condemned host is also marked for release at once: each call is
                    // checked by the registry while the host is quiet, so no work is cut.
                    let release = condemned.then_some(Action::ReleaseHost(name.clone()));
                    self.actions.push(Action::Drain(name));
                    self.actions.extend(release);
                } else {
                    self.quiet_now.insert(name);
                }
            }
        }
    }

    /// Asks for the release of every machine that is being released, every step, until a
    /// read by id says it is gone. A machine with no provider id cannot be released by id;
    /// that is refused and counted, never sent as an empty id.
    fn release(&mut self, pool: &PoolInput) {
        let releasing: Vec<(String, Option<String>)> = self
            .records
            .instances
            .iter()
            .filter(|record| {
                (record.pool.as_str(), record.phase)
                    == (pool.settings.pool.as_str(), Phase::Releasing)
            })
            .map(|record| (record.name.clone(), record.cloud_id.clone()))
            .collect();
        for (host, cloud_id) in releasing {
            match cloud_id {
                Some(id) => self.actions.push(Action::Release {
                    host,
                    id: HostRef(id),
                }),
                None => self.refused += 1,
            }
        }
    }

    /// Why the pool may not launch now, other than its caps and a deferred launch.
    fn gate(&self, pool: &PoolInput) -> Option<Blocked> {
        let health = match pool.health {
            Health::Checking => Some(Blocked::Checking),
            Health::Failing => Some(Blocked::Unavailable),
            Health::Available => None,
        };
        let list = pool.held.is_none().then_some(Blocked::ListFailed);
        let cooling = self
            .records
            .cooldowns
            .get(&pool.settings.pool)
            .filter(|cooldown| cooldown.until > self.now)
            .map(|cooldown| match cooldown.reason {
                CooldownReason::BootFailed => Blocked::BootFailed,
                CooldownReason::Quota => Blocked::Quota,
                CooldownReason::Denied => Blocked::Denied,
                CooldownReason::Shape => Blocked::Shape,
            });
        health.or(list).or(cooling)
    }

    /// Whether a launch of the pool is waiting for the time the provider named: the pool
    /// launches nothing else until the clock passes it.
    fn deferred(&self, pool: &str) -> bool {
        self.records.instances.iter().any(|record| {
            [
                record.pool == pool,
                record.phase == Phase::Planned,
                record.retry_at.is_some_and(|at| at > self.now),
            ]
            .into_iter()
            .all(|ok| ok)
        })
    }

    /// Retries the launches the provider told to wait, once their time has come. A retry is
    /// a launch: it is checked against every cap again, with the machines of the pool other
    /// than itself, and one that does not fit stays planned and shows the cap. Returns how
    /// many were sent.
    fn retry_planned(&mut self, settings: &Settings) -> usize {
        let due: Vec<usize> = self
            .indices(&settings.pool)
            .into_iter()
            .filter(|&i| {
                let record = &self.records.instances[i];
                [
                    record.phase == Phase::Planned,
                    record.retry_at.is_some_and(|at| at <= self.now),
                ]
                .into_iter()
                .all(|ok| ok)
            })
            .take(settings.max_step)
            .collect();
        let mut sent = 0;
        for i in due {
            let others: Vec<Record> = self
                .records
                .instances
                .iter()
                .enumerate()
                .filter(|(each, _)| *each != i)
                .map(|(_, record)| record.clone())
                .collect();
            let price = self.records.instances[i].price_cents_per_hour;
            match check(settings, self.now, &others, &[], &BTreeSet::new(), price) {
                Ok(()) => {
                    self.go(i, Phase::Launching);
                    let now = self.now;
                    let record = &mut self.records.instances[i];
                    record.launched_at = now;
                    record.retry_at = None;
                    record.attempt += 1;
                    record.token_expires = Some(now.plus(settings.token_life()));
                    self.actions
                        .extend(launch_actions(record, settings.token_life()));
                    sent += 1;
                }
                Err(blocked) => {
                    self.blocked.insert(settings.pool.clone(), blocked);
                }
            }
        }
        sent
    }

    /// Launches, up to `max_step`, for the requests nothing covers. A pool in `Plan`
    /// mode records what it would launch and launches nothing, and retries nothing.
    pub(crate) fn launch<'w>(
        &mut self,
        pool: &PoolInput,
        uncovered: Vec<&'w Waiting>,
    ) -> Vec<&'w Waiting> {
        let settings = &pool.settings;
        let live = settings.launch == LaunchMode::Live;
        if let Some(blocked) = self.gate(pool) {
            self.blocked.insert(settings.pool.clone(), blocked);
            return uncovered;
        }
        let retried = if live {
            self.retry_planned(settings)
        } else {
            0
        };
        if self.deferred(&settings.pool) {
            self.blocked
                .insert(settings.pool.clone(), Blocked::Deferred);
            return uncovered;
        }
        let mut staged: Vec<Record> = Vec::new();
        let mut served: Vec<&Waiting> = Vec::new();
        let mut left = Vec::new();
        let mut stopped = None;
        for request in uncovered {
            let room = [
                stopped.is_none(),
                retried + staged.len() < settings.max_step,
            ]
            .into_iter()
            .all(|ok| ok);
            let offer =
                choose(settings, &pool.offers, &request.demand, &self.declined).filter(|_| room);
            match offer.map(|offer| self.stage(settings, offer, &staged)) {
                Some(Ok(record)) => {
                    staged.push(record);
                    served.push(request);
                }
                Some(Err(blocked)) => {
                    stopped = Some(blocked);
                    left.push(request);
                }
                None => left.push(request),
            }
        }
        if live {
            if let Some(blocked) = stopped {
                self.blocked.insert(settings.pool.clone(), blocked);
            }
            self.drawn += staged.len();
            for record in staged {
                self.actions
                    .extend(launch_actions(&record, settings.token_life()));
                self.records.instances.push(record);
            }
        } else {
            self.blocked.insert(settings.pool.clone(), Blocked::Plan);
            for (record, request) in staged.into_iter().zip(served) {
                self.plan.push(PlanLine {
                    pool: settings.pool.clone(),
                    request: request.id.clone(),
                    offer: record.offer.id,
                    zone: record.offer.zone,
                    price_cents_per_hour: record.price_cents_per_hour,
                });
            }
        }
        left
    }

    /// The record of a launch of `offer`, if the caps allow one more machine after the
    /// `staged` ones and the step has a fresh random value for its acquire id: the pool's
    /// name and the 128 bits in hex. A value that an existing record or a staged launch
    /// already used is a repeat, and is refused.
    fn stage(
        &self,
        settings: &Settings,
        offer: &Offer,
        staged: &[Record],
    ) -> Result<Record, Blocked> {
        let price = offer.price_cents_per_hour.unwrap_or_default();
        check(
            settings,
            self.now,
            &self.records.instances,
            staged,
            &BTreeSet::new(),
            price,
        )?;
        let bits = self
            .input
            .entropy
            .get(self.drawn + staged.len())
            .ok_or(Blocked::Entropy)?;
        let acquire = AcquireId::new(&format!(
            "{}-{:032x}",
            settings.pool,
            u128::from_be_bytes(*bits)
        ))
        .map_err(|_| Blocked::PoolName)?;
        let repeat = self
            .records
            .instances
            .iter()
            .chain(staged)
            .any(|record| record.acquire.as_ref() == Some(&acquire));
        let name = HostName::derive(&self.input.scheduler, &acquire).to_string();
        let record = Record {
            pool: settings.pool.clone(),
            acquire: Some(acquire),
            name,
            offer: crate::record::StoredOffer::from_offer(offer),
            price_cents_per_hour: price,
            phase: Phase::Launching,
            since: self.now,
            launched_at: self.now,
            token_expires: Some(self.now.plus(settings.token_life())),
            cloud_id: None,
            retry_at: None,
            attempt: 0,
            gone_at: None,
            registered: false,
            condemned: false,
            clean_lists: 0,
            last_clean: None,
            seen_live: false,
        };
        (!repeat).then_some(record).ok_or(Blocked::Entropy)
    }

    /// Ends the step: forgets the records that no longer count and whose billed time has
    /// left the day window, and the refused ones; writes the records first among the
    /// actions if the step changed them; and names the requests no offer fits, among those
    /// nothing covered.
    pub(crate) fn finish(
        mut self,
        before: &Records,
        uncovered: &[&Waiting],
    ) -> (State, Vec<Action>) {
        let now = self.now;
        let input: &Input = self.input;
        self.records.instances.retain(|record| {
            let boot_timeout = input
                .pools
                .iter()
                .find(|pool| pool.settings.pool == record.pool)
                .map_or(Duration::ZERO, |pool| pool.settings.boot_timeout);
            // A refused acquire never was a machine and costs nothing: it goes at once, so
            // a pool that is refused over and over does not grow its records.
            let costs = [
                record.counts(),
                billed_until(record, boot_timeout, now) > now.back(crate::caps::DAY),
            ];
            [
                record.phase != Phase::Refused,
                costs.into_iter().any(|keep| keep),
            ]
            .into_iter()
            .all(|keep| keep)
        });
        let save = (self.records != *before).then(|| Action::Save(Box::new(self.records.clone())));
        let mut actions: Vec<Action> = save.into_iter().collect();
        actions.append(&mut self.actions);
        let no_offer = uncovered
            .iter()
            .filter_map(|request| {
                let offers = input.pools.iter().flat_map(|pool| pool.offers.iter());
                nearest_miss(offers, &request.demand).map(|miss| (request.id.clone(), miss))
            })
            .collect();
        let quiet = std::mem::take(&mut self.quiet)
            .into_iter()
            .filter(|(name, _)| self.quiet_now.contains(name))
            .collect();
        let state = State {
            records: self.records,
            blocked: self.blocked,
            plan: self.plan,
            no_offer,
            refused: self.refused,
            quiet,
            silent: self.silent_now,
            silent_released: self.silent_released,
            missing: self.missing_now,
        };
        (state, actions)
    }
}
