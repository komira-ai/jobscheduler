//! A month of a pool that always has work, against a monthly total, on a manual clock and
//! with a cloud that bills by its own rules. The cloud here is a small driver of the step's
//! actions (accept, say hello, take the retire, release) and an oracle that bills every
//! machine for the minutes it ran, rounded up, without reading the scaler's records.
//!
//! The pool's caps alone allow 600 cents a day, so 31 days of them are 18600; the total is
//! 5000 over any 30 days. Each test says what it proves.

mod common;

use common::*;
use js_elastic::{HostStatus, Released};
use js_scaler::{
    AcquirePlan, AcquireResult, Action, Blocked, Budgets, Mark, Records, ReleaseResult, Settings,
    State, Totals, overrun,
};

/// The most the monthly total allows, in cents.
const MONTH_TOTAL: u64 = 5000;

/// The step of the clock: five minutes.
const TICK: u64 = 5 * MINUTE;

/// The pool: two machines of 60 cents an hour at most (120 an hour), 600 cents a day, and
/// a machine retired two hours after its launch. The server's ceiling on a lease is 30
/// minutes, so a launch carries a cloud lifetime of 2 h + 30 + 10 + 30 + 10 minutes.
fn pool_settings() -> Settings {
    Settings {
        max_instances: 2,
        max_spend_per_hour: 120,
        max_spend_per_day: 600,
        max_age: Some(minutes(120)),
        max_lease_wall: Some(minutes(30)),
        ..settings("gpu")
    }
}

/// One machine of the driver's cloud.
struct Machine {
    plan: AcquirePlan,
    cloud: String,
    start: u64,
    ended: Option<u64>,
    mark: Option<Mark>,
}

impl Machine {
    /// Whether its agent has said hello: two minutes after the launch.
    fn up(&self, now: u64) -> bool {
        self.ended.is_none() && now >= self.start + 2 * MINUTE
    }
}

/// What a run saw.
struct Outcome {
    /// The most the oracle billed inside any 30 days, in cent-minutes (60 of them a cent).
    peak: u64,
    /// When each launch was made, in milliseconds from the start.
    launches: Vec<u64>,
    /// What blocked the pool on day 15.
    blocked_on_day_15: Option<Blocked>,
}

/// What the oracle bills for the 30 days up to `now`: each machine for the minutes it ran
/// inside the window, rounded up, at 60 cents an hour, in cent-minutes.
fn billed(machines: &[Machine], now: u64) -> u64 {
    let from = now.saturating_sub(30 * DAY);
    machines
        .iter()
        .map(|machine| {
            let begin = machine.start.max(from);
            let end = machine.ended.unwrap_or(now).min(now);
            end.saturating_sub(begin).div_ceil(MINUTE) * 60
        })
        .sum()
}

/// The host the registry would show for `machine`.
fn facts(machine: &Machine) -> js_scaler::HostFacts {
    let mut each = host(&machine.plan.name, "gpu");
    each.mark = machine.mark;
    each.busy = machine.mark.is_none();
    each
}

/// Runs 31 days. Work never stops: two requests wait at every step, and a host is busy
/// until the scaler retires it. The scaler is restarted from its records file at each
/// instant of `restarts`, given in milliseconds from the start.
fn run(budgets: Budgets, restarts: &[u64]) -> Outcome {
    let mut rig = Rig::new(pool_settings());
    rig.input.budgets = budgets;
    rig.input.waiting = vec![request("r1"), request("r2")];
    let mut machines: Vec<Machine> = Vec::new();
    let mut answers: Vec<AcquireResult> = Vec::new();
    let mut released: Vec<ReleaseResult> = Vec::new();
    let mut out = Outcome {
        peak: 0,
        launches: Vec::new(),
        blocked_on_day_15: None,
    };
    for n in 0..=(31 * DAY / TICK) {
        let now = n * TICK;
        if restarts.contains(&now) {
            let text = rig.state.records.encode().unwrap();
            rig.state = State::new(Records::decode(&text).unwrap());
        }
        rig.input.acquired = std::mem::take(&mut answers);
        rig.input.released = std::mem::take(&mut released);
        rig.input.hosts = machines
            .iter()
            .filter(|machine| machine.up(now))
            .map(facts)
            .collect();
        let listing = machines.iter().filter(|machine| machine.ended.is_none());
        let held = listing.map(|machine| {
            let status = match machine.up(now) {
                true => HostStatus::Active,
                false => HostStatus::Booting,
            };
            listed(&machine.plan, &machine.cloud, status)
        });
        rig.pool().held = Some(held.collect());

        for action in rig.tick(at(now)) {
            match action {
                Action::Acquire(plan) => {
                    let cloud = format!("i-{}", machines.len());
                    answers.push(accepted(&plan, &cloud));
                    out.launches.push(now);
                    machines.push(Machine {
                        plan: *plan,
                        cloud,
                        start: now,
                        ended: None,
                        mark: None,
                    });
                }
                Action::Retire(name) | Action::Drain(name) => {
                    mark(&mut machines, &name, Mark::Draining)
                }
                Action::ReleaseHost(name) => mark(&mut machines, &name, Mark::Releasing),
                Action::Release { host, .. } => {
                    let found = machines
                        .iter_mut()
                        .find(|machine| machine.plan.name == host);
                    if let Some(machine) = found.filter(|machine| machine.ended.is_none()) {
                        machine.ended = Some(now);
                        released.push(ReleaseResult {
                            host,
                            answer: Released::Gone,
                        });
                    }
                }
                _ => {}
            }
        }
        out.peak = out.peak.max(billed(&machines, now));
        if now == 15 * DAY {
            out.blocked_on_day_15 = rig.state.blocked.get("gpu").copied();
        }
    }
    out
}

/// Sets the registry's mark on the host `name`: a host being drained takes no more work, so
/// its job ends with it.
fn mark(machines: &mut [Machine], name: &str, to: Mark) {
    if let Some(machine) = machines
        .iter_mut()
        .find(|machine| machine.plan.name == name)
    {
        machine.mark = Some(to);
    }
}

/// The server-wide total of the run: `MONTH_TOTAL` over any 30 days.
fn capped() -> Budgets {
    Budgets {
        server: Totals {
            per_month: Some(MONTH_TOTAL),
            ..Totals::default()
        },
        ..Budgets::default()
    }
}

/// The documented bound on how far a window can pass its total: what the held machines can
/// still cost after the last launch the total allowed.
fn bound(machines: usize) -> u64 {
    let limits = pool_settings();
    let machines = u64::try_from(machines).unwrap();
    overrun(
        limits.max_spend_per_hour,
        limits.lifetime().unwrap(),
        machines,
    )
}

/// Asserts the whole month. Without the total the pool's caps alone let the oracle bill far
/// more than the total and its bound inside 30 days (the control, which shows the run is
/// able to fail). With the total of 5000 cents the oracle never bills more than the total
/// plus the overrun bound (`overrun`: the hourly cap times the cloud lifetime of a launch,
/// 120 x 4 h = 480 cents), no launch is made from day 12 to day 29 although work never stops
/// and the day cap has room, and the pool says `MaxSpendTotal` on day 15. It catches a
/// total that is not enforced, one that counts only a day, and one that forgets the spend of
/// earlier days.
#[test]
fn a_month_of_work_never_bills_more_than_the_total_plus_the_overrun_bound() {
    let free = run(Budgets::default(), &[]);
    let control = (MONTH_TOTAL + bound(0)) * 60;
    assert!(
        free.peak > control,
        "without a total the run bills {}",
        free.peak / 60
    );

    let held = run(capped(), &[]);
    let limit = (MONTH_TOTAL + bound(held.launches.len())) * 60;
    assert!(
        held.peak <= limit,
        "billed {} of {}",
        held.peak / 60,
        limit / 60
    );
    assert!(
        held.peak > limit / 2,
        "the total was never near: {}",
        held.peak / 60
    );
    let quiet = held
        .launches
        .iter()
        .filter(|when| (12 * DAY..29 * DAY).contains(*when));
    assert_eq!(quiet.count(), 0);
    assert!(held.launches.iter().any(|when| *when < 5 * DAY));
    assert_eq!(held.blocked_on_day_15, Some(Blocked::MaxSpendTotal));
}

/// Asserts the same month with the scaler restarted from its records file in the middle:
/// on day 5 (the spend so far is in the file), on day 12 (the total is spent: a scaler
/// that forgot the month would launch again) and on day 20. The bound holds, nothing is
/// launched from day 12 to day 29, and the pool still says `MaxSpendTotal` on day 15. It
/// catches a month total that lives only in memory.
#[test]
fn the_monthly_total_holds_across_restarts() {
    let restarts = [5 * DAY, 12 * DAY, 20 * DAY];
    let held = run(capped(), &restarts);
    let limit = (MONTH_TOTAL + bound(held.launches.len())) * 60;
    assert!(
        held.peak <= limit,
        "billed {} of {}",
        held.peak / 60,
        limit / 60
    );
    let quiet = held
        .launches
        .iter()
        .filter(|when| (12 * DAY..29 * DAY).contains(*when));
    assert_eq!(quiet.count(), 0);
    assert_eq!(held.blocked_on_day_15, Some(Blocked::MaxSpendTotal));
}
