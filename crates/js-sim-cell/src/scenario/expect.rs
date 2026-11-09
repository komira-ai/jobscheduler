//! What a scenario may state beyond the invariants: a small closed set, judged once per
//! seed after the end check ([`super::Scenario::expect`]) or at a virtual time
//! ([`super::Scenario::expect_at`]).
//!
//! Every statement but [`Expect::answer`] and the last ten reads what the cell observed
//! (`launched_on.rs` and `held_after_runs.rs` hold the two that need more than a count).
//! [`Expect::answer`] reads an answer the client kept (`answer.rs`). The two from
//! [`Expect::pool_tripped`] on read the elastic pool's cloud and its brake when they are
//! judged (`cloud.rs`). The eight from [`Expect::jobs_read`] on read the server's job and
//! allocation tables as the cell read them, and the jobs the agents launched (`jobs.rs`).

use std::time::Duration;

use js_agent_wire::Phase;
use js_jobs::{AllocationStep, JobState};
use js_registry::{HostState, HostView};
use js_sim::text::list;
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::Value;

use crate::bus::agent_name;
use crate::cell::Cell;
use crate::observe::{ClientEvent, HowEnded, ItemId, ItemRecord};
use crate::server::ServerNode;

use super::answer::{self, Call};
use super::cloud::{self, CloudFact};
use super::jobs::{self, JobFact};
use super::{held_after_runs, launched_on};

/// How the server's listing shows a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostIs {
    /// Listed and heard within its grace.
    Live,
    /// Listed, and silent past its grace.
    Lost,
    /// Not listed: never seated, or forgotten.
    Absent,
}

impl HostIs {
    /// A stable kebab-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        ["live", "lost", "absent"][self as usize]
    }
}

/// One statement about the end of every seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expect(Wanted);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Wanted {
    Seen(Seen),
    Answer(Call, u16, Option<Value>),
    Cloud(CloudFact),
    Job(JobFact),
}

/// A statement over what the cell observed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    AllTerminal,
    Count(usize, HowEnded),
    Host(usize, HostIs),
    HeldAndReleased(usize),
    EveryHostServing,
    Launched(Vec<usize>, FarmTime),
    NotServing(Vec<usize>),
    HeldAfterRuns(ItemId),
}

impl Expect {
    /// Every item submitted has a terminal answer (one the client gave up on counts).
    #[must_use]
    pub fn all_items_terminal() -> Self {
        Self(Wanted::Seen(Seen::AllTerminal))
    }

    /// Exactly `count` items ended `how`.
    #[must_use]
    pub fn terminal_count(count: usize, how: HowEnded) -> Self {
        Self(Wanted::Seen(Seen::Count(count, how)))
    }

    /// The server's listing shows agent `index`'s host (`h000` is 0) as `state`.
    #[must_use]
    pub fn host_state(index: usize, state: HostIs) -> Self {
        Self(Wanted::Seen(Seen::Host(index, state)))
    }

    /// Exactly `count` items were held and released: the client heard each placed on a
    /// host and heard it done with the job's result.
    #[must_use]
    pub fn held_and_released(count: usize) -> Self {
        Self(Wanted::Seen(Seen::HeldAndReleased(count)))
    }

    /// The server's listing shows every agent's host live and serving: its last phase
    /// `serving`, nothing booked, not releasing, not set aside and not tainted.
    #[must_use]
    pub fn every_host_serving() -> Self {
        Self(Wanted::Seen(Seen::EveryHostServing))
    }

    /// Every agent of `agents` (by index, `h000` is 0) launched at least one run at or
    /// after `after` from the start, by the cell's launch ledger: those hosts took work
    /// then.
    #[must_use]
    pub fn launched_on(agents: &[usize], after: Duration) -> Self {
        let at = FarmTime::default().saturating_add(after);
        Self(Wanted::Seen(Seen::Launched(agents.to_vec(), at)))
    }

    /// The server's listing shows every agent's host serving (as
    /// [`Expect::every_host_serving`] reads it) but the hosts of `agents` (by index), and
    /// none of those: how a scenario states that a refusal took hosts out of service (a
    /// `host-unsafe` refusal taints the host for good).
    #[must_use]
    pub fn serving_but(agents: &[usize]) -> Self {
        Self(Wanted::Seen(Seen::NotServing(agents.to_vec())))
    }

    /// `item` was placed on a host, and every run launched on that host before the
    /// placement had completed by then, by the cell's ledger: the host was given to the
    /// item only after the runs that were on it ended.
    #[must_use]
    pub fn held_after_the_runs_on_its_host(item: ItemId) -> Self {
        Self(Wanted::Seen(Seen::HeldAfterRuns(item)))
    }

    /// The server's scaler reads the brake of elastic pool `pool` tripped (`true`: it
    /// launches nothing until an operator resets it) or not, when this is judged. A pool
    /// that is not the cell's elastic pool is `no-pool`.
    #[must_use]
    pub fn pool_tripped(pool: &str, tripped: bool) -> Self {
        Self(Wanted::Cloud(CloudFact::Tripped(pool.to_owned(), tripped)))
    }

    /// The cloud of elastic pool `pool` has launched exactly `count` machines for the
    /// scheduler by the time this is judged, by the elastic fake: every machine it made
    /// (`ElasticFake::launched`) but those added as another's
    /// ([`crate::Cell::add_foreign_machine`]), whether it still runs or not. A pool that is
    /// not the cell's elastic pool is `no-pool`.
    #[must_use]
    pub fn machines_launched(pool: &str, count: usize) -> Self {
        Self(Wanted::Cloud(CloudFact::Launched(pool.to_owned(), count)))
    }

    /// The answer to `call` was `status`, with `body` (compared as JSON) when one is
    /// given. It reads the answers the client kept ([`Cell::answers`]); a call not made is
    /// `no-answer`.
    #[must_use]
    pub fn answer(call: Call, status: u16, body: Option<Value>) -> Self {
        Self(Wanted::Answer(call, status, body))
    }

    /// Exactly `count` jobs were read in the server's job table, whatever their state:
    /// how a scenario states that a submit sent again under the same job id made no
    /// second job.
    #[must_use]
    pub fn jobs_read(count: usize) -> Self {
        Self(Wanted::Job(JobFact::Read(count)))
    }

    /// Exactly `count` jobs were last read in the server's job table in a state that has
    /// ended (succeeded, failed, cancelled, lost or failed to start).
    #[must_use]
    pub fn jobs_ended(count: usize) -> Self {
        Self(Wanted::Job(JobFact::Ended(count)))
    }

    /// Exactly `count` jobs were last read in the server's job table in `state`.
    #[must_use]
    pub fn jobs_in_state(count: usize, state: JobState) -> Self {
        Self(Wanted::Job(JobFact::In(count, state)))
    }

    /// The job of `item` (`sim-job-<n>`, as the job front submits it) was last read in
    /// the server's job table in `state`; a job never read is `unread`.
    #[must_use]
    pub fn job_state(item: ItemId, state: JobState) -> Self {
        Self(Wanted::Job(JobFact::State(item, state)))
    }

    /// Exactly `count` allocations of the job of `item` were read in the server's
    /// allocation table: one per placement, so a job placed again has more than one.
    #[must_use]
    pub fn allocations_of(item: ItemId, count: usize) -> Self {
        Self(Wanted::Job(JobFact::Allocations(item, count)))
    }

    /// The allocation of the job of `item` was last read at `step` (booked, releasing or
    /// freed); an allocation never read is `unread`.
    #[must_use]
    pub fn allocation_step(item: ItemId, step: AllocationStep) -> Self {
        Self(Wanted::Job(JobFact::Step(item, step)))
    }

    /// No allocation was last read booked or releasing in the server's allocation table:
    /// every allocation read was freed, so once every job is terminal nothing is left
    /// booked for one.
    #[must_use]
    pub fn no_allocation_live() -> Self {
        Self(Wanted::Job(JobFact::NoneLive))
    }

    /// No job was launched more than once by the cell's launch ledger, under any
    /// allocation: `at-most-one-launch` counts a launch per job and allocation, so this is
    /// how a scenario states that a job was never run again under a second placement.
    #[must_use]
    pub fn each_job_launched_once() -> Self {
        Self(Wanted::Job(JobFact::LaunchedOnce))
    }

    /// `None` when `cell` meets the statement; otherwise what it wanted and what it got,
    /// as `<statement>: wanted <w>, got <g>`.
    #[must_use]
    pub fn unmet(&self, cell: &Cell) -> Option<String> {
        let (name, wanted, got) = match &self.0 {
            Wanted::Answer(call, status, body) => {
                let [name, wanted, got] =
                    answer::judged(call, *status, body.as_ref(), cell.answers());
                (name, wanted, got)
            }
            Wanted::Cloud(fact) => {
                let [name, wanted, got] = cloud::judged(fact, cell);
                (name, wanted, got)
            }
            Wanted::Job(fact) => {
                let [name, wanted, got] = jobs::judged(fact, cell.observations());
                (name, wanted, got)
            }
            Wanted::Seen(seen) => observed(seen, cell),
        };
        (wanted != got).then(|| format!("{name}: wanted {wanted}, got {got}"))
    }
}

/// The statement, what it wanted and what it got, of one of the statements over what
/// `cell` observed.
fn observed(seen: &Seen, cell: &Cell) -> (String, String, String) {
    let observations = cell.observations();
    let items = observations.items.values();
    match seen {
        Seen::AllTerminal => {
            let ended = items.filter(|record| ended(record).is_some()).count();
            let wanted = observations.items.len();
            (
                "all-items-terminal".to_owned(),
                wanted.to_string(),
                ended.to_string(),
            )
        }
        Seen::Count(count, how) => {
            let ended = items.filter(|record| ended(record) == Some(*how)).count();
            let name = format!("terminal-count({})", how.name());
            (name, count.to_string(), ended.to_string())
        }
        Seen::Host(index, state) => {
            let host = HostId::new(agent_name(*index).as_str()).ok();
            let listed = host.as_ref().and_then(|h| observations.server.hosts.get(h));
            let lost = [HostIs::Live, HostIs::Lost];
            let is = listed.map_or(HostIs::Absent, |l| lost[usize::from(l.lost)]);
            let name = format!("host-state(h{index:03})");
            (name, state.name().to_owned(), is.name().to_owned())
        }
        Seen::HeldAndReleased(count) => {
            let both = items.filter(|record| held_and_released(record)).count();
            let name = "held-and-released".to_owned();
            (name, count.to_string(), both.to_string())
        }
        Seen::EveryHostServing => {
            let hosts = cell.server().map(ServerNode::hosts).unwrap_or_default();
            let serving = hosts.iter().filter(|host| serving(host)).count();
            let name = "every-host-serving".to_owned();
            (name, cell.agent_count().to_string(), serving.to_string())
        }
        Seen::Launched(agents, after) => {
            let [name, wanted, got] = launched_on::judged(agents, *after, cell.ledger());
            (name, wanted, got)
        }
        Seen::NotServing(agents) => {
            let hosts = cell.server().map(ServerNode::hosts).unwrap_or_default();
            let listed_serving = |name: &String| {
                let mut listed = hosts.iter();
                listed.any(|host| (host.id.as_str() == name.as_str()) & serving(host))
            };
            let names = (0..cell.agent_count()).map(|i| agent_name(i).as_str().to_owned());
            let not: Vec<String> = names.filter(|name| !listed_serving(name)).collect();
            let wanted = agents.iter().map(|i| agent_name(*i).as_str().to_owned());
            let wanted: Vec<String> = wanted.collect();
            let name = "hosts-not-serving".to_owned();
            (
                name,
                list(&wanted, String::clone),
                list(&not, String::clone),
            )
        }
        Seen::HeldAfterRuns(item) => {
            let [name, wanted, got] = held_after_runs::judged(observations, *item);
            (name, wanted, got)
        }
    }
}

/// How the item ended, as the client first heard it.
pub(crate) fn ended(record: &ItemRecord) -> Option<HowEnded> {
    let terminal = |event: &ClientEvent| match event {
        ClientEvent::Terminal { how } => Some(*how),
        _ => None,
    };
    record.heard.iter().find_map(|heard| terminal(&heard.event))
}

/// Whether the client heard the item placed and heard its release answered.
fn held_and_released(record: &ItemRecord) -> bool {
    let mut heard = record.heard.iter().map(|heard| &heard.event);
    let placed = heard.any(|event| matches!(event, ClientEvent::Placed { .. }));
    let released = record
        .heard
        .iter()
        .any(|h| h.event == ClientEvent::Released);
    placed & released
}

/// Whether the listing shows `host` live and serving, with nothing booked, set aside or
/// left to finish, and untainted.
fn serving(host: &HostView) -> bool {
    let idle = host.leases.is_empty() & !host.releasing & host.mark.is_none();
    (host.state == HostState::Live) & (host.phase == Phase::Serving) & idle & host.taint.is_none()
}
