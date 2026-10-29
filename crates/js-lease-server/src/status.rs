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
use js_scaler::{Phase, Record};
use js_types::ExecMode;
use serde::Serialize;

use crate::adapt::SharedClock;
use crate::checks::{State, Verdict};
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
    launch: &'static str,
    state: State,
    /// Why the pool is unavailable, one line for each check that failed.
    reasons: Vec<String>,
    credentials: Option<CheckView>,
    network_check: Option<CheckView>,
    instances: Instances,
    blocked: Option<&'static str>,
    no_offer: Option<NoOffer>,
    last_launch: Option<LaunchView>,
    machines: Vec<MachineView>,
    orphans: Vec<OrphanView>,
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
    /// `accepted`, or the phase of a launch that is not settled: `planned`, `launching`,
    /// `unresolved` or `failed`.
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
    let outcome = match record.phase {
        Phase::Planned => "planned",
        Phase::Launching => "launching",
        Phase::Unresolved => "unresolved",
        Phase::Failed => "failed",
        _ => "accepted",
    };
    LaunchView {
        at_ms: record.launched_at.as_millis(),
        outcome,
        host: record.name.clone(),
    }
}

/// What the scaler's records say of the pool `pool`: its machines, its orphans and its
/// newest launch.
fn held(
    scaler: Option<&Scaler>,
    pool: &str,
) -> (Vec<MachineView>, Vec<OrphanView>, Option<LaunchView>) {
    let Some(scaler) = scaler else {
        return (Vec::new(), Vec::new(), None);
    };
    let records = scaler.state().records;
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
        let blocked = match state {
            State::Available => None,
            _ => Some("unavailable"),
        };
        let (machines, orphans, last_launch) = held(self.scaler.as_ref(), &pool.name);
        let spend = machines
            .iter()
            .map(|machine| machine.price_cents_per_hour)
            .sum();
        PoolView {
            name: pool.name.clone(),
            kind: pool.kind.clone(),
            network: pool.network_kind,
            exec: exec_name(pool.exec),
            launch: pool.launch,
            state,
            reasons: health.reasons(),
            credentials: check_view(health.credentials),
            network_check: check_view(health.network),
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
    row(out, "credentials", &check_text(&pool.credentials));
    row(out, "network check", &check_text(&pool.network_check));
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
