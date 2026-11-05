//! What a scenario may state beyond the invariants: a small closed set, judged once per
//! seed after the end check.

use std::time::Duration;

use js_agent_wire::Phase;
use js_registry::{HostState, HostView};
use js_types::HostId;
use js_types::sched::FarmTime;

use crate::bus::agent_name;
use crate::cell::Cell;
use crate::observe::{ClientEvent, HowEnded, ItemRecord};
use crate::server::ServerNode;

use super::launched_on;

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
    AllTerminal,
    Count(usize, HowEnded),
    Host(usize, HostIs),
    HeldAndReleased(usize),
    EveryHostServing,
    Launched(Vec<usize>, FarmTime),
}

impl Expect {
    /// Every item submitted has a terminal answer (one the client gave up on counts).
    #[must_use]
    pub fn all_items_terminal() -> Self {
        Self(Wanted::AllTerminal)
    }

    /// Exactly `count` items ended `how`.
    #[must_use]
    pub fn terminal_count(count: usize, how: HowEnded) -> Self {
        Self(Wanted::Count(count, how))
    }

    /// The server's listing shows agent `index`'s host (`h000` is 0) as `state`.
    #[must_use]
    pub fn host_state(index: usize, state: HostIs) -> Self {
        Self(Wanted::Host(index, state))
    }

    /// Exactly `count` items were held and released: the client heard each placed on a
    /// host (for a whole-host item, the grant of its lease) and heard its release
    /// answered.
    #[must_use]
    pub fn held_and_released(count: usize) -> Self {
        Self(Wanted::HeldAndReleased(count))
    }

    /// The server's listing shows every agent's host live and serving: its last phase
    /// `serving`, nothing booked, not releasing, not set aside and not tainted.
    #[must_use]
    pub fn every_host_serving() -> Self {
        Self(Wanted::EveryHostServing)
    }

    /// Every agent of `agents` (by index, `h000` is 0) launched at least one run at or
    /// after `after` from the start, by the cell's launch ledger: those hosts took work
    /// then.
    #[must_use]
    pub fn launched_on(agents: &[usize], after: Duration) -> Self {
        let at = FarmTime::default().saturating_add(after);
        Self(Wanted::Launched(agents.to_vec(), at))
    }

    /// `None` when `cell` meets the statement; otherwise what it wanted and what it got,
    /// as `<statement>: wanted <w>, got <g>`.
    #[must_use]
    pub fn unmet(&self, cell: &Cell) -> Option<String> {
        let observations = cell.observations();
        let items = observations.items.values();
        let (name, wanted, got) = match &self.0 {
            Wanted::AllTerminal => {
                let ended = items.filter(|record| ended(record).is_some()).count();
                let wanted = observations.items.len();
                (
                    "all-items-terminal".to_owned(),
                    wanted.to_string(),
                    ended.to_string(),
                )
            }
            Wanted::Count(count, how) => {
                let ended = items.filter(|record| ended(record) == Some(*how)).count();
                let name = format!("terminal-count({})", how.name());
                (name, count.to_string(), ended.to_string())
            }
            Wanted::Host(index, state) => {
                let host = HostId::new(agent_name(*index).as_str()).ok();
                let listed = host.as_ref().and_then(|h| observations.server.hosts.get(h));
                let lost = [HostIs::Live, HostIs::Lost];
                let is = listed.map_or(HostIs::Absent, |l| lost[usize::from(l.lost)]);
                let name = format!("host-state(h{index:03})");
                (name, state.name().to_owned(), is.name().to_owned())
            }
            Wanted::HeldAndReleased(count) => {
                let both = items.filter(|record| held_and_released(record)).count();
                let name = "held-and-released".to_owned();
                (name, count.to_string(), both.to_string())
            }
            Wanted::EveryHostServing => {
                let hosts = cell.server().map(ServerNode::hosts).unwrap_or_default();
                let serving = hosts.iter().filter(|host| serving(host)).count();
                let name = "every-host-serving".to_owned();
                (name, cell.agent_count().to_string(), serving.to_string())
            }
            Wanted::Launched(agents, after) => {
                let [name, wanted, got] = launched_on::judged(agents, *after, cell.ledger());
                (name, wanted, got)
            }
        };
        (wanted != got).then(|| format!("{name}: wanted {wanted}, got {got}"))
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
