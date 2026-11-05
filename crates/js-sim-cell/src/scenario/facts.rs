//! What the server holds at the moment an expectation is judged, read then and not from
//! the observations: the lease list as a client reads it, where the agent registry books
//! a lease, a whole-node request's record and its place on the wait list, the reservations,
//! and how the listing shows a host set aside. These are the facts the server's own HTTP
//! tests read ([`super::Expect::listed`] and the six after it), so a test moved onto the
//! cell reads the same ones.
//!
//! A lease is named by the client call whose answer named it ([`Call`]). The list is read
//! through the client router as the front reads it ([`crate::front`]); the registry
//! through the server's own handle ([`ServerNode::registry`]). A server that is down
//! answers nothing: the list reads `unread`, and the registry holds nothing.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, Response};
use js_registry::{Exclusion, Registry, RequestKey, RequestRecord};
use js_sim::text::list;
use js_types::{HostId, RequestId};

use crate::agent::lease_id;
use crate::bus::agent_name;
use crate::cell::Cell;
use crate::front::{self, ClientPort};
use crate::server::ServerNode;

use super::answer::{Call, lease_of};

/// What the list says when it could not be read.
const UNREAD: &str = "unread";

/// How the lease list shows a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed {
    /// Listed `pending`: committed or waiting, and not yet held.
    Pending,
    /// Listed `held`.
    Held,
    /// Not listed.
    Absent,
}

impl Listed {
    /// A stable kebab-case name: the state the list spells, or `absent`.
    #[must_use]
    pub fn name(self) -> &'static str {
        ["pending", "held", "absent"][self as usize]
    }
}

/// A host an expectation names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRef {
    /// The host of agent `index` (`h000` is 0).
    Agent(usize),
    /// The first host the server's listing showed the lease the answer to the call named
    /// booked on ([`Cell::first_booked`]), wherever it is booked now.
    BookedFor(Call),
}

/// Where the agent registry books a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Booked {
    /// On no host.
    Nowhere,
    /// On some host.
    Somewhere,
    /// On this host.
    On(HostRef),
    /// On some host, not this one.
    Elsewhere(HostRef),
}

/// One fact of the agent registry's record of a whole-node request
/// ([`Registry::request_record`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordFact {
    /// The registry holds no record of it.
    Absent,
    /// Its prepares that failed.
    PrepareFailures(u32),
    /// Whether it has used up `max_replace`.
    Terminal(bool),
    /// Exactly these hosts are left out of its placements, each until when.
    Exclusions(Vec<(HostRef, Exclusion)>),
    /// The code of its last failed prepare (`prepare-timeout` for a deadline that passed);
    /// a record with no failed prepare reads the empty code.
    LastFailure(String),
}

impl RecordFact {
    fn name(&self) -> &'static str {
        let index = match self {
            RecordFact::Absent => 0,
            RecordFact::PrepareFailures(_) => 1,
            RecordFact::Terminal(_) => 2,
            RecordFact::Exclusions(_) => 3,
            RecordFact::LastFailure(_) => 4,
        };
        [
            "absent",
            "prepare-failures",
            "terminal",
            "exclusions",
            "last-failure",
        ][index]
    }
}

/// One fact of the server's, read when it is judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fact {
    Listed(Call, Listed),
    NoLeasesListed,
    Booked(Call, Booked),
    Record(RequestKey, RecordFact),
    Waiting(String, Option<usize>),
    NoReservations,
    SetAside(usize, Option<String>),
}

/// The statement, what it wanted and what it got, of `fact` over the server of `cell`
/// now.
pub(super) fn judged(fact: &Fact, cell: &mut Cell) -> [String; 3] {
    match fact {
        Fact::Listed(call, wanted) => {
            let lease = lease_of(call, cell.answers());
            let states = read_list(cell);
            let got = match (lease, states) {
                (None, _) => "no-lease".to_owned(),
                (Some(_), None) => UNREAD.to_owned(),
                (Some(id), Some(states)) => {
                    states.get(&id).map_or("absent", String::as_str).to_owned()
                }
            };
            let name = format!("listed({})", call.name());
            [name, wanted.name().to_owned(), got]
        }
        Fact::NoLeasesListed => {
            let ids = read_list(cell).map(|states| states.into_keys().collect::<Vec<String>>());
            let got = ids.map_or_else(|| UNREAD.to_owned(), |ids| list(&ids, String::clone));
            [
                "no-leases-listed".to_owned(),
                list::<String>(&[], String::clone),
                got,
            ]
        }
        Fact::Booked(call, wanted) => booked(call, *wanted, cell),
        Fact::Record(key, wanted) => record(key, wanted, cell),
        Fact::Waiting(request, wanted) => {
            let id = RequestId::new(request.as_str()).ok();
            let registry = registry(cell);
            let got = registry.zip(id).and_then(|(r, id)| r.queue_position(&id));
            [format!("waiting({request})"), place(*wanted), place(got)]
        }
        Fact::NoReservations => {
            let held = registry(cell).map(|r| r.reservations()).unwrap_or_default();
            let got = list(&held, |r| format!("{} for {}", r.host, r.request));
            [
                "no-reservations".to_owned(),
                list::<String>(&[], String::clone),
                got,
            ]
        }
        Fact::SetAside(index, wanted) => {
            let host = HostId::new(agent_name(*index).as_str()).ok();
            let view = cell.server().map(ServerNode::view).unwrap_or_default();
            let listed = host.and_then(|host| view.hosts.get(&host).cloned());
            let got = listed.map_or_else(
                || "unlisted".to_owned(),
                |listed| aside(listed.set_aside, listed.reserved_for.as_deref()),
            );
            let name = format!("set-aside(h{index:03})");
            [name, aside(wanted.is_some(), wanted.as_deref()), got]
        }
    }
}

/// [`Fact::Booked`]: where the registry books the lease the answer to `call` named, and
/// whether that is `wanted`; when it is, what it got reads as what it wanted.
fn booked(call: &Call, wanted: Booked, cell: &Cell) -> [String; 3] {
    let lease = lease_of(call, cell.answers()).and_then(|id| lease_id(&id));
    let registry = registry(cell);
    let host = lease.zip(registry).and_then(|(lease, r)| r.host_of(&lease));
    let (wanted, holds) = match wanted {
        Booked::Nowhere => ("nowhere".to_owned(), host.is_none()),
        Booked::Somewhere => ("somewhere".to_owned(), host.is_some()),
        Booked::On(named) => {
            let named = resolve(named, cell);
            let holds = host.is_some() & (host == named);
            (format!("on {}", shown(named.as_ref())), holds)
        }
        Booked::Elsewhere(named) => {
            let named = resolve(named, cell);
            let holds = host.is_some() & named.is_some() & (host != named);
            (format!("not on {}", shown(named.as_ref())), holds)
        }
    };
    let got = if holds {
        wanted.clone()
    } else {
        shown(host.as_ref())
    };
    [format!("booked({})", call.name()), wanted, got]
}

/// [`Fact::Record`]: one field of the registry's record of the request `key` names.
fn record(key: &RequestKey, wanted: &RecordFact, cell: &Cell) -> [String; 3] {
    let found = registry(cell).and_then(|r| r.request_record(key.clone()));
    let field = |read: &dyn Fn(&RequestRecord) -> String| {
        found.as_ref().map_or_else(|| "absent".to_owned(), read)
    };
    let (want, got) = match wanted {
        RecordFact::Absent => ("absent".to_owned(), field(&|_| "present".to_owned())),
        RecordFact::PrepareFailures(count) => {
            let got = field(&|record| record.prepare_failures.to_string());
            (count.to_string(), got)
        }
        RecordFact::Terminal(terminal) => (
            terminal.to_string(),
            field(&|record| record.terminal.to_string()),
        ),
        RecordFact::Exclusions(hosts) => {
            let named = hosts.iter().map(|(host, until)| {
                let host = resolve(*host, cell);
                (shown(host.as_ref()), *until)
            });
            let got = field(&|record| {
                let held = record.exclusions.iter();
                excluded(
                    held.map(|(host, until)| (host.to_string(), *until))
                        .collect(),
                )
            });
            (excluded(named.collect()), got)
        }
        RecordFact::LastFailure(code) => {
            let got = field(&|record| {
                record
                    .last
                    .as_ref()
                    .map(|l| l.code.clone())
                    .unwrap_or_default()
            });
            (code.clone(), got)
        }
    };
    let name = format!("request-record({}).{}", key_name(key), wanted.name());
    [name, want, got]
}

/// The lease list as the client reads it now, through the cell's client router.
fn read_list(cell: &mut Cell) -> Option<BTreeMap<String, String>> {
    front::listed(&mut Through(cell))
}

/// The client router of a cell, as [`front::listed`] calls it: through [`Cell::client_call`],
/// so the call is written into the trace like any other.
struct Through<'a>(&'a mut Cell);

impl ClientPort for Through<'_> {
    fn call(&mut self, request: Request<Body>) -> Response<String> {
        self.0.client_call(request).unwrap_or_default()
    }
}

/// The server's agent registry; `None` while the server is down.
fn registry(cell: &Cell) -> Option<Registry> {
    cell.server().and_then(ServerNode::registry)
}

/// The host `named` is, now; `None` for an agent name that is not a host id, and for a
/// lease never seen booked.
fn resolve(named: HostRef, cell: &Cell) -> Option<HostId> {
    match named {
        HostRef::Agent(index) => HostId::new(agent_name(index).as_str()).ok(),
        HostRef::BookedFor(call) => {
            let lease = lease_of(&call, cell.answers()).and_then(|id| lease_id(&id));
            lease.and_then(|lease| cell.first_booked(&lease).cloned())
        }
    }
}

/// A host, or `nowhere`.
fn shown(host: Option<&HostId>) -> String {
    host.map_or_else(|| "nowhere".to_owned(), ToString::to_string)
}

/// A place on the wait list, or `none`.
fn place(at: Option<usize>) -> String {
    at.map_or_else(|| "none".to_owned(), |at| at.to_string())
}

/// Hosts left out, each with until when, in host order.
fn excluded(hosts: BTreeMap<String, Exclusion>) -> String {
    let hosts: Vec<(String, Exclusion)> = hosts.into_iter().collect();
    list(&hosts, |(host, until)| match until {
        Exclusion::Until(end) => format!("{host} until {}", end.millis()),
        Exclusion::ForLife => format!("{host} for life"),
    })
}

/// How the listing shows a host set aside, and for which request.
fn aside(set: bool, request: Option<&str>) -> String {
    format!("aside={set} for={}", request.unwrap_or("none"))
}

/// A request key as a statement names it: the request id, or `content`.
fn key_name(key: &RequestKey) -> String {
    match key {
        RequestKey::Id(id) => id.to_string(),
        RequestKey::Content(_) => "content".to_owned(),
    }
}
