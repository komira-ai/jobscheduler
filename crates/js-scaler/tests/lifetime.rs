//! How long a machine may live: `max_age` retires a host whether or not it is busy and
//! never cuts its work; `min_host_lifetime` keeps a short `idle` from turning every billed
//! boot into a launch for nothing; and every launch carries the lifetime the cloud itself
//! is to enforce. Each test drives the pure `step` on a time it names; none reads a clock.

mod common;

use std::time::Duration;

use common::*;
use js_elastic::{Acquired, HostStatus, Released};
use js_scaler::{
    AcquirePlan, AcquireResult, Action, Blocked, HostState, Mark, Phase, Records, ReleaseResult,
    Settings, State,
};

/// Live settings with a `max_age` of an hour, a lease ceiling of 30 minutes and the
/// default prepare timeout (10), `unknown_hold` (30) and margin (10): 140 minutes of cloud
/// lifetime.
fn bounded() -> Settings {
    Settings {
        max_age: Some(minutes(60)),
        max_lease_wall: Some(minutes(30)),
        ..settings("gpu")
    }
}

/// A scaler started from the records file that `rig` has written, given the input `rig` was
/// last given.
fn restart(rig: &Rig, settings: Settings) -> Rig {
    let text = rig.state.records.encode().unwrap();
    let mut fresh = Rig::new(settings);
    fresh.input = rig.input.clone();
    fresh.state = State::new(Records::decode(&text).unwrap());
    fresh
}

/// Asserts the whole way down for a host that is busy for good. Until the instant it
/// reaches `max_age` (a millisecond short is not enough) nothing is done to it; at that
/// instant it is retired though it holds a lease; while it stays busy the scaler neither
/// asks for its release nor takes the mark back, however long that is; when its work ends the
/// release is asked; and once the registry shows the host releasing the machine is released.
/// It catches an age that waits for quiet (a host with back-to-back leases would live on),
/// a retire that is undone by the busy check of the drain, and a release of a busy host.
#[test]
fn a_host_that_never_goes_idle_is_retired_at_max_age_and_released_when_its_work_ends() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;

    let early = rig.tick(at(60 * MINUTE - 1));
    assert!(early.is_empty(), "{:?}", kinds(&early));
    assert_eq!(rig.phase(0), Phase::Registered);

    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(kinds(&actions), ["Save", "Retire"]);
    assert_eq!(actions[1], Action::Retire(plan.name.clone()));
    assert_eq!(rig.phase(0), Phase::Draining);

    rig.input.hosts[0].mark = Some(Mark::Draining);
    for step in 1..=40 {
        let actions = rig.tick(at(60 * MINUTE + step * 10 * MINUTE));
        assert!(actions.is_empty(), "{:?}", kinds(&actions));
        assert_eq!(rig.phase(0), Phase::Draining);
    }

    rig.input.hosts[0].busy = false;
    let done = 60 * MINUTE + 41 * 10 * MINUTE;
    let actions = rig.tick(at(done));
    assert_eq!(actions, [Action::ReleaseHost(plan.name.clone())]);

    rig.input.hosts[0].mark = Some(Mark::Releasing);
    let actions = rig.tick(at(done + SECOND));
    assert_eq!(count(&actions, "Release"), 1);
    assert_eq!(rig.phase(0), Phase::Releasing);
}

/// Asserts that a pool with no `max_age` keeps a busy host for as long as it is busy: the
/// rule is off by default and nothing changes for a pool that does not set it.
#[test]
fn without_max_age_a_busy_host_is_never_retired() {
    let (mut rig, _) = registered(settings("gpu"), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    let actions = rig.tick(at(5 * DAY));
    assert!(actions.is_empty(), "{:?}", kinds(&actions));
    assert_eq!(rig.phase(0), Phase::Registered);
}

/// Asserts that a host set aside for work (a whole-node lease, mark `Held`) is retired at
/// `max_age` too: the registry's mark wins over the hold, and the lease keeps running.
#[test]
fn a_host_held_by_a_whole_node_lease_is_retired_at_max_age() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    rig.input.hosts[0].mark = Some(Mark::Held);
    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(actions[1], Action::Retire(plan.name));
}

/// Asserts that a quiet host that reaches `max_age` does not cover a waiting request. At
/// half its age the request is covered by it and nothing launches; at `max_age` it is retired
/// and the request is given a machine. It catches an aged host that still claims work
/// in the step that retires it, which would leave the request waiting for a host that is leaving.
#[test]
fn an_aged_host_covers_no_request() {
    let (mut rig, _) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.waiting = vec![request("r2")];
    let young = rig.tick(at(30 * MINUTE));
    assert!(acquires(&young).is_empty());
    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(count(&actions, "Retire"), 1);
    assert_eq!(acquires(&actions).len(), 1);
}

/// Asserts that a retiring host is not taken back for a request that fits it: the request
/// launches a machine and no undrain is sent. It catches a retire that a waiting request
/// undoes, which would keep the host past its age for as long as requests keep coming.
#[test]
fn a_retiring_host_is_not_taken_back_for_a_request() {
    let (mut rig, _) = registered(bounded(), "i-1", 2 * SECOND);
    rig.tick(at(60 * MINUTE));
    rig.input.hosts[0].mark = Some(Mark::Draining);
    rig.input.waiting = vec![request("r2")];
    let actions = rig.tick(at(61 * MINUTE));
    assert_eq!(count(&actions, "Undrain"), 0);
    assert_eq!(acquires(&actions).len(), 1);
    assert_eq!(rig.phase(0), Phase::Draining);
}

/// Asserts that an aged host the registry already shows draining (the retire was applied
/// but the record still says registered) is retired again and not undone by the step that
/// heals a lost undrain. It catches the two rules fighting: an undrain and a retire for one host.
#[test]
fn an_aged_registered_host_shown_draining_is_retired_not_undrained() {
    let (mut rig, _) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].mark = Some(Mark::Draining);
    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(count(&actions, "Undrain"), 0);
    assert_eq!(count(&actions, "Retire"), 1);
}

/// Asserts that a host the registry does not hear from is not retired: the silence rules
/// own it, and a retire for a host that cannot answer would only be a call that fails.
#[test]
fn an_aged_host_the_registry_does_not_hear_from_is_left_to_the_silence_rules() {
    let (mut rig, _) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].state = HostState::Lost;
    let actions = rig.tick(at(60 * MINUTE));
    assert_eq!(count(&actions, "Retire"), 0);
    assert_eq!(rig.phase(0), Phase::Registered);
}

/// Asserts, across a restart, that the age is counted from the stored launch time: a
/// scaler restarted at half the age retires at the original instant (a millisecond short
/// is not enough), not an age after the restart.
#[test]
fn max_age_runs_on_across_a_restart() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    rig.tick(at(30 * MINUTE));

    let mut again = restart(&rig, bounded());
    assert_eq!(count(&again.tick(at(31 * MINUTE)), "Retire"), 0);
    assert_eq!(count(&again.tick(at(60 * MINUTE - 1)), "Retire"), 0);
    let actions = again.tick(at(60 * MINUTE));
    assert!(actions.contains(&Action::Retire(plan.name)));
}

/// Asserts that the time the scaler was down counts toward the age: the cloud bills through
/// it, so a scaler back after five hours retires the host at its first step instead of
/// giving it another hour.
#[test]
fn the_downtime_counts_toward_max_age() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    rig.tick(at(30 * MINUTE));

    let mut again = restart(&rig, bounded());
    let actions = again.tick(at(5 * HOUR));
    assert!(actions.contains(&Action::Retire(plan.name)));
}

/// Asserts that a retire lost between the step and the registry call is sent again. The
/// records were written with the host draining, the registry kept no mark (the process died
/// before the call), and the first step after the restart reads the mismatch and retires
/// the host again. It catches a retire that is sent once and remembered as done.
#[test]
fn a_retire_lost_in_a_crash_is_sent_again() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    rig.input.hosts[0].busy = true;
    let first = rig.tick(at(60 * MINUTE));
    assert_eq!(count(&first, "Retire"), 1);

    let mut again = restart(&rig, bounded());
    assert_eq!(again.phase(0), Phase::Draining);
    assert_eq!(again.input.hosts[0].mark, None);
    let actions = again.tick(at(60 * MINUTE + 5 * SECOND));
    assert!(actions.contains(&Action::Retire(plan.name)));
    assert_eq!(again.phase(0), Phase::Draining);
}

/// Asserts the lifetime a launch carries: `max_age` plus the ceiling on a lease's wall time
/// plus the margin; none when the pool has no `max_age` or the server no ceiling, since the
/// cloud's bound cannot then be worked out. It catches a lifetime that omits a term, and one
/// that is made up when a term is missing.
#[test]
fn a_launch_carries_the_lifetime_the_cloud_is_to_enforce() {
    assert_eq!(bounded().lifetime(), Some(minutes(60 + 30 + 10 + 30 + 10)));
    let no_age = Settings {
        max_age: None,
        ..bounded()
    };
    assert_eq!(no_age.lifetime(), None);
    let no_ceiling = Settings {
        max_lease_wall: None,
        ..bounded()
    };
    assert_eq!(no_ceiling.lifetime(), None);

    let mut rig = Rig::new(bounded());
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    assert_eq!(plan.lifetime, Some(minutes(140)));

    let mut plain = Rig::new(settings("gpu"));
    plain.input.waiting = vec![request("r1")];
    assert_eq!(acquires(&plain.tick(at(0)))[0].lifetime, None);
}

/// Asserts the lifetime term by term, to the second: each of `max_age`, the ceiling, the
/// prepare timeout, `unknown_hold` and the margin adds exactly its own length. It catches a
/// lifetime that leaves out the wait before a whole-node lease's wall time starts or the wait
/// before a silent host is released, which would let the cloud stop a host that is still
/// legitimately draining.
#[test]
fn every_wait_of_a_retire_is_in_the_lifetime() {
    let base = bounded().lifetime().unwrap();
    let more = Duration::from_secs(1);
    let grown = [
        Settings {
            max_age: Some(minutes(60) + more),
            ..bounded()
        },
        Settings {
            max_lease_wall: Some(minutes(30) + more),
            ..bounded()
        },
        Settings {
            prepare_timeout: minutes(10) + more,
            ..bounded()
        },
        Settings {
            unknown_hold: minutes(30) + more,
            ..bounded()
        },
        Settings {
            lifetime_margin: minutes(10) + more,
            ..bounded()
        },
    ];
    for settings in grown {
        assert_eq!(settings.lifetime(), Some(base + more));
    }
}

/// Asserts that a launch the provider told to wait carries the lifetime when it is retried
/// after a restart: the lifetime is worked out from the settings at each call, and nothing
/// of it need be stored. It catches a retry path that builds its call without it.
#[test]
fn a_retried_launch_carries_the_lifetime_after_a_restart() {
    let mut rig = Rig::new(bounded());
    rig.input.waiting = vec![request("r1")];
    let plan = acquires(&rig.tick(at(0))).remove(0);
    rig.input.acquired = vec![answered(
        &plan,
        Acquired::Deferred {
            wait: Duration::from_secs(600),
        },
    )];
    rig.tick(at(SECOND));
    assert_eq!(rig.phase(0), Phase::Planned);

    let mut again = restart(&rig, bounded());
    let retry = acquires(&again.tick(at(601 * SECOND))).remove(0);
    assert_eq!(retry.acquire, plan.acquire);
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.lifetime, Some(minutes(140)));
}

/// Asserts that `idle` drains a host only once it has lived `min_host_lifetime`, to the
/// millisecond, and that the quiet time keeps counting meanwhile: the host is drained at
/// the first step at the age, not an `idle` later. It catches a minimum that is not applied,
/// one that restarts the quiet count, and an off-by-one at the edge.
#[test]
fn a_short_idle_waits_for_the_minimum_lifetime() {
    let short = Settings {
        idle: minutes(1),
        min_host_lifetime: minutes(10),
        ..settings("gpu")
    };
    let (mut rig, plan) = registered(short, "i-1", 2 * SECOND);
    assert!(rig.tick(at(2 * MINUTE)).is_empty());
    assert!(rig.tick(at(10 * MINUTE - 1)).is_empty());
    let actions = rig.tick(at(10 * MINUTE));
    assert_eq!(actions[1], Action::Drain(plan.name));
    assert_eq!(rig.phase(0), Phase::Draining);
}

/// What the cloud bills for one machine on its own books: from its launch to the earliest
/// of now, the machine's termination and the end of the lifetime it was launched with, per
/// started minute at the hourly price rounded up. It reads nothing the scaler computes.
struct Cloud {
    launched: u64,
    lifetime: Option<Duration>,
    terminated: Option<u64>,
    price_per_hour: u64,
}

impl Cloud {
    fn launch(plan: &AcquirePlan, at: u64) -> Self {
        Self {
            launched: at,
            lifetime: plan.lifetime,
            terminated: None,
            price_per_hour: plan.offer.price_cents_per_hour.unwrap(),
        }
    }

    fn bill(&self, until: u64) -> u64 {
        let ends = self
            .lifetime
            .map(|life| self.launched + u64::try_from(life.as_millis()).unwrap());
        let end = [Some(until), self.terminated, ends]
            .into_iter()
            .flatten()
            .min()
            .unwrap();
        let minutes = (end - self.launched).div_ceil(MINUTE);
        (minutes * self.price_per_hour).div_ceil(60)
    }
}

/// Launches one machine, registers its host busy for good, lets the scaler take one step
/// at ten minutes and then dies for `dead_hours` hours. Returns the cloud's bill at the end
/// and the lifetime it enforced.
fn dead_server_bill(settings: Settings, dead_hours: u64) -> (u64, Option<Duration>) {
    let (mut rig, plan) = registered(settings, "i-1", 2 * SECOND);
    let cloud = Cloud::launch(&plan, 0);
    rig.input.hosts[0].busy = true;
    rig.tick(at(10 * MINUTE));
    (cloud.bill(10 * MINUTE + dead_hours * HOUR), cloud.lifetime)
}

/// Asserts that a fake cloud which enforces the lifetime it was launched with bills a host at
/// most lifetime times price when the server dies at ten minutes and stays dead for 30
/// hours: 140 minutes at 60 cents an hour is 140 cents, not 1800. The control is the same
/// pool with no `max_age`, which has no lifetime and is billed for all 30 hours, so the
/// oracle can fail.
#[test]
fn a_cloud_that_enforces_the_lifetime_bills_a_dead_scaler_at_most_the_lifetime() {
    let (bill, lifetime) = dead_server_bill(bounded(), 30);
    assert_eq!(lifetime, Some(minutes(140)));
    assert_eq!(bill, 140);

    let (bill, lifetime) = dead_server_bill(settings("gpu"), 30);
    assert_eq!(lifetime, None);
    assert!(bill > 1000, "{bill}");
}

/// Asserts, with the scaler alive, that a host busy until the end of the last lease the
/// ceiling allows (`max_age` plus the 30 minute ceiling) is released at that instant and not
/// before, and that the cloud's bill is within the lifetime it was launched with. It proves
/// the retire cuts no work and that the lifetime has room for a drain: release at 90
/// minutes against a lifetime of 140.
#[test]
fn a_retired_host_is_released_when_its_last_lease_ends_within_the_lifetime() {
    let (mut rig, plan) = registered(bounded(), "i-1", 2 * SECOND);
    let mut cloud = Cloud::launch(&plan, 0);
    let work_ends = 90 * MINUTE;
    for minute in 3..=150 {
        let now = minute * MINUTE;
        rig.input.hosts[0].busy = now < work_ends;
        for action in rig.tick(at(now)) {
            match action {
                Action::Retire(_) => rig.input.hosts[0].mark = Some(Mark::Draining),
                Action::ReleaseHost(_) => rig.input.hosts[0].mark = Some(Mark::Releasing),
                Action::Release { .. } if cloud.terminated.is_none() => {
                    cloud.terminated = Some(now);
                }
                _ => {}
            }
        }
    }
    let ended = cloud.terminated.expect("the host was released");
    assert!(
        ended >= work_ends,
        "released at {ended} before the work ended"
    );
    assert!(
        ended <= 140 * MINUTE,
        "released at {ended}, after the lifetime"
    );
    assert!(cloud.bill(150 * MINUTE) <= 140);
}

/// One machine of the churn world.
struct Machine {
    plan: AcquirePlan,
    cloud: String,
    launched: u64,
    up_at: u64,
    gone_at: Option<u64>,
    busy_until: u64,
    mark: Option<Mark>,
    removed: bool,
}

/// A world with one kind of work: a one-minute job every `GAP`, which waits for a host when
/// none is free; a machine takes a minute to boot; the registry marks hosts as the real one
/// does (a drain only of a quiet host, a retire of any). The world keeps its own count of
/// launches and its own bill.
struct World {
    rig: Rig,
    machines: Vec<Machine>,
    answers: Vec<AcquireResult>,
    releases: Vec<ReleaseResult>,
    next_job: u64,
    waiting: bool,
    launches: usize,
    saw_day_cap: bool,
}

/// The time between the start of one job and the arrival of the next: a minute of work, then
/// 180 seconds of quiet, which is two minutes of `idle` and a minute more.
const GAP: u64 = 240 * SECOND;
const JOB: u64 = MINUTE;
const BOOT: u64 = MINUTE;

impl World {
    fn new(settings: Settings) -> Self {
        Self {
            rig: Rig::new(settings),
            machines: Vec::new(),
            answers: Vec::new(),
            releases: Vec::new(),
            next_job: 0,
            waiting: false,
            launches: 0,
            saw_day_cap: false,
        }
    }

    fn step(&mut self, now: u64) {
        if now >= self.next_job {
            self.waiting = true;
        }
        if self.waiting {
            let free = self.machines.iter_mut().find(|m| {
                [
                    m.gone_at.is_none(),
                    !m.removed,
                    now >= m.up_at,
                    m.mark.is_none(),
                    m.busy_until <= now,
                ]
                .into_iter()
                .all(|ok| ok)
            });
            if let Some(machine) = free {
                machine.busy_until = now + JOB;
                self.waiting = false;
                self.next_job = now + GAP;
            }
        }
        self.rig.input.waiting = match self.waiting {
            true => vec![request("job")],
            false => Vec::new(),
        };
        self.rig.input.hosts = (self.machines.iter())
            .filter(|m| {
                [m.gone_at.is_none(), !m.removed, now >= m.up_at]
                    .into_iter()
                    .all(|ok| ok)
            })
            .map(|m| {
                let mut facts = host(&m.plan.name, "gpu");
                facts.mark = m.mark;
                facts.busy = m.busy_until > now;
                facts
            })
            .collect();
        self.rig.pool().held = Some(
            (self.machines.iter())
                .filter(|m| m.gone_at.is_none())
                .map(|m| listed(&m.plan, &m.cloud, HostStatus::Active))
                .collect(),
        );
        self.rig.input.acquired = std::mem::take(&mut self.answers);
        self.rig.input.released = std::mem::take(&mut self.releases);
        let actions = self.rig.tick(at(now));
        self.saw_day_cap |= self.rig.state.blocked.get("gpu") == Some(&Blocked::MaxSpendDay);
        for action in actions {
            self.apply(action, now);
        }
    }

    fn named(&mut self, name: &str) -> Option<&mut Machine> {
        self.machines.iter_mut().find(|m| m.plan.name == name)
    }

    fn apply(&mut self, action: Action, now: u64) {
        match action {
            Action::Acquire(plan) => {
                self.launches += 1;
                let cloud = format!("i-{}", self.launches);
                self.answers.push(accepted(&plan, &cloud));
                self.machines.push(Machine {
                    plan: *plan,
                    cloud,
                    launched: now,
                    up_at: now + BOOT,
                    gone_at: None,
                    busy_until: 0,
                    mark: None,
                    removed: false,
                });
            }
            Action::Drain(name) => {
                if let Some(m) = self.named(&name).filter(|m| m.busy_until <= now) {
                    m.mark = Some(Mark::Draining);
                }
            }
            Action::Retire(name) => {
                if let Some(m) = self.named(&name) {
                    m.mark = Some(Mark::Draining);
                }
            }
            Action::Undrain(name) => {
                if let Some(m) = self.named(&name).filter(|m| m.mark == Some(Mark::Draining)) {
                    m.mark = None;
                }
            }
            Action::ReleaseHost(name) => {
                let ready =
                    |m: &&mut Machine| m.busy_until <= now && m.mark == Some(Mark::Draining);
                if let Some(m) = self.named(&name).filter(ready) {
                    m.mark = Some(Mark::Releasing);
                }
            }
            Action::Release { host, .. } => {
                if let Some(m) = self.named(&host).filter(|m| m.gone_at.is_none()) {
                    m.gone_at = Some(now);
                    self.releases.push(ReleaseResult {
                        host,
                        answer: Released::Gone,
                    });
                }
            }
            Action::RemoveHost(name) => {
                if let Some(m) = self.named(&name) {
                    m.removed = true;
                }
            }
            _ => {}
        }
    }

    /// What the cloud billed in all by `end`: each machine from its launch to its
    /// termination or `end`, per started minute at 60 cents an hour (a cent a minute).
    fn spend(&self, end: u64) -> u64 {
        (self.machines.iter())
            .map(|m| (m.gone_at.unwrap_or(end) - m.launched).div_ceil(MINUTE))
            .sum()
    }
}

/// Runs the churn world for six hours in ten-second steps.
fn churn(settings: Settings) -> World {
    let mut world = World::new(settings);
    for tick in 0..(6 * HOUR / (10 * SECOND)) {
        world.step(tick * 10 * SECOND);
    }
    world
}

fn churn_settings(minimum: Duration) -> Settings {
    Settings {
        idle: minutes(2),
        min_host_lifetime: minimum,
        ..settings("gpu")
    }
}

/// Asserts that work arriving a minute after `idle` has run out, every time, makes a
/// pool with no minimum launch a machine for almost every job (the baseline: at least 60 in
/// six hours), and that a minimum lifetime of 30 minutes brings it down to what it allows:
/// no more than one machine per 30 minutes of the six hours and one more. It is
/// the churn rule, measured on the world's own count of launches, not the scaler's.
#[test]
fn work_arriving_just_after_idle_is_bounded_by_the_minimum_lifetime() {
    let thrash = churn(churn_settings(Duration::ZERO));
    assert!(thrash.launches >= 60, "{}", thrash.launches);

    let held = churn(churn_settings(minutes(30)));
    assert!(held.launches >= 1);
    assert!(held.launches <= 6 * 2 + 1, "{}", held.launches);
}

/// Asserts that the caps bound the same pattern when the minimum lifetime is off: with a day
/// cap of 300 cents the pool reaches the cap (the status says `MaxSpendDay`), launches
/// fewer machines than without it, and the cloud's own bill for the six hours is within the
/// cap plus ten minutes of each of the four machines the pool may hold, which is the room the
/// cap's release latency leaves.
#[test]
fn work_arriving_just_after_idle_is_bounded_by_the_day_cap() {
    let free = churn(churn_settings(Duration::ZERO));
    let capped = churn(Settings {
        max_spend_per_day: 300,
        ..churn_settings(Duration::ZERO)
    });
    assert!(capped.saw_day_cap);
    assert!(capped.launches < free.launches);
    let slack = 4 * 10;
    assert!(
        capped.spend(6 * HOUR) <= 300 + slack,
        "{}",
        capped.spend(6 * HOUR)
    );
}
