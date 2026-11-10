//! The faults a scenario scripts into its elastic pool's cloud: steps queued for a call of
//! the fake, a step it repeats for good, its knobs, a foreign machine, machines that never
//! say hello, and the operator's brake reset through the server's route. Each writes one
//! hand-written trace line in the step it is made in and then checks:
//!
//! - `cloud script <call> <step>,<step>...` and `cloud script <call> forever <step>`, a call
//!   named `acquire`, `list`, `release` or `is-gone` (any other `other`: the fake does not
//!   script it), a step `pass`, `refuse-<code>` (`quota`, `capacity`, `shape`, `denied`),
//!   `lost-landed`, `lost-not-landed` or `throttled`;
//! - `cloud knobs quota=<n> capacity=<n> boot=<ms> list-lag=<ms> remove-lag=<ms>
//!   unknown-hold=<ms>`, `-` for no limit;
//! - `cloud foreign named <added>` or `cloud foreign unnamed <added>`;
//! - `cloud never-join <pool> <never> <applied>`;
//! - `cloud brake reset route <status>`, after the client call's own line;
//! - for one named cloud of a server of [`crate::ServerSpec::ordered`], `cloud <provider>
//!   script <call> <step>,<step>...`, `cloud <provider> exhaust` and `cloud <provider>
//!   quota-refuse <offer> <zone>` (`-` for no zone), each followed by `applied` or `none`
//!   (no cloud of that name).
//!
//! None holds a payload, a token, a machine's name or an id the cloud generated. A cell
//! without an elastic pool writes the line and changes nothing.

use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Method, Request};
use js_elastic::RefusalCode;
use js_elastic_fake::{Call, Knobs, Step};

use super::Cell;
use crate::elastic::{Cloud, offer};
use crate::invariant::Touched;
use crate::server::CLIENT_TOKEN;

/// The status the server answers a reset of a pool's brake it accepted.
const RESET_ACCEPTED: u16 = 202;

impl Cell {
    /// Queues `steps` for the cloud's calls of kind `call`, after the steps already queued
    /// (`ElasticFake::script`), and checks.
    pub fn script_provider(&mut self, call: Call, steps: &[Step]) {
        let fake = self.cloud().map(|cloud| cloud.fake().clone());
        fake.into_iter().for_each(|fake| fake.script(call, steps));
        let named: Vec<&str> = steps.iter().map(|step| step_name(*step)).collect();
        self.record(format!(
            "cloud script {} {}",
            call_name(call),
            named.join(",")
        ));
        self.check(Touched::nothing());
    }

    /// Makes every call of kind `call` with no step queued take `step`, for good or until
    /// this is called again (`ElasticFake::script_forever`), and checks.
    pub fn script_provider_forever(&mut self, call: Call, step: Step) {
        let fake = self.cloud().map(|cloud| cloud.fake().clone());
        fake.into_iter()
            .for_each(|fake| fake.script_forever(call, step));
        let (call, step) = (call_name(call), step_name(step));
        self.record(format!("cloud script {call} forever {step}"));
        self.check(Touched::nothing());
    }

    /// Queues `steps` for the calls of kind `call` of the cloud named `provider`
    /// ([`Cell::cloud_named`]), as [`Cell::script_provider`] does for the first, and checks.
    pub fn script_provider_by_name(&mut self, provider: &str, call: Call, steps: &[Step]) {
        let fake = self.cloud_named(provider).map(|cloud| cloud.fake().clone());
        let applied = applied(fake.as_ref());
        fake.into_iter().for_each(|fake| fake.script(call, steps));
        let named: Vec<&str> = steps.iter().map(|step| step_name(*step)).collect();
        let (call, steps) = (call_name(call), named.join(","));
        self.record(format!("cloud {provider} script {call} {steps} {applied}"));
        self.check(Touched::nothing());
    }

    /// Makes every acquire of the cloud named `provider` refuse for `capacity`, whatever
    /// its offer, for good or until its acquires are scripted to repeat another step
    /// (`ElasticFake::script_forever`), and checks.
    pub fn exhaust_provider(&mut self, provider: &str) {
        let fake = self.cloud_named(provider).map(|cloud| cloud.fake().clone());
        let applied = applied(fake.as_ref());
        let capacity = Step::Refuse(RefusalCode::Capacity);
        fake.into_iter()
            .for_each(|fake| fake.script_forever(Call::Acquire, capacity));
        self.record(format!("cloud {provider} exhaust {applied}"));
        self.check(Touched::nothing());
    }

    /// Makes the cloud named `provider` refuse every acquire of offer `offer` in `zone`
    /// for `quota` from now on ([`Cloud::quota_refuse`]): a zone is its own region, as the
    /// server reads them. Checks.
    pub fn quota_refuse(&mut self, provider: &str, offer: &str, zone: Option<&str>) {
        let cloud = self.cloud_named(provider).cloned();
        let applied = applied(cloud.as_ref());
        cloud
            .into_iter()
            .for_each(|cloud| cloud.quota_refuse(offer, zone));
        let zone = zone.unwrap_or("-");
        self.record(format!(
            "cloud {provider} quota-refuse {offer} {zone} {applied}"
        ));
        self.check(Touched::nothing());
    }

    /// Replaces the cloud's limits and delays (`ElasticFake::set_knobs`), and checks. The
    /// boot time of a machine the cell sees from now on stays the pool's
    /// ([`crate::ElasticPool::knobs`]): the cell, not the fake, boots its agent.
    pub fn set_cloud_knobs(&mut self, knobs: &Knobs) {
        let fake = self.cloud().map(|cloud| cloud.fake().clone());
        fake.into_iter()
            .for_each(|fake| fake.set_knobs(knobs.clone()));
        self.record(format!("cloud knobs {}", knobs_text(knobs)));
        self.check(Touched::nothing());
    }

    /// Adds a machine to the cloud's account that the scheduler did not launch, named
    /// `name` or unmarked with no name, on the pool's first offer
    /// (`ElasticFake::add_foreign`), and checks: whether it was added (`false` for a cell
    /// without an elastic pool).
    pub fn add_foreign_machine(&mut self, name: Option<&str>) -> bool {
        let cloud = self.cloud().cloned();
        let first = cloud.as_ref().and_then(|cloud| {
            let spec = cloud.pool().offers().first();
            spec.map(|spec| (cloud, offer(spec, cloud.shape())))
        });
        let added = first.map(|(cloud, offer)| cloud.fake().add_foreign(name, offer));
        let kind = name.map_or("unnamed", |_| "named");
        let added = added.is_some();
        self.record(format!("cloud foreign {kind} {added}"));
        self.check(Touched::nothing());
        added
    }

    /// Makes the machines of pool `pool` that boot from now on never say hello (`never`)
    /// or say it again (`!never`), and checks: whether the cell's elastic pool is `pool`.
    /// A machine that already booted keeps what it did.
    pub fn machines_never_join(&mut self, pool: &str, never: bool) -> bool {
        let cloud = self.cloud().filter(|cloud| cloud.pool().name() == pool);
        let applied = cloud.map(|cloud| cloud.set_joins(!never)).is_some();
        self.record(format!("cloud never-join {pool} {never} {applied}"));
        self.check(Touched::nothing());
        applied
    }

    /// Asks the server to reset the brake of pool `pool` as an operator does, through its
    /// route `POST /v1/pools/{pool}/brake/reset` with the client token, and notes the reset
    /// with the calls of each cloud when the server accepted it (202; the scaler applies it at
    /// its next tick), then checks: the answer's status (0 when no server answered).
    pub fn reset_brake_by_route(&mut self, pool: &str) -> u16 {
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/pools/{pool}/brake/reset"))
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .body(Body::empty())
            // A pool name the route cannot carry makes the empty request, which the server
            // refuses and the status says so.
            .unwrap_or_default();
        let answer = self.client_call(request);
        let status = answer.map_or(0, |answer| answer.status().as_u16());
        if status == RESET_ACCEPTED {
            self.clouds().iter().for_each(Cloud::note_reset);
        }
        self.record(format!("cloud brake reset route {status}"));
        self.check(Touched::nothing());
        status
    }
}

/// How a trace line says whether a change to a named cloud found the cloud.
fn applied<T>(cloud: Option<&T>) -> &'static str {
    match cloud {
        Some(_) => "applied",
        None => "none",
    }
}

/// How a trace line names `call`.
fn call_name(call: Call) -> &'static str {
    match call {
        Call::Acquire => "acquire",
        Call::List => "list",
        Call::Release => "release",
        Call::IsGone => "is-gone",
        _ => "other",
    }
}

/// How a trace line names `step`.
fn step_name(step: Step) -> &'static str {
    match step {
        Step::Pass => "pass",
        Step::Refuse(RefusalCode::Quota) => "refuse-quota",
        Step::Refuse(RefusalCode::Capacity) => "refuse-capacity",
        Step::Refuse(RefusalCode::Shape) => "refuse-shape",
        Step::Refuse(RefusalCode::Denied) => "refuse-denied",
        Step::Lost { landed: true } => "lost-landed",
        Step::Lost { landed: false } => "lost-not-landed",
        Step::Throttled => "throttled",
    }
}

/// How a trace line writes `knobs`.
fn knobs_text(knobs: &Knobs) -> String {
    let limit = |limit: Option<usize>| limit.map_or_else(|| "-".to_owned(), |n| n.to_string());
    format!(
        "quota={} capacity={} boot={}ms list-lag={}ms remove-lag={}ms unknown-hold={}ms",
        limit(knobs.quota),
        limit(knobs.capacity),
        knobs.boot.as_millis(),
        knobs.list_lag.as_millis(),
        knobs.remove_lag.as_millis(),
        knobs.unknown_hold.as_millis(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Catches: two calls the fake scripts apart written alike, and a call it does not
    /// script written as one it does.
    #[test]
    fn each_scripted_call_has_its_own_name() {
        let names = [
            Call::Acquire,
            Call::List,
            Call::Release,
            Call::IsGone,
            Call::Catalog,
        ]
        .map(call_name);
        assert_eq!(names, ["acquire", "list", "release", "is-gone", "other"]);
    }

    /// Catches: two steps written alike (a landed lost acquire as one that did not land, a
    /// refusal of one code as another), which would let two scripts share a trace.
    #[test]
    fn each_step_has_its_own_name() {
        let names = [
            Step::Pass,
            Step::Refuse(RefusalCode::Quota),
            Step::Refuse(RefusalCode::Capacity),
            Step::Refuse(RefusalCode::Shape),
            Step::Refuse(RefusalCode::Denied),
            Step::Lost { landed: true },
            Step::Lost { landed: false },
            Step::Throttled,
        ]
        .map(step_name);
        assert_eq!(
            names,
            [
                "pass",
                "refuse-quota",
                "refuse-capacity",
                "refuse-shape",
                "refuse-denied",
                "lost-landed",
                "lost-not-landed",
                "throttled",
            ]
        );
    }

    /// Catches: a knob left out of the line, a limit written when there is none, and a
    /// delay written in another unit.
    #[test]
    fn the_knobs_line_writes_every_knob() {
        let knobs = Knobs {
            quota: Some(3),
            list_lag: Duration::from_secs(60),
            ..Knobs::default()
        };
        assert_eq!(
            knobs_text(&knobs),
            "quota=3 capacity=- boot=120000ms list-lag=60000ms remove-lag=20000ms \
             unknown-hold=1800000ms"
        );
    }
}
