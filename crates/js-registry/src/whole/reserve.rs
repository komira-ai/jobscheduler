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
//! The pass reads the table as [`HostFacts`], which the registry builds under its lock, so
//! it holds no clock and no lock of its own.

use std::collections::BTreeMap;
use std::time::Duration;

use js_lease::Timestamp;
use js_types::{HostId, Request, RequestId};

use crate::whole_config::WholeNodeConfig;

use super::caps::{self, Capability, Counts, Limits, MarkKind};
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

    /// Drops the mark of request `id`, if it has one.
    pub(crate) fn drop_request(&mut self, id: &RequestId) {
        self.0.retain(|_, mark| mark.request != *id);
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
/// order, as the module documentation says.
pub(crate) fn advance(
    marks: &mut Marks,
    waiting: &WaitList,
    requests: &mut Requests,
    pass: Pass<'_>,
) {
    marks.0.retain(|host, _| {
        let fact = pass.hosts.iter().find(|fact| fact.id == *host);
        fact.is_some_and(|fact| fact.capability != Capability::Tainted)
    });
    let mut ctx = Ctx {
        marks,
        requests,
        pass,
    };
    ctx.drop_over_ceiling();
    waiting
        .waiting()
        .for_each(|(id, request, since)| ctx.step(id, request, since));
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
        let reserved = self.marks.0.values().map(|mark| caps::Mark {
            request: mark.request.to_string(),
            kind: MarkKind::Reserved,
            at: mark.at.millis(),
        });
        let held = self.pass.hosts.iter().filter_map(|fact| {
            Some(caps::Mark {
                request: fact.id.to_string(),
                kind: MarkKind::Held,
                at: fact.held?.millis(),
            })
        });
        let all: Vec<caps::Mark> = reserved.chain(held).collect();
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

    /// A mark for `id` made now, on a host that waits `wait_ms` to empty.
    fn mark(&self, id: &RequestId, request: &Request, wait_ms: u64, at: Timestamp) -> Mark {
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
        }
    }

    /// Sets a quiet `host` aside for `id`, replacing the mark it had, if any.
    fn assign(&mut self, host: &HostId, id: &RequestId, request: &Request, at: Timestamp) {
        let mark = Mark {
            assigned: true,
            ..self.mark(id, request, 0, at)
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
            self.requests.give_up(RequestKey::from(id), host, end);
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
            fact.map(|fact| (fact.id.clone(), cost.wait_ms))
        });
        if let Some((host, wait_ms)) = chosen {
            let mark = self.mark(id, request, wait_ms, now);
            self.marks.place(host, mark);
        }
    }
}
