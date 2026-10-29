//! The second half of a step: which waiting requests are already covered, what to drain,
//! release and launch, and what the step leaves behind.

use std::collections::BTreeSet;
use std::time::Duration;

use js_elastic::{AcquireId, HostName, HostRef, Offer};

use crate::caps::{check, choose, nearest_miss};
use crate::input::{HostState, Input, Mark, PoolInput, Waiting};
use crate::pass::{Pass, host_named};
use crate::record::{CooldownReason, Phase, Record};
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
/// or record.
#[derive(Default)]
struct Claims {
    hosts: BTreeSet<String>,
    records: BTreeSet<usize>,
}

/// The actions that start a launch: the join token, then the acquire.
fn launch_actions(record: &Record, valid_for: Duration) -> impl Iterator<Item = Action> {
    let mint = Action::Mint {
        host: record.name.clone(),
        valid_for,
    };
    let acquire = record.acquire.clone().map(|acquire| {
        Action::Acquire(Box::new(AcquirePlan {
            acquire,
            pool: record.pool.clone(),
            name: record.name.clone(),
            offer: record.offer.to_offer(record.price_cents_per_hour),
        }))
    });
    [Some(mint), acquire].into_iter().flatten()
}

impl Pass<'_> {
    /// Scales one pool: matches `waiting` to what is coming, drains and releases what is
    /// idle, and launches for what is left. Returns the requests this pool did not take.
    pub(crate) fn scale<'w>(
        &mut self,
        pool: &PoolInput,
        waiting: Vec<&'w Waiting>,
    ) -> Vec<&'w Waiting> {
        let mut claims = Claims::default();
        let uncovered = self.cover(pool, waiting, &mut claims);
        let uncovered = self.undrain(pool, uncovered, &mut claims);
        self.advance_draining(pool);
        self.drain_idle(pool, &claims);
        self.release(pool);
        self.launch(pool, uncovered)
    }

    /// Matches requests greedily, in queue order, to a free host that fits and is not set
    /// aside, then to a machine on its way whose offer fits. A request left over is
    /// uncovered.
    fn cover<'w>(
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

    /// Takes back a draining host that an uncovered request fits: reuse beats a launch.
    fn undrain<'w>(
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
                    record.phase == Phase::Draining,
                    !record.condemned,
                    !claims.hosts.contains(&record.name),
                    serves,
                ]
                .into_iter()
                .all(|ok| ok)
            });
            if let Some(i) = hit {
                let host = self.records.instances[i].name.clone();
                self.go(i, Phase::Registered);
                claims.hosts.insert(host.clone());
                self.actions.push(Action::Undrain(host));
            } else {
                left.push(request);
            }
        }
        left
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
                    self.start_release(i);
                } else {
                    self.quiet_now.insert(name);
                }
            }
        }
    }

    /// Asks for the release of every machine that is being released, every step, until a
    /// read by id says it is gone.
    fn release(&mut self, pool: &PoolInput) {
        let releases: Vec<Action> = self
            .records
            .instances
            .iter()
            .filter(|record| {
                (record.pool.as_str(), record.phase)
                    == (pool.settings.pool.as_str(), Phase::Releasing)
            })
            .map(|record| Action::Release {
                host: record.name.clone(),
                id: HostRef(record.cloud_id.clone().unwrap_or_default()),
            })
            .collect();
        self.actions.extend(releases);
    }

    /// Why the pool may not launch now, other than its caps.
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

    /// Retries the launches the provider told to wait, once their time has come.
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
        for &i in &due {
            self.go(i, Phase::Launching);
            let now = self.now;
            let record = &mut self.records.instances[i];
            record.launched_at = now;
            record.retry_at = None;
            record.token_expires = Some(now.plus(settings.boot_timeout));
            self.actions
                .extend(launch_actions(record, settings.boot_timeout));
        }
        due.len()
    }

    /// Launches, up to `max_step`, for the requests nothing covers. A pool in `Plan`
    /// mode records what it would launch and launches nothing.
    fn launch<'w>(&mut self, pool: &PoolInput, uncovered: Vec<&'w Waiting>) -> Vec<&'w Waiting> {
        let settings = &pool.settings;
        if let Some(blocked) = self.gate(pool) {
            self.blocked.insert(settings.pool.clone(), blocked);
            return uncovered;
        }
        let retried = self.retry_planned(settings);
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
        if settings.launch == LaunchMode::Live {
            if let Some(blocked) = stopped {
                self.blocked.insert(settings.pool.clone(), blocked);
            }
            for record in staged {
                self.records.launches += 1;
                self.actions
                    .extend(launch_actions(&record, settings.boot_timeout));
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
    /// `staged` ones.
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
        let number = self.records.launches + u64::try_from(staged.len()).unwrap_or_default();
        let text = format!("{}-{}-{number}", settings.pool, self.now.as_millis());
        let acquire = AcquireId::new(&text).map_err(|_| Blocked::PoolName)?;
        let name = HostName::derive(&self.input.scheduler, &acquire).to_string();
        Ok(Record {
            pool: settings.pool.clone(),
            acquire: Some(acquire),
            name,
            offer: crate::record::StoredOffer::from_offer(offer),
            price_cents_per_hour: price,
            phase: Phase::Launching,
            since: self.now,
            launched_at: self.now,
            token_expires: Some(self.now.plus(settings.boot_timeout)),
            cloud_id: None,
            retry_at: None,
            gone_at: None,
            registered: false,
            condemned: false,
            clean_lists: 0,
            last_clean: None,
        })
    }

    /// Ends the step: forgets the records that no longer count and have left the day
    /// window, puts every changed record first among the actions, and names the requests
    /// no offer fits.
    pub(crate) fn finish(mut self, before: &[Record]) -> (State, Vec<Action>) {
        let now = self.now;
        self.records.instances.retain(|record| {
            [
                record.counts(),
                now.since(record.launched_at) < crate::caps::DAY,
            ]
            .into_iter()
            .any(|keep| keep)
        });
        let mut actions: Vec<Action> = self
            .records
            .instances
            .iter()
            .filter(|record| !before.contains(record))
            .map(|record| Action::Record(Box::new(record.clone())))
            .collect();
        actions.append(&mut self.actions);
        let input: &Input = self.input;
        let no_offer = input
            .waiting
            .iter()
            .filter(|request| request.reserved.is_none())
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
        };
        (state, actions)
    }
}
