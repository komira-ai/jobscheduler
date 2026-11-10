//! The status of the elastic pools: `GET /v1/pools` as JSON and `GET /status` as the
//! same data in a page.
//!
//! Both are served on `server.listen` only, never on the agent port, and only when that
//! address is a private one (see [`crate::Config::serves_status`]), and then only to a
//! peer whose address is private too: the check is on each request, by the address of
//! the connection, so a wildcard listener does not serve them to the internet. They take
//! no token: who may reach them is the network's to decide, as for `/healthz`. Nothing here holds a
//! secret to leak: the plan of the next launch is the masked render of a plan that has
//! no secret in it, and the checks' reasons are messages written to be shown.
//!
//! With `[logs] sink = "object_store"` both also show the counters of the host credentials
//! of live logs (`logs`: mints that succeeded and failed since the start, the hosts holding a
//! credential and those whose last mint failed); they hold no credential, and without a sink
//! the field is left out, so the reply is as it was before it.
//!
//! Both also show the health of the store paths of the tick (`store`, see
//! [`crate::store_health`]): the failed ticks in a row, the kind of the last failure, the last
//! tick whose store work succeeded, the job pass's latency, the writer lock, and the
//! `store:unreachable` alert while it holds. It holds no error text, only a kind.
//!
//! The machines, the figures of what they cost, the orphans and the last launch are the
//! scaler's records for the pool (what its last tick left), so they are empty for a pool
//! that has launched nothing, and in `plan` mode. So are the offer cool-downs
//! (`offer_cooldowns`: provider, instance type, zone, `until_ms`, the host and the
//! dimensions its hello failed), which are left out of the reply while there is none. A record holds no secret: it names a
//! machine, its provider id, its offer and its price. `idle_since_ms` of the design is not
//! shown: the scaler does not give out since when a host has been quiet.

use std::fmt::Write;
use std::sync::{Arc, PoisonError};

use js_elastic::{Demand, Instant, OfferKey};
use js_scaler::{
    Brake, Cooldown, HelloDimension, LaunchMode, Millis, Phase, Record, Records,
    State as ScalerState,
};
use js_sched::failover::{AcquireMemory, PassedOver};
use js_types::{Clock, Timestamp};
use js_types::{ExecMode, ProviderId, RequestId};
use serde::Serialize;

use crate::catalogs::Catalogs;
use crate::checks::{Checked, State, Verdict};
use crate::clock::SharedClock;
use crate::hosts::MemoryView;
use crate::logs::{LogCounters, LogGrants};
use crate::pool::{Next, NextLaunch, NoOffer, Pool, Wanted};
use crate::scaler::{InFlight, Scaler};
use crate::store_health::{self, StoreHealth, StoreView};

/// What the status routes read.
pub(crate) struct Status {
    pool: Option<Pool>,
    scaler: Option<Scaler>,
    warnings: Vec<String>,
    clock: SharedClock,
    logs: Option<Arc<LogGrants>>,
    store: Option<Arc<StoreHealth>>,
    catalogs: Option<Arc<Catalogs>>,
}

/// The reply of `GET /v1/pools`.
#[derive(Debug, Serialize)]
pub(crate) struct PoolsView {
    pub(crate) pools: Vec<PoolView>,
    /// `server: warning: ...` lines, such as a listener on every interface.
    pub(crate) warnings: Vec<String>,
    /// The counters of the host credentials of live logs; left out without a sink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) logs: Option<LogCounters>,
    /// The health of the store paths; left out only by a status built without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) store: Option<StoreView>,
}

/// One pool.
#[derive(Debug, Serialize)]
pub(crate) struct PoolView {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) network: &'static str,
    /// The execution mode the hosts must report, or `none` for a pool that asked for none.
    pub(crate) exec: String,
    /// The mode the pool is in now: `plan` once its `live_until` has passed, whatever the
    /// file says.
    pub(crate) launch: &'static str,
    /// When the pool's go ends, in milliseconds since the epoch, if the file sets one.
    pub(crate) live_until_ms: Option<u64>,
    /// The memory of the pool's floor, the percent of it the pool books and what that
    /// comes to.
    pub(crate) memory: MemoryView,
    pub(crate) state: State,
    /// Why the pool is unavailable, one line for each check that failed.
    pub(crate) reasons: Vec<String>,
    pub(crate) credentials: Option<CheckView>,
    pub(crate) network_check: Option<CheckView>,
    /// The provider's own checks of the last pass.
    pub(crate) provider_checks: Vec<NamedCheckView>,
    pub(crate) instances: Instances,
    pub(crate) blocked: Option<&'static str>,
    pub(crate) no_offer: Option<NoOffer>,
    pub(crate) last_launch: Option<LaunchView>,
    pub(crate) machines: Vec<MachineView>,
    pub(crate) orphans: Vec<OrphanView>,
    /// The offers of the pool cooling down because a machine's hello did not match them
    /// (design 5.6.1, step 6). Left out while there is none, so the reply is then as it was
    /// before the field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) offer_cooldowns: Vec<OfferCooldownView>,
    /// What `ListProviders` reads of the pool's provider besides its checks and machines.
    /// Not written in the body of `GET /v1/pools`, which stays as it was.
    #[serde(skip)]
    pub(crate) provider_state: ProviderState,
    /// What an operator must act on, as the scaler's last tick left it: a stuck revoke or
    /// removal, a release the provider does not confirm, an orphan, a silent host that
    /// nothing releases.
    pub(crate) alerts: Vec<String>,
    /// The server's time at the end of the scaler's last finished tick, in milliseconds;
    /// null before the first. A watcher outside the server reads it as the heartbeat.
    pub(crate) last_tick_ms: Option<u64>,
    /// Whether the pool has a scaler: it ticks, so `last_tick_ms` is a heartbeat that must
    /// move. A pool without one (a plan-only or joined pool) has no heartbeat.
    pub(crate) scaler: bool,
    pub(crate) next_launch: Option<NextLaunch>,
}

/// One check.
#[derive(Debug, Serialize)]
pub(crate) struct CheckView {
    pub(crate) ok: bool,
    pub(crate) reason: Option<String>,
    pub(crate) checked_at_ms: u64,
}

/// The newest launch of the pool.
#[derive(Debug, Serialize)]
pub(crate) struct LaunchView {
    pub(crate) at_ms: u64,
    /// `accepted` once the provider has given an id for the machine, `pending` before.
    pub(crate) outcome: &'static str,
    pub(crate) host: String,
}

/// A machine the scheduler launched and still holds.
#[derive(Debug, Serialize)]
pub(crate) struct MachineView {
    pub(crate) host: String,
    pub(crate) state: Phase,
    pub(crate) offer: String,
    pub(crate) zone: Option<String>,
    pub(crate) price_cents_per_hour: u64,
    /// The provider's id for the machine, once the provider has given one: what a watcher
    /// outside the server matches the provider's list against.
    pub(crate) provider_id: Option<String>,
}

/// A machine of the scheduler's that no record knew: counted, never released by the
/// scaler.
#[derive(Debug, Serialize)]
pub(crate) struct OrphanView {
    pub(crate) host: String,
    pub(crate) provider_id: Option<String>,
}

/// An offer cooling down because the hello of a machine acquired for it did not match it.
/// It holds no credential: it names the offer, the host and what failed.
#[derive(Debug, Serialize)]
pub(crate) struct OfferCooldownView {
    pub(crate) provider: String,
    /// The provider's instance type.
    pub(crate) offer: String,
    pub(crate) zone: Option<String>,
    /// The first instant the offer is no longer cooling down, in milliseconds since the
    /// epoch.
    pub(crate) until_ms: u64,
    /// The host whose hello did not match.
    pub(crate) host: String,
    /// Every dimension that failed, such as `accelerator_model` or `memory`.
    pub(crate) failed: Vec<HelloDimension>,
}

/// What `ListProviders` reads of a pool's provider besides its checks and machines, at the
/// status's time: why it launches nothing now or what it is waiting on. Each field is read
/// through the part of the server that owns it, when the status is read; none is kept here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderState {
    /// The status's time, in milliseconds since the epoch, which the ages are taken from.
    pub(crate) now_ms: u64,
    /// When the catalog read now was fetched, and how many offers it holds
    /// ([`Catalogs::fetched`]); `None` before the first fetch.
    pub(crate) catalog: Option<(Instant, usize)>,
    /// The instance types passed over now after a `quota` answer, each until its end.
    pub(crate) passed_over: Vec<(PassedOver, Instant)>,
    /// The offers declined for capacity in the attempt in progress of a waiting request,
    /// each with the request and the time of the answer.
    pub(crate) declined: Vec<(RequestId, OfferKey, Instant)>,
    /// The pool's failure brake, while the records hold one ([`Records::brake`]).
    pub(crate) brake: Option<Brake>,
    /// The pool's cool-down, while it runs ([`Records::cooldowns`]).
    pub(crate) cooldown: Option<Cooldown>,
    /// The launches whose provider call is out now ([`Scaler::in_flight`]).
    pub(crate) in_flight: Vec<InFlight>,
}

/// One of the provider's own checks.
#[derive(Debug, Serialize)]
pub(crate) struct NamedCheckView {
    pub(crate) name: &'static str,
    pub(crate) ok: bool,
    pub(crate) reason: Option<String>,
    /// What a check that held said, such as what a launch will make.
    pub(crate) note: Option<String>,
    pub(crate) checked_at_ms: u64,
}

fn named_view(checked: Checked) -> NamedCheckView {
    NamedCheckView {
        name: checked.name,
        ok: checked.verdict.reason.is_none(),
        reason: checked.verdict.reason,
        note: checked.note,
        checked_at_ms: checked.verdict.at.millis(),
    }
}

/// The machines the scheduler holds. The spend is the stored price of the machines listed,
/// not of the orphans.
#[derive(Debug, Serialize)]
pub(crate) struct Instances {
    pub(crate) n: usize,
    pub(crate) max: usize,
    pub(crate) spend_cents_per_hour: u64,
    pub(crate) max_spend_cents_per_hour: u64,
}

/// What the next launch is for: a request for the pool floor.
pub(crate) fn wanted(floor: &Demand) -> Wanted {
    Wanted {
        request: "floor".to_owned(),
        demand: floor.clone(),
    }
}

/// How the file spells the execution mode a pool requires.
fn exec_name(exec: Option<ExecMode>) -> String {
    match exec {
        Some(mode) => mode.to_string(),
        None => "none".to_owned(),
    }
}

/// The launch `record` made, as the status shows it.
fn launch_view(record: &Record) -> LaunchView {
    let outcome = match record.cloud_id {
        Some(_) => "accepted",
        None => "pending",
    };
    LaunchView {
        at_ms: record.launched_at.as_millis(),
        outcome,
        host: record.name.clone(),
    }
}

/// What the scaler's records say of the pool `pool`: its machines, its orphans and its
/// newest launch.
fn held(records: &Records, pool: &str) -> (Vec<MachineView>, Vec<OrphanView>, Option<LaunchView>) {
    let ours: Vec<&Record> = records
        .instances
        .iter()
        .filter(|record| record.pool == pool)
        .collect();
    let machines = ours
        .iter()
        .filter(|record| record.counts() && record.phase != Phase::Orphan)
        .map(|record| MachineView {
            host: record.name.clone(),
            state: record.phase,
            offer: record.offer.id.clone(),
            zone: record.offer.zone.clone(),
            price_cents_per_hour: record.price_cents_per_hour,
            provider_id: record.cloud_id.clone(),
        })
        .collect();
    let orphans = ours
        .iter()
        .filter(|record| record.phase == Phase::Orphan)
        .map(|record| OrphanView {
            host: record.name.clone(),
            provider_id: record.cloud_id.clone(),
        })
        .collect();
    let newest = ours
        .iter()
        .filter(|record| record.phase != Phase::Orphan)
        .max_by_key(|record| record.launched_at);
    (machines, orphans, newest.map(|record| launch_view(record)))
}

/// The offer cool-downs the scaler's records hold for the pool `pool`.
fn cooled(records: &Records, pool: &str) -> Vec<OfferCooldownView> {
    records
        .cooled_offers
        .iter()
        .filter(|cooled| cooled.pool == pool)
        .map(|cooled| OfferCooldownView {
            provider: cooled.provider.clone(),
            offer: cooled.offer.clone(),
            zone: cooled.zone.clone(),
            until_ms: cooled.until.as_millis(),
            host: cooled.host.clone(),
            failed: cooled.failed.clone(),
        })
        .collect()
}

fn check_view(verdict: Option<Verdict>) -> Option<CheckView> {
    verdict.map(|verdict| CheckView {
        ok: verdict.reason.is_none(),
        reason: verdict.reason,
        checked_at_ms: verdict.at.millis(),
    })
}

impl Status {
    pub(crate) fn new(
        pool: Option<Pool>,
        scaler: Option<Scaler>,
        warnings: Vec<String>,
        clock: SharedClock,
    ) -> Self {
        Self {
            pool,
            scaler,
            warnings,
            clock,
            logs: None,
            store: None,
            catalogs: None,
        }
    }

    /// The same status, reading the cached catalogs of the providers from `catalogs`.
    #[must_use]
    pub(crate) fn with_catalogs(self, catalogs: Arc<Catalogs>) -> Self {
        Self {
            catalogs: Some(catalogs),
            ..self
        }
    }

    /// The same status, showing the counters of the host credentials of `logs`.
    #[must_use]
    pub(crate) fn with_logs(self, logs: Option<Arc<LogGrants>>) -> Self {
        Self { logs, ..self }
    }

    /// The same status, showing the health of the store paths `store` records.
    #[must_use]
    pub(crate) fn with_store_health(self, store: Arc<StoreHealth>) -> Self {
        Self {
            store: Some(store),
            ..self
        }
    }

    /// The status now.
    pub(crate) fn view(&self) -> PoolsView {
        let pools = self.pool.iter().map(|pool| self.pool_view(pool)).collect();
        PoolsView {
            pools,
            warnings: self.warnings.clone(),
            logs: self.logs.as_ref().map(|logs| logs.counters()),
            store: self.store.as_ref().map(|store| store.view()),
        }
    }

    fn pool_view(&self, pool: &Pool) -> PoolView {
        let health = pool
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let state = health.state();
        let wanted = wanted(&pool.floor_demand);
        let (next_launch, no_offer) = match state {
            State::Checking => (None, None),
            _ => match pool.next_launch(&wanted, &health.offers) {
                Next::Planned(next) => (Some(next), None),
                Next::NoOffer(none) => (None, Some(none)),
            },
        };
        let now = Millis::new(self.clock.now().millis());
        let settings = self.scaler.as_ref().map(Scaler::settings);
        let scaled = self.scaler.as_ref().map(|scaler| scaler.state());
        let tripped = scaled
            .as_ref()
            .is_some_and(|seen| seen.records.tripped(&pool.name));
        let launch = settings.map_or(pool.launch, |set| match (set.mode(now), tripped) {
            (LaunchMode::Live, false) => "live",
            _ => "plan",
        });
        let live_until_ms = settings
            .and_then(|set| set.live_until)
            .map(Millis::as_millis);
        // The checks first; once they pass, the cap that keeps the scaler from launching.
        let capped = scaled
            .as_ref()
            .and_then(|seen| seen.blocked.get(&pool.name));
        let blocked = match state {
            State::Available => capped.and_then(|why| why.shown_name()),
            _ => Some("unavailable"),
        };
        let provider_state = self.provider_state(pool, scaled.as_ref(), now);
        let records = scaled.map(|seen| seen.records).unwrap_or_default();
        let (machines, orphans, last_launch) = held(&records, &pool.name);
        let offer_cooldowns = cooled(&records, &pool.name);
        let spend = machines
            .iter()
            .map(|machine| machine.price_cents_per_hour)
            .sum();
        PoolView {
            name: pool.name.clone(),
            kind: pool.kind.clone(),
            network: pool.network_kind,
            exec: exec_name(pool.exec),
            launch,
            live_until_ms,
            memory: MemoryView::of(pool.memory),
            state,
            reasons: health.reasons(),
            credentials: check_view(health.credentials),
            network_check: check_view(health.network),
            provider_checks: health.provider.into_iter().map(named_view).collect(),
            instances: Instances {
                n: machines.len(),
                max: pool.max_instances,
                spend_cents_per_hour: spend,
                max_spend_cents_per_hour: pool.max_spend_per_hour_cents,
            },
            blocked,
            no_offer,
            last_launch,
            machines,
            orphans,
            offer_cooldowns,
            provider_state,
            alerts: self.scaler.as_ref().map(Scaler::alerts).unwrap_or_default(),
            scaler: self.scaler.is_some(),
            last_tick_ms: self
                .scaler
                .as_ref()
                .and_then(Scaler::last_tick)
                .map(Millis::as_millis),
            next_launch,
        }
    }

    /// What `ListProviders` reads of the provider of `pool` at `now`: from the cached
    /// catalogs, from what the scaler's last tick left (`scaled`: its failover memory, the
    /// pool's brake and cool-down in its records) and from the scaler's calls out now. Only
    /// the entries of the pool's own provider and pool are taken.
    fn provider_state(
        &self,
        pool: &Pool,
        scaled: Option<&ScalerState>,
        now: Millis,
    ) -> ProviderState {
        let id = pool.provider_id();
        let catalogs = self.catalogs.as_ref();
        let seen = scaled.map(|seen| (seen.failover(), &seen.records));
        let held = Held {
            catalog: catalogs.and_then(|catalogs| catalogs.fetched(&id)),
            seen,
            out: self.scaler.iter().flat_map(Scaler::in_flight).collect(),
        };
        held.of((&id, &pool.name), now)
    }

    /// The time the status is read at, for the page.
    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }
}

/// What the status reads for `ListProviders` before it keeps a pool's own: the cached
/// catalog of the pool's provider, the scaler's failover memory and records as its last
/// tick left them (`None` without a scaler), and the scaler's launches out.
struct Held<'a> {
    catalog: Option<(Instant, usize)>,
    seen: Option<(&'a AcquireMemory, &'a Records)>,
    out: Vec<InFlight>,
}

impl Held<'_> {
    /// The provider state of the provider `id` of the pool `pool` at `now`: the pass-overs
    /// and declines of that provider only (the pass-overs that hold at `now`), the brake
    /// and cool-down of that pool only (the cool-down only while it runs), and the launches
    /// out for that pool only.
    fn of(self, (id, pool): (&ProviderId, &str), now: Millis) -> ProviderState {
        let at = Instant::from_millis(now.as_millis());
        let memory = self.seen.map(|(memory, _)| memory);
        let passed = memory.into_iter().flat_map(|memory| memory.passed_over(at));
        let passed = passed.filter(|(key, _)| key.provider == *id);
        let declined = memory.into_iter().flat_map(|memory| memory.declined());
        let declined = declined.filter(|(_, key, _)| key.provider == *id);
        let records = self.seen.map(|(_, records)| records);
        let cooldown = records.and_then(|records| records.cooldowns.get(pool));
        ProviderState {
            now_ms: now.as_millis(),
            catalog: self.catalog,
            passed_over: passed.map(|(key, until)| (key.clone(), until)).collect(),
            declined: declined
                .map(|(request, key, when)| (request.clone(), key.clone(), when))
                .collect(),
            brake: records.and_then(|records| records.brake.get(pool).copied()),
            cooldown: cooldown.copied().filter(|cooldown| cooldown.until > now),
            in_flight: self
                .out
                .into_iter()
                .filter(|flight| flight.pool == pool)
                .collect(),
        }
    }
}

/// `text` as HTML text: `&`, `<`, `>` and both quotes are escaped.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The same data as a page. Every value is escaped; the page has no script and no link.
pub(crate) fn page(view: &PoolsView, at: Timestamp) -> String {
    let mut out = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>jobscheduler: pools</title></head><body>\n<h1>Pools</h1>\n",
    );
    let _ = writeln!(out, "<p>read at {} ms</p>", at.millis());
    for warning in &view.warnings {
        let _ = writeln!(out, "<p><strong>{}</strong></p>", escape(warning));
    }
    for pool in &view.pools {
        pool_section(&mut out, pool);
    }
    view.logs
        .iter()
        .for_each(|logs| logs_section(&mut out, logs));
    view.store
        .iter()
        .for_each(|store| store_section(&mut out, store));
    out.push_str("</body></html>\n");
    out
}

/// The counters of the host credentials of live logs.
fn logs_section(out: &mut String, logs: &LogCounters) {
    out.push_str("<h2>Live logs</h2>\n<table>\n");
    row(out, "credentials minted", &logs.minted.to_string());
    row(out, "mints failed", &logs.failed.to_string());
    let held = logs.hosts_with_credential.to_string();
    row(out, "hosts with a credential", &held);
    row(
        out,
        "hosts whose last mint failed",
        &logs.hosts_failing.to_string(),
    );
    out.push_str("</table>\n");
}

/// The health of the store paths.
fn store_section(out: &mut String, store: &StoreView) {
    out.push_str("<h2>Store</h2>\n<table>\n");
    for (label, value) in store_health::rows(store) {
        row(out, label, &value);
    }
    out.push_str("</table>\n");
}

/// One row of a pool's table.
fn row(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(
        out,
        "<tr><th align=\"left\">{}</th><td>{}</td></tr>",
        escape(label),
        escape(value)
    );
}

fn check_text(check: &Option<CheckView>) -> String {
    let Some(check) = check else {
        return "not checked yet".to_owned();
    };
    match &check.reason {
        None => format!("ok at {} ms", check.checked_at_ms),
        Some(reason) => format!("failed at {} ms: {reason}", check.checked_at_ms),
    }
}

fn pool_section(out: &mut String, pool: &PoolView) {
    let _ = writeln!(out, "<h2>{}</h2>\n<table>", escape(&pool.name));
    let state = format!("{:?}", pool.state).to_lowercase();
    row(out, "state", &state);
    row(out, "kind", &pool.kind);
    row(out, "network", pool.network);
    row(out, "exec", &pool.exec);
    row(out, "launch", pool.launch);
    let memory = &pool.memory;
    let booked = format!(
        "{} of {} bytes ({}%)",
        memory.bookable_bytes, memory.configured_bytes, memory.schedulable_percent
    );
    row(out, "memory booked", &booked);
    row(out, "credentials", &check_text(&pool.credentials));
    row(out, "network check", &check_text(&pool.network_check));
    for check in &pool.provider_checks {
        let text = match (&check.reason, &check.note) {
            (Some(reason), _) => format!("failed at {} ms: {reason}", check.checked_at_ms),
            (None, Some(note)) => format!("ok at {} ms: {note}", check.checked_at_ms),
            (None, None) => format!("ok at {} ms", check.checked_at_ms),
        };
        row(out, &format!("check {}", check.name), &text);
    }
    for reason in &pool.reasons {
        row(out, "reason", reason);
    }
    let instances = &pool.instances;
    let held = format!(
        "{} of {}; {} of {} cents an hour",
        instances.n,
        instances.max,
        instances.spend_cents_per_hour,
        instances.max_spend_cents_per_hour
    );
    row(out, "instances", &held);
    row(out, "blocked", pool.blocked.unwrap_or("no"));
    for cooled in &pool.offer_cooldowns {
        let failed: Vec<&str> = cooled.failed.iter().map(|each| each.name()).collect();
        let text = format!(
            "{} of {} in {} until {} ms: host {} did not match in {}",
            cooled.offer,
            cooled.provider,
            cooled.zone.as_deref().unwrap_or("any zone"),
            cooled.until_ms,
            cooled.host,
            failed.join(", ")
        );
        row(out, "offer cooled down", &text);
    }
    for alert in &pool.alerts {
        row(out, "alert", alert);
    }
    if let Some(none) = &pool.no_offer {
        row(
            out,
            "no offer",
            &format!("{}: {}", none.request, none.dimension),
        );
    }
    if let Some(next) = &pool.next_launch {
        row(out, "next offer", &next.offer);
        row(out, "next zone", next.zone.as_deref().unwrap_or("any"));
        row(out, "next name", &next.name);
        row(out, "secret slots", &next.secret_slots.join(", "));
        if let Some(error) = &next.plan_error {
            row(out, "no plan", error);
        }
    }
    out.push_str("</table>\n");
    if let Some(next) = &pool.next_launch
        && let Some(text) = &next.plan
    {
        let _ = writeln!(out, "<h3>Next launch</h3>\n<pre>{}</pre>", escape(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that each character with a meaning in HTML is escaped and any other is
    /// kept, so a pool name or a reason cannot add markup to the page.
    #[test]
    fn the_five_html_characters_are_escaped() {
        assert_eq!(
            escape("<a href=\"x\">&'y'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;y&#39;&lt;/a&gt;"
        );
        assert_eq!(escape("plain 123 é"), "plain 123 é");
    }

    /// An offer `id` in `zone`, of no particular shape.
    fn offer(id: &str, zone: &str) -> js_elastic::Offer {
        js_elastic::Offer {
            zone: Some(zone.to_owned()),
            ..js_scaler::StoredOffer::nothing(id, None).to_offer(100)
        }
    }

    /// A failover memory holding, for the provider `own` and for `foreign`, one offer
    /// declined for capacity at 10 ms (request `r1`) and one instance type passed over for
    /// quota at 20 ms for an hour.
    fn memory(own: &ProviderId, foreign: &ProviderId) -> AcquireMemory {
        let refused = |code| {
            js_elastic::Acquired::Refused(js_elastic::Refusal {
                code,
                reason: js_elastic::Reason::Api("scripted".to_owned()),
            })
        };
        let request = RequestId::new("r1").unwrap();
        let mut memory = AcquireMemory::new();
        let hour = std::time::Duration::from_secs(3600);
        for (n, provider) in [own, foreign].into_iter().enumerate() {
            let (cap, quota) = (format!("acq-c{n}"), format!("acq-q{n}"));
            let cap = js_elastic::AcquireId::new(&cap).unwrap();
            let quota = js_elastic::AcquireId::new(&quota).unwrap();
            let capacity = js_elastic::RefusalCode::Capacity;
            memory.launched(&request, &cap);
            let declined = offer(&format!("{provider}-x"), "z1");
            memory.answered(
                &cap,
                (provider, &declined),
                &refused(capacity),
                at(10),
                hour,
            );
            memory.launched(&request, &quota);
            let passed = offer(&format!("{provider}-y"), "z2");
            let code = js_elastic::RefusalCode::Quota;
            memory.answered(&quota, (provider, &passed), &refused(code), at(20), hour);
        }
        memory
    }

    fn at(millis: u64) -> Instant {
        Instant::from_millis(millis)
    }

    /// Records with a brake and a cool-down for the pool `pool` (the cool-down ending at
    /// `until`) and another of each for the pool `other`.
    fn records(pool: &str, other: &str, until: u64) -> Records {
        let mut records = Records::default();
        let brake = |failures| Brake {
            failures,
            tripped_at: Some(Millis::new(5)),
        };
        records.brake.insert(pool.to_owned(), brake(2));
        records.brake.insert(other.to_owned(), brake(7));
        let cooldown = |until| Cooldown {
            until: Millis::new(until),
            reason: js_scaler::CooldownReason::BootFailed,
        };
        records.cooldowns.insert(pool.to_owned(), cooldown(until));
        records
            .cooldowns
            .insert(other.to_owned(), cooldown(u64::MAX));
        records
    }

    /// A launch out for `pool`.
    fn out(pool: &str, host: &str) -> InFlight {
        InFlight {
            pool: pool.to_owned(),
            host: host.to_owned(),
            offer: "x".to_owned(),
            zone: None,
            since: Millis::new(30),
        }
    }

    /// Asserts that the provider state of a pool keeps only its own: the declines and
    /// pass-overs of its provider and not of another provider the memory also holds, the
    /// brake and cool-down of its pool and not another pool's, the launches out for its pool
    /// and not another's; that a pass-over is left out at its end; that a cool-down is kept
    /// while it runs and left out at its end; and that a status with no scaler holds none of
    /// them but its catalog. Catches a filter dropped, which would show one provider's or
    /// pool's state on another, and an ended pass-over or cool-down still shown.
    #[test]
    fn a_provider_state_keeps_its_own_provider_and_pool_and_what_still_holds() {
        let own = ProviderId::known("own-cloud");
        let foreign = ProviderId::known("other-cloud");
        let memory = memory(&own, &foreign);
        let records = records("burst", "other", 1000);
        let held = |now: u64| {
            Held {
                catalog: Some((at(1), 4)),
                seen: Some((&memory, &records)),
                out: vec![out("burst", "js-1"), out("other", "js-2")],
            }
            .of((&own, "burst"), Millis::new(now))
        };

        let state = held(999);
        let request = RequestId::new("r1").unwrap();
        let declined_key = js_elastic::OfferKey::of(&own, &offer("own-cloud-x", "z1"));
        assert_eq!(state.declined, [(request, declined_key, at(10))]);
        let passed = PassedOver {
            provider: own.clone(),
            instance_type: "own-cloud-y".to_owned(),
            region: Some("z2".to_owned()),
        };
        let until = at(20).saturating_add(std::time::Duration::from_secs(3600));
        assert_eq!(state.passed_over, [(passed, until)]);
        assert_eq!(state.brake.map(|brake| brake.failures), Some(2));
        assert_eq!(state.cooldown.map(|c| c.until), Some(Millis::new(1000)));
        assert_eq!(state.in_flight, [out("burst", "js-1")]);
        assert_eq!((state.now_ms, state.catalog), (999, Some((at(1), 4))));

        assert_eq!(held(1000).cooldown, None);
        assert!(held(until.millis()).passed_over.is_empty());

        let alone = Held {
            catalog: Some((at(1), 4)),
            seen: None,
            out: Vec::new(),
        }
        .of((&own, "burst"), Millis::new(999));
        let expected = ProviderState {
            now_ms: 999,
            catalog: Some((at(1), 4)),
            ..ProviderState::default()
        };
        assert_eq!(alone, expected);
    }
}
