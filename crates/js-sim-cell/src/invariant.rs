//! What must always be true of the cell: the five properties of "exactly once, nothing
//! double-booked", and the [`Checker`] that tests them.
//!
//! The checker runs a step check ([`Checker::step`]) after every kernel step and every
//! client call, and an end check ([`Checker::end`]) once, after the scenario has been
//! quiet (no fault active) for its `settle_within`. Each check returns every
//! [`Violation`] it finds; none returns the server's opinion of itself.
//!
//! # Cost
//!
//! A step check that does not re-read the server's listing costs what the step touched,
//! not what the cell holds, so checking such a step costs the same with 20 hosts as with
//! 200. The caller names in [`Touched`] the agents,
//! the items and whether the server's listing the step changed or re-read; the checker
//! then reads, for each touched agent, that agent's own runs; for each touched item,
//! that item's own events; for each ledger entry appended since the last step, one
//! lookup; and otherwise only the indices it keeps (which agent runs each run, which run
//! was launched, which whole-host item holds which host), each lookup logarithmic. An
//! agent, item or host the step did not touch is not re-read, so a fact that is wrong
//! there is caught at the step that touches it, not before. A step that touched the
//! server reads the whole listing, at a cost in the hosts and bookings it lists; the
//! cell is to re-read the listing at every client call and once per virtual second, not
//! at every step. Only the end check scans everything.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use js_sim::text::{list, time};
use js_types::sched::FarmTime;
use js_types::{HostId, LeaseId};

use crate::observe::{
    AgentTruth, Ask, ClientEvent, Heard, HowEnded, ItemId, ItemRecord, Launch, Observations, RunId,
};
use crate::text;

/// One property the cell must keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invariant {
    /// Step check. (a) By agent ground truth, the runs one agent runs at one time never
    /// need more than its capacity (cpu, memory, gpus). (b) No run is run by two agents
    /// at the same virtual time. (c) No host runs a run of another lease while a
    /// whole-host item (as the client asked for it, not as the server lists it) holds a
    /// lease on that host. By the client's facts, a whole-host item holds its lease on a
    /// host from the moment the client learned it was placed there until the client
    /// sent its release, or heard that the server ended the run (lost or forgotten),
    /// whichever comes first. A run that only succeeded, failed or was cancelled keeps
    /// the hold, since its lease stays booked until released; an answer to the release
    /// lost to a fault cannot stretch the hold past the server freeing the host. At a
    /// step that touched the server, the item also holds the host while the server's
    /// listing books its lease there, so a release the server never processed is still
    /// covered. The listing is read for (c) only at such steps.
    NoDoubleBooking,
    /// Step check. Summed over the cell's launch ledger, each run is launched at most
    /// once, across agent deaths and replacements.
    AtMostOneLaunch,
    /// Step check. Once an item has a terminal answer it has no other terminal answer
    /// and no `Started`: it reaches terminal once.
    OneTerminalAnswer,
    /// End check, evaluated after the scenario's `settle_within` of virtual time in
    /// which no fault was active. Every submitted item is terminal (a client `GaveUp`
    /// counts), no lease is held by the client (it learned one and its release was not
    /// answered), and no host shows a booking on the server's listing. A booking that
    /// no agent holds is named once, by [`Invariant::BookingsMatchAgents`]; the
    /// bookings named here are those whose host still runs a run of the lease.
    SettlesWhenQuiet,
    /// End check, evaluated with [`Invariant::SettlesWhenQuiet`]. Nothing is booked on
    /// the server's listing of a host that the host's agent does not hold (it runs no
    /// run of that lease), and nothing runs on an agent that nothing books (the
    /// server's listing of that host books no such lease). At the end of a settled
    /// scenario both sides are empty: this is the leak detector. It is not evaluated
    /// at every step because a booking legitimately precedes its start by a heartbeat
    /// and a run legitimately outlives its release by one.
    BookingsMatchAgents,
}

impl Invariant {
    /// Every invariant.
    pub const ALL: [Invariant; 5] = [
        Invariant::NoDoubleBooking,
        Invariant::AtMostOneLaunch,
        Invariant::OneTerminalAnswer,
        Invariant::SettlesWhenQuiet,
        Invariant::BookingsMatchAgents,
    ];

    /// A stable kebab-case name, as a failure writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Invariant::NoDoubleBooking => "no-double-booking",
            Invariant::AtMostOneLaunch => "at-most-one-launch",
            Invariant::OneTerminalAnswer => "one-terminal-answer",
            Invariant::SettlesWhenQuiet => "settles-when-quiet",
            Invariant::BookingsMatchAgents => "bookings-match-agents",
        }
    }

    /// When the invariant is checked.
    #[must_use]
    pub fn checked(self) -> Checked {
        match self {
            Invariant::NoDoubleBooking
            | Invariant::AtMostOneLaunch
            | Invariant::OneTerminalAnswer => Checked::EveryStep,
            Invariant::SettlesWhenQuiet | Invariant::BookingsMatchAgents => Checked::AtEnd,
        }
    }
}

/// When an invariant is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checked {
    /// After every kernel step and every client call ([`Checker::step`]).
    EveryStep,
    /// Once, when the scenario has settled ([`Checker::end`]).
    AtEnd,
}

/// An invariant that did not hold, when, and what the checker saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The invariant.
    pub invariant: Invariant,
    /// The virtual time of the observations it was found in.
    pub at: FarmTime,
    /// What did not hold, in hand-written text (no `Debug`).
    pub detail: String,
}

/// One line: `<invariant> at <virtual ms>: <detail>`. The scenario runner (a later part
/// of the cell) is to add the
/// scenario's name, its seed and the trace.
impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: {}",
            self.invariant.name(),
            time(self.at),
            self.detail
        )
    }
}

/// What one step touched: the agents whose truth it changed, the items the client heard
/// about, and whether the server's listing was re-read. A step check reads only these
/// (see the module's cost rule).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touched {
    agents: BTreeSet<HostId>,
    items: BTreeSet<ItemId>,
    server: bool,
}

impl Touched {
    /// A step that touched nothing.
    #[must_use]
    pub fn nothing() -> Self {
        Self::default()
    }

    /// Everything `observations` hold: every agent, every item and the server's listing.
    /// A step check over it costs a full scan; tests use it to check a hand-built
    /// history in one step.
    #[must_use]
    pub fn everything_in(observations: &Observations) -> Self {
        Self {
            agents: observations.agents.keys().cloned().collect(),
            items: observations.items.keys().copied().collect(),
            server: true,
        }
    }

    /// This step, also touching the agent of `host` (a new or replaced agent process
    /// too, and a host whose agent went away).
    #[must_use]
    pub fn agent(mut self, host: HostId) -> Self {
        self.agents.insert(host);
        self
    }

    /// This step, also touching `item`.
    #[must_use]
    pub fn item(mut self, item: ItemId) -> Self {
        self.items.insert(item);
        self
    }

    /// This step, also re-reading the server's listing.
    #[must_use]
    pub fn server(mut self) -> Self {
        self.server = true;
        self
    }
}

/// The end check was asked for before the scenario settled, and refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotSettled {
    /// The virtual time of the observations.
    pub at: FarmTime,
    /// Whether a fault is active.
    pub fault_active: bool,
    /// The first virtual time the end check may run at, if no fault starts first.
    pub settles_at: FarmTime,
}

impl fmt::Display for NotSettled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.fault_active {
            write!(
                f,
                "end check refused at {}: a fault is active",
                time(self.at)
            )
        } else {
            write!(
                f,
                "end check refused at {}: not quiet long enough until {}",
                time(self.at),
                time(self.settles_at)
            )
        }
    }
}

impl std::error::Error for NotSettled {}

/// Tests the invariants over the observations of one run, keeping the indices that let
/// a step check read only what the step touched.
///
/// One checker follows one run from its first step: it remembers which ledger entries
/// it has read, which agent it last saw running each run, and which host each
/// whole-host item holds.
#[derive(Clone, Debug, Default)]
pub struct Checker {
    /// How many ledger entries were read.
    ledger_read: usize,
    /// The first launch of each run.
    launched: BTreeMap<RunId, Launch>,
    /// The agents each run was seen running on and, when last asked, still ran it.
    /// Usually one; more only while a run is double-booked, so a duplicate that
    /// survives the agent first indexed is still found.
    runner: BTreeMap<RunId, BTreeSet<HostId>>,
    /// The host each whole-host item holds, by the client's facts.
    holds: BTreeMap<ItemId, HostId>,
    /// The whole-host items holding each host, with their leases.
    held: BTreeMap<HostId, BTreeMap<ItemId, LeaseId>>,
    /// The leases of whole-host items, with their items.
    whole: BTreeMap<LeaseId, ItemId>,
}

impl Checker {
    /// A checker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks the step invariants ([`Checked::EveryStep`]) after one step that touched
    /// `touched`, and returns every violation found.
    pub fn step(&mut self, observations: &Observations, touched: &Touched) -> Vec<Violation> {
        let mut found = Found::at(observations.at);
        self.read_ledger(observations, &mut found);
        let items = touched.items.iter();
        for (item, record) in items.filter_map(|i| observations.items.get_key_value(i)) {
            self.read_item(*item, record, &mut found);
        }
        for host in &touched.agents {
            self.read_agent(observations, host, &mut found);
        }
        let mut hosts: BTreeSet<&HostId> = touched.agents.iter().collect();
        hosts.extend(touched.items.iter().filter_map(|i| self.holds.get(i)));
        if touched.server {
            let listed = observations.server.hosts.iter();
            let whole =
                listed.filter(|(_, host)| host.leases.iter().any(|l| self.whole.contains_key(l)));
            hosts.extend(whole.map(|(host, _)| host));
        }
        for host in hosts {
            self.whole_host(observations, host, touched.server, &mut found);
        }
        found.list
    }

    /// Checks the end invariants ([`Checked::AtEnd`]) over `observations`, a full scan,
    /// and returns every violation found.
    ///
    /// # Errors
    ///
    /// [`NotSettled`], and no check, when a fault is active or less than
    /// `settle_within` of virtual time has passed since the observations'
    /// `quiet_since`. The quiet time is compared as a duration, so a window that would
    /// end past the last representable virtual time is never satisfied early.
    pub fn end(
        observations: &Observations,
        settle_within: Duration,
    ) -> Result<Vec<Violation>, NotSettled> {
        let settles_at = observations.quiet_since.saturating_add(settle_within);
        let quiet = observations
            .at
            .saturating_duration_since(observations.quiet_since);
        if observations.fault_active || quiet < settle_within {
            return Err(NotSettled {
                at: observations.at,
                fault_active: observations.fault_active,
                settles_at,
            });
        }
        let mut found = Found::at(observations.at);
        for (item, record) in &observations.items {
            let state = ItemState::of(record);
            if !state.terminal {
                let detail = format!("{} is not terminal", text::item(*item));
                found.flag(Invariant::SettlesWhenQuiet, detail);
            }
            if let Some(lease) = state.lease {
                let (item, lease) = (text::item(*item), text::lease(lease));
                let detail = format!("{item} still holds lease {lease}");
                found.flag(Invariant::SettlesWhenQuiet, detail);
            }
        }
        for (host, listed) in &observations.server.hosts {
            for lease in &listed.leases {
                let (name, booked) = (text::host(host), text::lease(*lease));
                if runs_lease(observations, host, *lease) {
                    let detail = format!("host {name} still books lease {booked}");
                    found.flag(Invariant::SettlesWhenQuiet, detail);
                } else {
                    let detail =
                        format!("host {name} books lease {booked} and its agent runs no run of it");
                    found.flag(Invariant::BookingsMatchAgents, detail);
                }
            }
        }
        for (host, agent) in &observations.agents {
            for (run, running) in &agent.running {
                if !books_lease(observations, host, running.lease) {
                    let detail = format!(
                        "host {} runs {} of lease {} and the server books no such lease there",
                        text::host(host),
                        text::run(run),
                        text::lease(running.lease)
                    );
                    found.flag(Invariant::BookingsMatchAgents, detail);
                }
            }
        }
        Ok(found.list)
    }

    /// `AtMostOneLaunch`: the ledger entries appended since the last step.
    fn read_ledger(&mut self, observations: &Observations, found: &mut Found) {
        for launch in observations.ledger.iter().skip(self.ledger_read) {
            match self.launched.entry(launch.run.clone()) {
                Entry::Occupied(first) => {
                    let first = first.get();
                    let detail = format!(
                        "{} launched on host {} at {}, already launched on host {} at {}",
                        text::run(&launch.run),
                        text::host(&launch.host),
                        time(launch.at),
                        text::host(&first.host),
                        time(first.at)
                    );
                    found.flag(Invariant::AtMostOneLaunch, detail);
                }
                Entry::Vacant(slot) => {
                    slot.insert(launch.clone());
                }
            }
        }
        self.ledger_read = observations.ledger.len();
    }

    /// `OneTerminalAnswer` for one touched item, and the whole-host indices from its
    /// events.
    fn read_item(&mut self, item: ItemId, record: &ItemRecord, found: &mut Found) {
        if let Some((first, again)) = second_answer(record) {
            let detail = format!(
                "{} heard {} at {} after {} at {}",
                text::item(item),
                answer(again),
                time(again.at),
                answer(first),
                time(first.at)
            );
            found.flag(Invariant::OneTerminalAnswer, detail);
        }
        if record.ask.whole_host {
            for heard in &record.heard {
                if let ClientEvent::Queued { lease, .. } = &heard.event {
                    self.whole.insert(*lease, item);
                }
            }
        }
        if let Some(host) = self.holds.remove(&item) {
            self.held.entry(host).or_default().remove(&item);
        }
        if let Some((host, lease)) = ItemState::of(record).whole_hold(&record.ask) {
            self.held
                .entry(host.clone())
                .or_default()
                .insert(item, lease);
            self.holds.insert(item, host);
        }
    }

    /// `NoDoubleBooking` (a) and (b) for one touched agent.
    fn read_agent(&mut self, observations: &Observations, host: &HostId, found: &mut Found) {
        let Some(agent) = observations.agents.get(host) else {
            return;
        };
        over_capacity(host, agent, found);
        for run in agent.running.keys() {
            let owners = self.runner.entry(run.clone()).or_default();
            owners.retain(|other| other == host || runs(observations, other, run));
            if let Some(other) = owners.iter().find(|other| *other != host) {
                let detail = format!(
                    "{} runs on host {} and on host {}",
                    text::run(run),
                    text::host(other),
                    text::host(host)
                );
                found.flag(Invariant::NoDoubleBooking, detail);
            }
            owners.insert(host.clone());
        }
    }

    /// `NoDoubleBooking` (c) on one host: the whole-host items holding it by the
    /// client's facts and, when the step re-read the server's listing, by the listing.
    fn whole_host(
        &self,
        observations: &Observations,
        host: &HostId,
        server_read: bool,
        found: &mut Found,
    ) {
        let by_client = self.held.get(host).into_iter().flatten();
        let mut holders: BTreeMap<LeaseId, ItemId> =
            by_client.map(|(item, lease)| (*lease, *item)).collect();
        if server_read {
            let listed = observations.server.hosts.get(host).into_iter();
            let leases = listed.flat_map(|l| &l.leases);
            let whole = leases.filter_map(|lease| self.whole.get_key_value(lease));
            holders.extend(whole.map(|(lease, item)| (*lease, *item)));
        }
        let agent = observations.agents.get(host).into_iter();
        for (run, running) in agent.flat_map(|a| &a.running) {
            let other = holders.iter().find(|(lease, _)| **lease != running.lease);
            if let Some((held, item)) = other {
                let detail = format!(
                    "host {} runs {} of lease {} while whole-host {} holds lease {} there",
                    text::host(host),
                    text::run(run),
                    text::lease(running.lease),
                    text::item(*item),
                    text::lease(*held)
                );
                found.flag(Invariant::NoDoubleBooking, detail);
            }
        }
    }
}

/// The violations of one check, at one virtual time.
struct Found {
    at: FarmTime,
    list: Vec<Violation>,
}

impl Found {
    fn at(at: FarmTime) -> Self {
        Self {
            at,
            list: Vec::new(),
        }
    }

    fn flag(&mut self, invariant: Invariant, detail: String) {
        self.list.push(Violation {
            invariant,
            at: self.at,
            detail,
        });
    }
}

/// `NoDoubleBooking` (a): the agent's runs need more than its capacity on some axis.
fn over_capacity(host: &HostId, agent: &AgentTruth, found: &mut Found) {
    // Summed in u128: a run count below 2^64 cannot overflow it, so no total is
    // clipped to a capacity it exceeds.
    let need = agent.running.values().map(|r| text::axes(&r.resources));
    let used = need.fold([0; 3], |sum: [u128; 3], add| {
        std::array::from_fn(|i| sum[i] + add[i])
    });
    let capacity = text::axes(&agent.capacity);
    let axes = text::AXES.iter().zip(used.iter().zip(capacity));
    let over: Vec<&str> = axes
        .filter(|(_, (used, cap))| **used > *cap)
        .map(|(name, _)| *name)
        .collect();
    if !over.is_empty() {
        let runs: Vec<String> = agent.running.keys().map(text::run).collect();
        let detail = format!(
            "host {} runs {} needing {} beyond its capacity {} (over on {})",
            text::host(host),
            list(&runs, String::clone),
            text::load(used),
            text::load(capacity),
            over.join(", ")
        );
        found.flag(Invariant::NoDoubleBooking, detail);
    }
}

/// Whether the agent of `host` runs `run` now.
fn runs(observations: &Observations, host: &HostId, run: &RunId) -> bool {
    let agent = observations.agents.get(host);
    agent.is_some_and(|a| a.running.contains_key(run))
}

/// Whether the agent of `host` runs a run of `lease` now.
fn runs_lease(observations: &Observations, host: &HostId, lease: LeaseId) -> bool {
    let agent = observations.agents.get(host);
    agent.is_some_and(|a| a.running.values().any(|r| r.lease == lease))
}

/// Whether the server's listing of `host` books `lease`.
fn books_lease(observations: &Observations, host: &HostId, lease: LeaseId) -> bool {
    let listed = observations.server.hosts.get(host);
    listed.is_some_and(|l| l.leases.contains(&lease))
}

/// The item's first terminal answer and the first terminal or `Started` heard after it.
fn second_answer(record: &ItemRecord) -> Option<(&Heard, &Heard)> {
    let mut heard = record.heard.iter();
    let first = heard.find(|h| matches!(h.event, ClientEvent::Terminal { .. }))?;
    let again =
        heard.find(|h| matches!(h.event, ClientEvent::Terminal { .. } | ClientEvent::Started))?;
    Some((first, again))
}

/// A terminal answer or a start: `terminal(<how>)` or `started`.
fn answer(heard: &Heard) -> String {
    match &heard.event {
        ClientEvent::Terminal { how } => format!("terminal({})", how.name()),
        _ => "started".to_owned(),
    }
}

/// What the client's events say of an item now.
#[derive(Default)]
struct ItemState {
    /// The lease it learned and has not released.
    lease: Option<LeaseId>,
    /// The host it learned it was placed on, since it last learned a lease.
    host: Option<HostId>,
    /// Whether it heard a terminal answer.
    terminal: bool,
    /// Whether the lease it learned last has ended on the client's side: the client
    /// sent its release, or heard the server end the run (lost or forgotten).
    hold_ended: bool,
}

impl ItemState {
    fn of(record: &ItemRecord) -> Self {
        let mut state = Self::default();
        for heard in &record.heard {
            match &heard.event {
                ClientEvent::Queued { lease, .. } => {
                    state.lease = Some(*lease);
                    state.host = None;
                    state.hold_ended = false;
                }
                ClientEvent::Placed { host } => state.host = Some(host.clone()),
                ClientEvent::Terminal {
                    how: HowEnded::Lost | HowEnded::Forgotten,
                } => {
                    state.terminal = true;
                    state.hold_ended = true;
                }
                ClientEvent::Terminal { .. } => state.terminal = true,
                ClientEvent::ReleaseSent => state.hold_ended = true,
                ClientEvent::Released => {
                    state.lease = None;
                    state.host = None;
                }
                ClientEvent::Submitted | ClientEvent::Started => {}
            }
        }
        state
    }

    /// The host and lease a whole-host item holds now, by the client's facts: placed,
    /// with no release sent and no answer that the server ended the run.
    fn whole_hold(self, ask: &Ask) -> Option<(HostId, LeaseId)> {
        if !ask.whole_host || self.hold_ended {
            return None;
        }
        let lease = self.lease?;
        Some((self.host?, lease))
    }
}
