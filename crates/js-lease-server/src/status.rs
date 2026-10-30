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
//! The machines, the figures of what they cost, the orphans and the last launch are the
//! scaler's records for the pool (what its last tick left), so they are empty for a pool
//! that has launched nothing, and in `plan` mode. A record holds no secret: it names a
//! machine, its provider id, its offer and its price. `idle_since_ms` of the design is not
//! shown: the scaler does not give out since when a host has been quiet.

use std::fmt::Write;
use std::sync::PoisonError;

use js_elastic::Demand;
use js_lease::{Clock, Timestamp};
use js_registry::Registry;
use js_scaler::{LaunchMode, Millis, Phase, Record, Records};
use js_types::ExecMode;
use serde::Serialize;

use crate::adapt::SharedClock;
use crate::checks::{Checked, State, Verdict};
use crate::hosts::MemoryView;
use crate::pool::{Next, NextLaunch, NoOffer, Pool, Wanted};
use crate::scaler::Scaler;

/// What the status routes read.
pub(crate) struct Status {
    pool: Option<Pool>,
    registry: Option<Registry>,
    scaler: Option<Scaler>,
    warnings: Vec<String>,
    clock: SharedClock,
}

/// The reply of `GET /v1/pools`.
#[derive(Debug, Serialize)]
pub(crate) struct PoolsView {
    pools: Vec<PoolView>,
    /// `server: warning: ...` lines, such as a listener on every interface.
    warnings: Vec<String>,
}

/// One pool.
#[derive(Debug, Serialize)]
struct PoolView {
    name: String,
    kind: String,
    network: &'static str,
    /// The execution mode the hosts must report, or `none` for a pool that asked for none.
    exec: String,
    /// The mode the pool is in now: `plan` once its `live_until` has passed, whatever the
    /// file says.
    launch: &'static str,
    /// When the pool's go ends, in milliseconds since the epoch, if the file sets one.
    live_until_ms: Option<u64>,
    /// The memory of the pool's floor, the percent of it the pool books and what that
    /// comes to.
    memory: MemoryView,
    state: State,
    /// Why the pool is unavailable, one line for each check that failed.
    reasons: Vec<String>,
    credentials: Option<CheckView>,
    network_check: Option<CheckView>,
    /// The provider's own checks of the last pass.
    provider_checks: Vec<NamedCheckView>,
    instances: Instances,
    blocked: Option<&'static str>,
    no_offer: Option<NoOffer>,
    last_launch: Option<LaunchView>,
    machines: Vec<MachineView>,
    orphans: Vec<OrphanView>,
    /// What an operator must act on, as the scaler's last tick left it: a stuck revoke or
    /// removal, a release the provider does not confirm, an orphan, a silent host that
    /// nothing releases.
    alerts: Vec<String>,
    next_launch: Option<NextLaunch>,
}

/// One check.
#[derive(Debug, Serialize)]
struct CheckView {
    ok: bool,
    reason: Option<String>,
    checked_at_ms: u64,
}

/// The newest launch of the pool.
#[derive(Debug, Serialize)]
struct LaunchView {
    at_ms: u64,
    /// `accepted` once the provider has given an id for the machine, `pending` before.
    outcome: &'static str,
    host: String,
}

/// A machine the scheduler launched and still holds.
#[derive(Debug, Serialize)]
struct MachineView {
    host: String,
    state: Phase,
    offer: String,
    zone: Option<String>,
    price_cents_per_hour: u64,
}

/// A machine of the scheduler's that no record knew: counted, never released by the
/// scaler.
#[derive(Debug, Serialize)]
struct OrphanView {
    host: String,
    provider_id: Option<String>,
}

/// One of the provider's own checks.
#[derive(Debug, Serialize)]
struct NamedCheckView {
    name: &'static str,
    ok: bool,
    reason: Option<String>,
    /// What a check that held said, such as what a launch will make.
    note: Option<String>,
    checked_at_ms: u64,
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
struct Instances {
    n: usize,
    max: usize,
    spend_cents_per_hour: u64,
    max_spend_cents_per_hour: u64,
}

/// What the next launch is for: the first request on the wait list, else a request for
/// the pool floor.
pub(crate) fn wanted(registry: Option<&Registry>, floor: &Demand) -> Wanted {
    let Some(registry) = registry else {
        return floor_wanted(floor);
    };
    match registry.waiting().into_iter().next() {
        Some(waiting) => Wanted {
            request: waiting.id.to_string(),
            demand: waiting.demand,
        },
        None => floor_wanted(floor),
    }
}

fn floor_wanted(floor: &Demand) -> Wanted {
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
        registry: Option<Registry>,
        scaler: Option<Scaler>,
        warnings: Vec<String>,
        clock: SharedClock,
    ) -> Self {
        Self {
            pool,
            registry,
            scaler,
            warnings,
            clock,
        }
    }

    /// The status now.
    pub(crate) fn view(&self) -> PoolsView {
        let pools = self.pool.iter().map(|pool| self.pool_view(pool)).collect();
        PoolsView {
            pools,
            warnings: self.warnings.clone(),
        }
    }

    fn pool_view(&self, pool: &Pool) -> PoolView {
        let health = pool
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let state = health.state();
        let wanted = wanted(self.registry.as_ref(), &pool.floor_demand);
        let (next_launch, no_offer) = match state {
            State::Checking => (None, None),
            _ => match pool.next_launch(&wanted, &health.offers) {
                Next::Planned(next) => (Some(next), None),
                Next::NoOffer(none) => (None, Some(none)),
            },
        };
        let now = Millis::new(self.clock.now().millis());
        let settings = self.scaler.as_ref().map(Scaler::settings);
        let launch = settings.map_or(pool.launch, |set| match set.mode(now) {
            LaunchMode::Live => "live",
            LaunchMode::Plan => "plan",
        });
        let live_until_ms = settings
            .and_then(|set| set.live_until)
            .map(Millis::as_millis);
        let scaled = self.scaler.as_ref().map(|scaler| scaler.state());
        // The checks first; once they pass, the cap that keeps the scaler from launching.
        let capped = scaled
            .as_ref()
            .and_then(|seen| seen.blocked.get(&pool.name));
        let blocked = match state {
            State::Available => capped.and_then(|why| why.cap_name()),
            _ => Some("unavailable"),
        };
        let records = scaled.map(|seen| seen.records);
        let (machines, orphans, last_launch) = held(&records.unwrap_or_default(), &pool.name);
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
            alerts: self.scaler.as_ref().map(Scaler::alerts).unwrap_or_default(),
            next_launch,
        }
    }

    /// The time the status is read at, for the page.
    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
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
    out.push_str("</body></html>\n");
    out
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
}
