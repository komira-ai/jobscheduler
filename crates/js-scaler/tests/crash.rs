//! A crash between any two steps of the scaler's shell, or between any two of the calls of
//! one step. The shell is played here by a driver that runs a step's actions in order on a
//! small world (a provider that keeps its machines, a registry that keeps its hosts and
//! their marks, a records file that holds what the last `Save` wrote), and a crash drops
//! everything but the world and the file: the shell's memory of the provider's answers,
//! and the state of the step. The restarted scaler starts from the file, with answers
//! nobody remembers. Nothing reads a clock; a tick is a minute of the injected time.

mod common;

use std::collections::BTreeMap;

use common::*;
use js_elastic::{Held, HostRef, HostStatus, Released};
use js_scaler::{
    AcquireResult, Action, HostFacts, Mark, Phase, Records, ReleaseResult, Settings, State,
};

/// How many one-minute ticks a run lasts: room for the 30 minutes an unanswered acquire is
/// held, the two clean lists that settle it, a boot, the idle time and the release.
const TICKS: u64 = 160;

/// One machine of the world's provider.
struct Machine {
    plan: js_scaler::AcquirePlan,
    cloud: String,
    status: HostStatus,
    born: u64,
}

/// One host of the world's registry.
struct Registered {
    name: String,
    mark: Option<Mark>,
    busy_until: u64,
}

/// What survives a crash: the provider, the registry, the file, and the log of calls.
#[derive(Default)]
struct World {
    machines: Vec<Machine>,
    hosts: Vec<Registered>,
    saved: Option<String>,
    log: Vec<String>,
    created: usize,
    waiting: bool,
}

/// What a crash loses: the provider's answers the shell had not yet given the step.
#[derive(Default)]
struct Process {
    acquired: Vec<AcquireResult>,
    released: Vec<ReleaseResult>,
}

impl World {
    fn new() -> Self {
        Self {
            waiting: true,
            ..Self::default()
        }
    }

    fn list(&self) -> Vec<Held> {
        self.machines
            .iter()
            .map(|machine| listed(&machine.plan, &machine.cloud, machine.status.clone()))
            .collect()
    }

    fn facts(&self, tick: u64) -> Vec<HostFacts> {
        self.hosts
            .iter()
            .map(|each| HostFacts {
                mark: each.mark,
                busy: tick < each.busy_until,
                ..host(&each.name, "gpu")
            })
            .collect()
    }

    /// The agents of the machines that have booted say hello: three ticks after the
    /// machine was made, and the first request is served and busy for two ticks.
    fn hellos(&mut self, tick: u64) {
        for machine in &self.machines {
            let booted = tick >= machine.born + 3;
            let known = self.hosts.iter().any(|each| each.name == machine.plan.name);
            if booted && !known && machine.status == HostStatus::Booting {
                self.hosts.push(Registered {
                    name: machine.plan.name.clone(),
                    mark: None,
                    busy_until: tick + 2,
                });
                self.waiting = false;
            }
        }
    }

    fn launch(&mut self, plan: &js_scaler::AcquirePlan, tick: u64) -> AcquireResult {
        let found = self
            .machines
            .iter()
            .find(|machine| machine.plan.acquire == plan.acquire);
        let cloud = match found {
            Some(machine) => machine.cloud.clone(),
            None => {
                self.created += 1;
                let cloud = format!("i-{}", self.created);
                self.machines.push(Machine {
                    plan: plan.clone(),
                    cloud: cloud.clone(),
                    status: HostStatus::Booting,
                    born: tick,
                });
                cloud
            }
        };
        accepted(plan, &cloud)
    }

    /// Terminates `id` and reads it back: the first call finds it terminating, the next
    /// finds it gone.
    fn terminate(&mut self, id: &HostRef) -> Released {
        let at = self.machines.iter().position(|each| each.cloud == id.0);
        match at {
            None => Released::Gone,
            Some(at) if self.machines[at].status == HostStatus::Terminating => {
                self.machines.remove(at);
                Released::Gone
            }
            Some(at) => {
                self.machines[at].status = HostStatus::Terminating;
                Released::Terminating(HostStatus::Terminating)
            }
        }
    }

    /// Runs one action as the shell would.
    fn apply(&mut self, action: &Action, tick: u64, process: &mut Process) {
        match action {
            Action::Save(records) => self.saved = Some(records.encode().unwrap()),
            Action::Mint { host, .. } => self.log.push(format!("mint:{host}")),
            Action::Acquire(plan) => {
                self.log.push(format!("acquire:{}", plan.acquire));
                process.acquired.push(self.launch(plan, tick));
            }
            Action::Drain(name) => self.mark(name, Mark::Draining, tick),
            Action::Undrain(name) => self.unmark(name),
            Action::ReleaseHost(name) => self.mark(name, Mark::Releasing, tick),
            Action::Revoke(name) => self.log.push(format!("revoke:{name}")),
            Action::Release { host, id } => {
                self.log.push(format!("release:{host}"));
                let entry = self.hosts.iter().find(|each| &each.name == host);
                if let Some(entry) = entry {
                    let quiet = tick >= entry.busy_until;
                    let marked = entry.mark == Some(Mark::Releasing);
                    assert!(
                        quiet && marked,
                        "{host} was released while it could take work"
                    );
                }
                let answer = self.terminate(id);
                process.released.push(ReleaseResult {
                    host: host.clone(),
                    answer,
                });
            }
            Action::ForgetNetwork(name) => self.log.push(format!("forget:{name}")),
            Action::RemoveHost(name) => self.hosts.retain(|each| &each.name != name),
        }
    }

    /// The registry's `drain` and `release_host`: the mark is set only while the host is
    /// quiet, and `release_host` only on a draining host.
    fn mark(&mut self, name: &str, mark: Mark, tick: u64) {
        let Some(entry) = self.hosts.iter_mut().find(|each| each.name == name) else {
            return;
        };
        let quiet = tick >= entry.busy_until;
        let allowed = match mark {
            Mark::Draining => entry.mark.is_none(),
            _ => entry.mark == Some(Mark::Draining),
        };
        if quiet && allowed {
            entry.mark = Some(mark);
        }
    }

    fn unmark(&mut self, name: &str) {
        let found = self.hosts.iter_mut().find(|each| each.name == name);
        found.into_iter().for_each(|entry| {
            entry.mark = entry.mark.filter(|mark| *mark != Mark::Draining);
        });
    }
}

fn one_machine() -> Settings {
    let mut limits = settings("gpu");
    limits.max_instances = 1;
    limits
}

/// A scaler that starts from `world.saved`, or from nothing if no file was written.
fn restart(world: &World, drawn: u64) -> Rig {
    let mut rig = Rig::new(one_machine());
    rig.drawn = drawn;
    if let Some(text) = &world.saved {
        rig.state = State::new(Records::decode(text).unwrap());
    }
    rig
}

/// Plays the scenario for [`TICKS`] ticks. With `crash` set to `(tick, call)`, the scaler
/// dies in that tick just before it runs its call number `call` (the number of calls when
/// it dies after all of them), and a new one starts from the file at the next tick. Returns
/// the world, and how many calls each tick made.
fn play(crash: Option<(u64, usize)>) -> (World, Vec<usize>) {
    let mut world = World::new();
    let mut rig = restart(&world, 1000);
    let mut process = Process::default();
    let mut calls = Vec::new();
    for tick in 0..TICKS {
        world.hellos(tick);
        rig.input.waiting = if world.waiting {
            vec![request("r1")]
        } else {
            Vec::new()
        };
        rig.input.hosts = world.facts(tick);
        rig.pool().held = Some(world.list());
        rig.input.acquired = std::mem::take(&mut process.acquired);
        rig.input.released = std::mem::take(&mut process.released);
        let actions = rig.tick(at(tick * MINUTE));
        calls.push(actions.len());
        for (index, action) in actions.iter().enumerate() {
            if crash == Some((tick, index)) {
                break;
            }
            world.apply(action, tick, &mut process);
        }
        if crash.is_some_and(|(at_tick, index)| at_tick == tick && index <= actions.len()) {
            let drawn = rig.drawn;
            rig = restart(&world, drawn);
            process = Process::default();
        }
        assert!(
            world.machines.len() <= 1,
            "two machines at once at tick {tick}"
        );
    }
    (world, calls)
}

/// The position of the first entry of `log` that starts with `prefix`.
fn first(log: &[String], prefix: &str) -> Option<usize> {
    log.iter().position(|entry| entry.starts_with(prefix))
}

/// Asserts the safety of the shell's records at every point a crash can fall: for the
/// crash in each tick that made calls, before each of its calls and after the last, the
/// request gets exactly one machine (a crash before the records were written leaves no
/// record and launches once afterwards; a crash after them, with or without the provider's
/// answer, never launches again for the record), no two machines ever exist at once under
/// a cap of one, the machine's token is revoked before it is first released, no machine is
/// released while its host could take work, and by the end every machine is gone and every
/// record is settled. It catches a record written after its call (a crash between the two
/// would double-launch), a restart that mints or acquires again for a record it finds,
/// and a release that can cut work.
#[test]
fn a_crash_at_any_step_launches_once_and_releases_safely() {
    let (clean, calls) = play(None);
    assert_eq!(clean.created, 1);
    let mut crashes = 0;
    let ticks_with_calls = calls.iter().enumerate().filter(|(_, made)| **made > 0);
    for (tick, made) in ticks_with_calls {
        for index in 0..=*made {
            let tick = u64::try_from(tick).unwrap();
            let (world, _) = play(Some((tick, index)));
            crashes += 1;
            let at = format!("crash in tick {tick} before call {index}");
            assert_eq!(world.created, 1, "{at}");
            assert!(world.machines.is_empty(), "{at}: a machine is left");
            assert!(
                first(&world.log, "release:").is_some(),
                "{at}: nothing was released"
            );
            for (index, entry) in world.log.iter().enumerate() {
                if let Some(name) = entry.strip_prefix("release:") {
                    let earlier = &world.log[..index];
                    let revoked = earlier.iter().any(|each| *each == format!("revoke:{name}"));
                    assert!(
                        revoked,
                        "{at}: {name} was terminated before its token was revoked"
                    );
                }
            }
            let state = restart(&world, 0).state;
            let live = state.records.instances.iter().filter(|each| each.counts());
            assert_eq!(live.count(), 0, "{at}: a record still counts");
            let phases: Vec<Phase> = state.records.instances.iter().map(|r| r.phase).collect();
            assert!(phases.contains(&Phase::Gone), "{at}: {phases:?}");
        }
    }
    assert!(
        crashes >= 12,
        "the scenario made too few calls to be a test: {crashes}"
    );
}

/// Asserts the baseline the crash test compares with: with no crash the first tick
/// records, mints and acquires in that order, the rest of the run launches nothing more,
/// and the machine is revoked, released until the provider reads it back gone, and removed
/// from the registry, with the revoke before the release.
#[test]
fn without_a_crash_one_machine_comes_and_goes() {
    let (world, calls) = play(None);
    assert_eq!(calls[0], 3);
    assert_eq!(world.created, 1);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &world.log {
        let kind = entry.split(':').next().unwrap();
        *counts.entry(kind).or_default() += 1;
    }
    assert_eq!(counts["mint"], 1);
    assert_eq!(counts["acquire"], 1);
    assert_eq!(counts["revoke"], 1);
    assert!(
        counts["release"] >= 2,
        "the release is read back: {counts:?}"
    );
    assert!(world.hosts.is_empty());
    assert!(world.machines.is_empty());
}
