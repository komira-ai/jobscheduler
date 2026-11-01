//! The reservation and drain state machine of a dedicated pool (design `whole-node.md`
//! sections 3, 4 and 6): which waiting request holds which host, and when it gives that
//! host up.
//!
//! A [`Mark`] says a host is set aside for one waiting request. A host carries at most one
//! mark and a request holds at most one ([`Marks::place`] keeps both by construction).
//! A marked host takes no other request's start: the registry's start reads the mark, so
//! a newer request cannot take the host of an older one. The mark is not a lease. The
//! pass here never commits one and never stops anything (W7); the request's own next
//! start commits the lease on the host it was given, and drops the mark.
//!
//! [`advance`] walks the wait list in queue order, once per tick:
//!
//! 1. **A quiet host first.** A host that may take a lease right now (live, serving,
//!    clean, nothing on it) and that no mark holds goes to the request at once, ignoring
//!    `drain_after`. The mark is *assigned*: it drains nothing, so it does not count
//!    toward `max_draining`, and it is held for the request's next start.
//! 2. **Otherwise a reservation**, once the request has waited `drain_after`: the host
//!    that costs least to drain ([`super::cost::drain_cost`]) among the live hosts that no
//!    mark and no whole-node lease holds and that are not excluded for the request. The
//!    deadline is [`super::cost::reserve_deadline`], with the larger of the request's
//!    wall time and the host's wait as the wall time.
//! 3. **No thrash.** A reserved request keeps its host, except that it takes another
//!    quiet host if one turns up, and drops the mark it had.
//! 4. **Give-up.** At its deadline, a reservation whose host is still not quiet is given
//!    up: the mark goes, the host is excluded for the request for `reserve_backoff`, the
//!    request's give-up count rises, and the request goes on to the next cheapest host in
//!    the same pass. A reservation made after `max_reserve_attempts` give-ups is *capped*:
//!    its deadline is the wall time and it is never given up, since by then every lease on
//!    the host has ended.
//! 5. **Caps** ([`super::caps::may_reserve`]) apply to every new mark. The single-token
//!    service has no consumer identity, so the per-consumer cap never applies here. When
//!    the ceiling falls, marks beyond it are dropped newest first and a held lease is
//!    never touched.
//!
//! A mark whose request leaves the wait list (cancelled, expired, committed) is dropped by
//! the registry at once ([`Marks::retain_waiting`]). A mark on a host that is tainted or
//! gone is dropped here without counting a give-up.
//!
//! # Restart
//!
//! The marks, with the wait list and the give-up counts, are kept in the lease file as
//! the registry's reservation ledger (`Registry::reservation_ledger`) and put back at a
//! start (`Registry::restore_reservations`). A restored mark keeps its host, its request
//! and the instant it was made; its deadline is what the lease table returns, which
//! raises it to at least the boot plus the boot grace, so a reservation does not fire
//! the moment the server is back; whether it drains or waits for a start is as it was.
//! It is *capped* when the request's give-ups have reached `max_reserve_attempts`, as it
//! was when it was made. Until the request is presented again the pass does not step it
//! (see the wait list), and a restored mark stands while its host has not yet said
//! hello: `claim_by`, the start plus `regain_grace`, ends that wait, after which a mark
//! on a host the registry does not know is dropped and counted. Nothing is persisted
//! about what a host costs to drain: the cost is read from the table each pass.
//!
//! The pass reads the table as [`HostFacts`], which the registry builds under its lock, so
//! it holds no clock and no lock of its own.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease::Timestamp;
use js_types::{HostId, Request, RequestId};

use crate::whole_config::WholeNodeConfig;

use super::caps::{self, Capability, Counts, Limits};
use super::cost::{self, HostLoad};
use super::queue::WaitList;
use super::requests::{RequestKey, Requests};

/// A host set aside for a waiting request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mark {
    request: RequestId,
    at: Timestamp,
    deadline: Timestamp,
    assigned: bool,
    capped: bool,
    /// The end of the host's tail when the mark was made ([`cost::DrainCost::tail_end`]);
    /// `None` for an assigned mark.
    tail_end: Option<u64>,
    /// For a mark put back from the ledger: until when it stands while its host is
    /// unknown to the registry. Cleared when the host is seen.
    claim_by: Option<Timestamp>,
}

/// A mark put back from the ledger ([`Marks::revive`]).
pub(crate) struct Revived {
    pub(crate) request: RequestId,
    pub(crate) host: HostId,
    pub(crate) at: Timestamp,
    pub(crate) deadline: Timestamp,
    pub(crate) assigned: bool,
    /// The request had given up `max_reserve_attempts` hosts or more when the mark was
    /// made.
    pub(crate) capped: bool,
    pub(crate) tail_end: Option<u64>,
    pub(crate) claim_by: Timestamp,
}

/// A host set aside for a request, as an operator sees it
/// ([`crate::Registry::reservations`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// The request the host is set aside for.
    pub request: RequestId,
    /// The host.
    pub host: HostId,
    /// When the host was set aside.
    pub at: Timestamp,
    /// When the reservation is given up if the host is still not quiet. Meaningless for an
    /// `assigned` reservation, which drains nothing.
    pub deadline: Timestamp,
    /// Whether the host was quiet when it was set aside: it drains nothing and waits for
    /// the request's next start.
    pub assigned: bool,
    /// The end of the host's tail when it was set aside, in milliseconds on the registry's
    /// clock: its leases' latest end, each cut to its bound. `None` for an `assigned`
    /// reservation, and for a host with a lease that had neither an estimate nor a bound.
    pub tail_end: Option<u64>,
}

/// The marks of a pool: host to mark.
#[derive(Debug, Default)]
pub(crate) struct Marks(BTreeMap<HostId, Mark>);

impl Marks {
    /// The host and mark of request `id`.
    fn of(&self, id: &RequestId) -> Option<(HostId, Mark)> {
        let found = self.0.iter().find(|(_, mark)| mark.request == *id);
        found.map(|(host, mark)| (host.clone(), mark.clone()))
    }

    /// The host set aside for request `id`.
    pub(crate) fn host_of(&self, id: &RequestId) -> Option<HostId> {
        self.of(id).map(|(host, _)| host)
    }

    /// The request `host` is set aside for.
    pub(crate) fn holder(&self, host: &HostId) -> Option<&RequestId> {
        self.0.get(host).map(|mark| &mark.request)
    }

    /// Sets `host` aside for the request of `mark`. The request's earlier mark goes, so
    /// it holds one; a host that is marked already keeps its mark, so a host carries one.
    fn place(&mut self, host: HostId, mark: Mark) {
        self.drop_request(&mark.request);
        self.0.entry(host).or_insert(mark);
    }

    /// Puts back a mark from the ledger on its host: whether it was, which it is not when
    /// the host carries a mark already. The request's earlier mark goes, so it holds one.
    pub(crate) fn revive(&mut self, mark: Revived) -> bool {
        if self.0.contains_key(&mark.host) {
            return false;
        }
        self.drop_request(&mark.request);
        let revived = Mark {
            request: mark.request,
            at: mark.at,
            deadline: mark.deadline,
            assigned: mark.assigned,
            capped: mark.capped,
            tail_end: mark.tail_end,
            claim_by: Some(mark.claim_by),
        };
        self.0.insert(mark.host, revived);
        true
    }

    /// Drops the marks that cannot stand on the hosts as the pass reads them: a mark on a
    /// tainted host, and one on a host the registry does not know, unless it was put back
    /// from the ledger and its host still has until `claim_by` to say hello. A host that
    /// is known clears the wait. How many put-back marks were dropped for their host
    /// never coming back.
    fn stand(&mut self, pass: &Pass<'_>) -> u64 {
        let mut gone = 0;
        self.0.retain(|host, mark| {
            let fact = pass.hosts.iter().find(|fact| fact.id == *host);
            let Some(fact) = fact else {
                let waiting = mark.claim_by.is_some_and(|by| pass.now < by);
                gone += u64::from(mark.claim_by.is_some() & !waiting);
                return waiting;
            };
            mark.claim_by = None;
            fact.capability != Capability::Tainted
        });
        gone
    }

    /// Drops the mark of request `id`, if it has one.
    pub(crate) fn drop_request(&mut self, id: &RequestId) {
        self.0.retain(|_, mark| mark.request != *id);
    }

    /// Drops the mark on `host`, if it has one: the request that held it is set aside for
    /// no host and is placed again at the next pass.
    pub(crate) fn drop_host(&mut self, host: &HostId) {
        self.0.remove(host);
    }

    /// Drops every mark whose request is not on the wait list.
    pub(crate) fn retain_waiting(&mut self, waiting: &WaitList) {
        self.0.retain(|_, mark| waiting.holds(&mark.request));
    }

    /// The marks, host order.
    pub(crate) fn reservations(&self) -> Vec<Reservation> {
        let marks = self.0.iter();
        let view = marks.map(|(host, mark)| Reservation {
            request: mark.request.clone(),
            host: host.clone(),
            at: mark.at,
            deadline: mark.deadline,
            assigned: mark.assigned,
            tail_end: mark.tail_end,
        });
        view.collect()
    }

    /// Whether no host is set aside.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Marks that drain something: assigned marks do not.
    fn draining(&self) -> usize {
        self.0.values().filter(|mark| !mark.assigned).count()
    }
}

/// One host as the pass reads it.
#[derive(Debug, Clone)]
pub(crate) struct HostFacts {
    pub(crate) id: HostId,
    /// Capable, lost (counted, since it may come back) or tainted (never eligible).
    pub(crate) capability: Capability,
    /// Whether the host may take a new lease right now: live, serving, clean, quiet and
    /// holding no lease.
    pub(crate) open: bool,
    /// Whether a whole-node lease is committed on the host, and since when.
    pub(crate) held: Option<Timestamp>,
    /// Leases on the host and the end bound of each (start plus max wall time), in
    /// milliseconds of the registry's clock.
    pub(crate) load: HostLoad,
}

/// What a pass reads besides the host table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pass<'a> {
    pub(crate) now: Timestamp,
    pub(crate) settings: &'a WholeNodeConfig,
    pub(crate) hosts: &'a [HostFacts],
}

/// Runs one pass: drops the marks that cannot stand, then walks the wait list in queue
/// order, as the module documentation says. How many put-back marks were dropped because
/// their host never said hello.
pub(crate) fn advance(
    marks: &mut Marks,
    waiting: &WaitList,
    requests: &mut Requests,
    pass: Pass<'_>,
) -> u64 {
    let gone = marks.stand(&pass);
    let mut ctx = Ctx {
        marks,
        requests,
        pass,
    };
    ctx.drop_over_ceiling();
    waiting
        .waiting()
        .for_each(|(id, request, since)| ctx.step(id, request, since));
    gone
}

fn capable_hosts(pass: &Pass<'_>) -> usize {
    let capabilities: Vec<Capability> = pass.hosts.iter().map(|fact| fact.capability).collect();
    caps::capable_hosts(&capabilities)
}

/// Hosts whole: marked, or holding a whole-node lease.
fn whole(marks: &Marks, pass: &Pass<'_>) -> usize {
    let held = pass.hosts.iter().filter(|fact| fact.held.is_some());
    marks.0.len() + held.count()
}

/// Whether the caps let one more host be set aside. An assigned host drains nothing, so
/// it is not held to `max_draining`.
pub(crate) fn has_room(marks: &Marks, pass: &Pass<'_>, assigning: bool) -> bool {
    let settings = pass.settings;
    let draining = if assigning { 0 } else { marks.draining() };
    let counts = Counts {
        whole: whole(marks, pass),
        reserved: draining,
        consumer_whole: 0,
    };
    let limits = Limits {
        max_whole_node: settings.max_whole_node,
        capable_hosts: capable_hosts(pass),
        keep_for_ordinary: settings.keep_for_ordinary,
        max_draining: settings.max_draining,
        per_consumer: None,
    };
    caps::may_reserve(counts, limits).is_ok()
}

/// Whether a start may book `host`, as the marks leave it. A marked host books only to
/// the request it is marked for, and only to a whole-node start that names it. An unmarked
/// host books to any start, except that a whole-node start with a request id books only
/// the host it was marked, and one without an id needs `room` under the caps.
pub(crate) fn may_start(
    holder: Option<&RequestId>,
    whole: bool,
    request: Option<&RequestId>,
    room: bool,
) -> bool {
    match (holder, whole, request) {
        (Some(holder), true, Some(request)) => holder == request,
        (Some(_), _, _) | (None, true, Some(_)) => false,
        (None, true, None) => room,
        (None, false, _) => true,
    }
}

struct Ctx<'a> {
    marks: &'a mut Marks,
    requests: &'a mut Requests,
    pass: Pass<'a>,
}

impl Ctx<'_> {
    /// Drops the newest marks beyond the effective ceiling; a held lease stays.
    fn drop_over_ceiling(&mut self) {
        let settings = self.pass.settings;
        let ceiling = caps::ceiling(
            settings.max_whole_node,
            capable_hosts(&self.pass),
            settings.keep_for_ordinary,
        );
        let reserved = self.marks.0.values().map(|mark| caps::Mark::Reserved {
            request: mark.request.to_string(),
            at: mark.at.millis(),
        });
        let held = self.pass.hosts.iter().filter_map(|fact| {
            Some(caps::Mark::Held {
                host: fact.id.to_string(),
                at: fact.held?.millis(),
            })
        });
        let all: Vec<caps::Mark> = reserved.chain(held).collect();
        // Only reservations come back, so the ids are request ids.
        let excess = caps::ceiling_excess(&all, ceiling);
        self.marks
            .0
            .retain(|_, mark| !excess.contains(&mark.request.to_string()));
    }

    fn may_mark(&self, assigning: bool) -> bool {
        has_room(self.marks, &self.pass, assigning)
    }

    /// The hosts the request under `key` could be set aside: no mark holds them and they
    /// are not excluded for it. Every host is booked as the pool's shape and a request
    /// waits only if the pool's shape fits it, so every host fits.
    fn candidates<'s>(&'s self, key: &'s RequestKey) -> impl Iterator<Item = &'s HostFacts> {
        self.pass.hosts.iter().filter(move |fact| {
            let free = self.marks.holder(&fact.id).is_none();
            let barred = self.requests.excluded(key, &fact.id, self.pass.now);
            free & !barred
        })
    }

    /// A mark for `id` made now, on a host that waits `wait_ms` to empty and whose tail
    /// ends at `tail_end`.
    fn mark(
        &self,
        id: &RequestId,
        request: &Request,
        (wait_ms, tail_end): (u64, Option<u64>),
        at: Timestamp,
    ) -> Mark {
        let settings = self.pass.settings;
        let key = RequestKey::from(id);
        let given_up = self.requests.give_ups(&key);
        let wall = u64::try_from(request.max_wall.as_millis()).unwrap_or(u64::MAX);
        let wall = wall.max(wait_ms);
        let deadline = cost::reserve_deadline(given_up, settings, wall);
        Mark {
            request: id.clone(),
            at,
            deadline: self.pass.now.plus(Duration::from_millis(deadline)),
            assigned: false,
            capped: given_up >= settings.max_reserve_attempts,
            tail_end,
            claim_by: None,
        }
    }

    /// Sets a quiet `host` aside for `id`, replacing the mark it had, if any.
    fn assign(&mut self, host: &HostId, id: &RequestId, request: &Request, at: Timestamp) {
        let mark = Mark {
            assigned: true,
            ..self.mark(id, request, (0, None), at)
        };
        self.marks.place(host.clone(), mark);
    }

    /// The first quiet host by id that `id` could be given.
    fn quiet_host(&self, id: &RequestId) -> Option<HostId> {
        let key = RequestKey::from(id);
        let mut open = self.candidates(&key).filter(|fact| fact.open);
        open.next().map(|fact| fact.id.clone())
    }

    fn step(&mut self, id: &RequestId, request: &Request, since: Timestamp) {
        let held = self.marks.of(id);
        let kept = held.is_some_and(|(host, mark)| self.keep_or_move(id, request, &host, &mark));
        if !kept {
            self.place(id, request, since);
        }
    }

    /// A request that holds `host` by `mark`: whether it still does after this pass. It
    /// keeps a quiet host; it moves to another quiet host if one turns up; it gives the
    /// host up at the deadline unless the reservation is capped.
    fn keep_or_move(
        &mut self,
        id: &RequestId,
        request: &Request,
        host: &HostId,
        mark: &Mark,
    ) -> bool {
        let fact = self.pass.hosts.iter().find(|fact| fact.id == *host);
        if fact.is_some_and(|fact| fact.open) {
            return true;
        }
        if let Some(quiet) = self.quiet_host(id) {
            self.assign(&quiet, id, request, mark.at);
            return true;
        }
        let due = [self.pass.now >= mark.deadline, !mark.capped]
            .into_iter()
            .all(|ok| ok);
        if due {
            let backoff = Duration::from_millis(self.pass.settings.reserve_backoff_ms);
            let end = self.pass.now.plus(backoff);
            self.requests
                .give_up(RequestKey::from(id), host, (self.pass.now, end));
            self.marks.drop_request(id);
        }
        !due
    }

    /// Sets a host aside for `id`, which holds none: a quiet one at once, else after
    /// `drain_after` the one that costs least to drain.
    fn place(&mut self, id: &RequestId, request: &Request, since: Timestamp) {
        let now = self.pass.now;
        if let Some(quiet) = self.quiet_host(id) {
            if self.may_mark(true) {
                self.assign(&quiet, id, request, now);
            }
            return;
        }
        let waited = now.millis().saturating_sub(since.millis());
        let blocked = [
            waited < self.pass.settings.drain_after_ms,
            !self.may_mark(false),
        ];
        if blocked.into_iter().any(|no| no) {
            return;
        }
        let key = RequestKey::from(id);
        let live: Vec<&HostFacts> = self
            .candidates(&key)
            .filter(|fact| fact.capability == Capability::Capable && fact.held.is_none())
            .collect();
        let loads: Vec<HostLoad> = live.iter().map(|fact| fact.load.clone()).collect();
        let cheapest = cost::drain_cost(&loads, now.millis()).into_iter().next();
        let chosen = cheapest.and_then(|cost| {
            let fact = live.iter().find(|fact| fact.id.as_str() == cost.id);
            fact.map(|fact| (fact.id.clone(), (cost.wait_ms, cost.tail_end)))
        });
        if let Some((host, tail)) = chosen {
            let mark = self.mark(id, request, tail, now);
            self.marks.place(host, mark);
        }
    }
}
