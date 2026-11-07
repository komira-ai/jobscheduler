//! The five invariants of an elastic pool's cloud: [`Invariant::InstancesWithinLimits`],
//! [`Invariant::OnlyOwnMachinesReleased`], [`Invariant::NoAcquireWhilePlanOnly`] and
//! [`Invariant::ThrottleHonoured`] after every step, and
//! [`Invariant::LaunchedMachinesSettle`] at the end.
//!
//! Each reads [`Observations::cloud`]: an invariant about an argument of a call or its time
//! reads the recording wrapper's calls, one about what the cloud holds reads what the cell
//! read of the fake. A step reads only the calls recorded since the last one, each once,
//! and the count of running machines. A cell without an elastic pool has nothing to check.

use std::collections::BTreeSet;

use js_elastic_fake::THROTTLE_WAIT;
use js_sim::text::time;
use js_types::sched::FarmTime;

use super::{Found, Invariant};
use crate::elastic::{AcquireAnswer, CloudCall, CloudEvent, CloudFacts};
use crate::observe::Observations;
use crate::text;

/// What the step invariants of the cloud have read.
#[derive(Clone, Debug, Default)]
pub(super) struct Indices {
    /// How many of the cloud's calls were read.
    read: usize,
    /// Since when the pool's brake reads tripped, with no reset since.
    tripped: Option<FarmTime>,
    /// When the cloud last answered an acquire with a 429.
    throttled: Option<FarmTime>,
    /// The machines of this scheduler: those an acquire was answered with, and those a
    /// list showed (a list shows only the machines an acquire launched).
    own: BTreeSet<String>,
}

impl Indices {
    /// Checks the calls recorded since the last step, and the count of running machines.
    pub(super) fn step(&mut self, observations: &Observations, found: &mut Found) {
        let Some(cloud) = &observations.cloud else {
            return;
        };
        for call in cloud.calls.iter().skip(self.read) {
            self.read_call(cloud, call, found);
        }
        self.read = cloud.calls.len();
        let running = u64::try_from(cloud.running.len()).unwrap_or(u64::MAX);
        if running > cloud.max_instances {
            let detail = format!(
                "the cloud of pool {} runs {running} of this scheduler's machines, past its \
                 max_instances of {}",
                cloud.pool, cloud.max_instances
            );
            found.flag(Invariant::InstancesWithinLimits, detail);
        }
    }

    fn read_call(&mut self, cloud: &CloudFacts, call: &CloudCall, found: &mut Found) {
        match &call.event {
            CloudEvent::Acquire {
                offer,
                zone,
                answer,
                ..
            } => {
                let what = format!(
                    "an acquire of offer {offer} in zone {} at {}",
                    zone.as_deref().unwrap_or("-"),
                    time(call.at)
                );
                self.acquired(cloud, call.at, &what, answer, found);
            }
            CloudEvent::Release { machine, .. } => {
                if !self.own.contains(machine) {
                    let detail = format!(
                        "a release of machine {machine} at {}, which this scheduler never \
                         launched",
                        time(call.at)
                    );
                    found.flag(Invariant::OnlyOwnMachinesReleased, detail);
                }
            }
            CloudEvent::List { listed } => {
                self.own.extend(listed.iter().flatten().cloned());
            }
            CloudEvent::Brake { tripped: true } => {
                self.tripped = self.tripped.or(Some(call.at));
            }
            CloudEvent::Brake { tripped: false } | CloudEvent::BrakeReset => {
                self.tripped = None;
            }
            // The acquire it follows was checked when it was read.
            CloudEvent::Crash => {}
        }
    }

    /// Checks an acquire made at `at` (`what` names it) and notes its answer.
    fn acquired(
        &mut self,
        cloud: &CloudFacts,
        at: FarmTime,
        what: &str,
        answer: &AcquireAnswer,
        found: &mut Found,
    ) {
        if let Some(since) = self.tripped {
            let detail = format!(
                "{what}, while the brake of pool {} has read tripped since {} with no reset",
                cloud.pool,
                time(since)
            );
            found.flag(Invariant::NoAcquireWhilePlanOnly, detail);
        }
        let wait = |then: &FarmTime| at.saturating_duration_since(*then) < THROTTLE_WAIT;
        if let Some(then) = self.throttled.filter(wait) {
            let detail = format!(
                "{what}, inside the {}s throttle wait of the 429 answered at {}",
                THROTTLE_WAIT.as_secs(),
                time(then)
            );
            found.flag(Invariant::ThrottleHonoured, detail);
        }
        match answer {
            AcquireAnswer::Accepted { machine } => {
                self.own.insert(machine.clone());
            }
            AcquireAnswer::Deferred(wait) if *wait == THROTTLE_WAIT => {
                self.throttled = Some(at);
            }
            _ => {}
        }
    }
}

/// [`Invariant::LaunchedMachinesSettle`]: once quiet, every machine of this scheduler the
/// cloud still runs has work (its agent runs a run or a job) or is leaving the pool
/// (drained or being released).
pub(super) fn settle(observations: &Observations, found: &mut Found) {
    let Some(cloud) = &observations.cloud else {
        return;
    };
    for id in &cloud.running {
        let machine = cloud.machines.get(id);
        let host = machine.map(|machine| &machine.host);
        let agent = host.and_then(|host| observations.agents.get(host));
        let busy = agent.is_some_and(|agent| !agent.running.is_empty() || !agent.jobs.is_empty());
        let listed = host.and_then(|host| observations.server.hosts.get(host));
        let leaving = listed.is_some_and(|listed| listed.leaving);
        if !busy && !leaving {
            let name = machine.map_or_else(
                || format!("machine {id}"),
                |machine| {
                    format!(
                        "machine m{:03} ({id}, host {})",
                        machine.number,
                        text::host(&machine.host)
                    )
                },
            );
            let joined = machine.is_some_and(|machine| machine.joined);
            let joined = if joined {
                "it joined"
            } else {
                "it never joined"
            };
            let detail = format!(
                "{name} still runs with no work and no drain once the cell is quiet ({joined})"
            );
            found.flag(Invariant::LaunchedMachinesSettle, detail);
        }
    }
}
