//! The start path of the registry: the guards a start meets, and the host facts the
//! whole-node queue's round and the start's caps read.

use std::collections::BTreeSet;

use js_elastic::Demand;
use js_lease::Timestamp;
use js_provider::{Candidate, HostBook};
use js_provider::{Observed, RefusalCode, StartAck, StartSpec};
use js_types::{
    Admission, Capabilities, HostId, LeaseId, RequestId, ResourceClass, Resources, Shape,
};

use super::record::{Bound, Queue};
use super::{HostState, Registry, State};
use crate::lives;
use crate::placer::{ProbeHost, Standing, WholeHost, WholeRound};
use crate::whole::caps::{self, Capability, Counts, Limits};
use crate::whole::exclusive::Exclusive;
use crate::whole::placement::{self, Asking};
use crate::whole::requests::{Exclusion, RequestKey, Rules, whole_settings};
use crate::whole::reserve::{HostFacts, Reservation};
use crate::whole_config::WholeNodeConfig;

/// How the whole-node queue of the placer stands the host of `record` at `now`
/// ([`Standing`]): the first that applies of tainted, leaving, lost, cordoned and live.
fn standing(record: &super::Record, now: Timestamp, grace_ms: u64) -> Standing {
    let live = record.state(now, grace_ms) == HostState::Live;
    match (record.taint.is_some(), record.leaving.is_some(), live) {
        (true, _, _) => Standing::Tainted,
        (false, true, _) => Standing::Leaving,
        (false, false, false) => Standing::Lost,
        (false, false, true) if record.cordoned(now) => Standing::Cordoned,
        (false, false, true) => Standing::Live,
    }
}

/// The hosts that reported a taint: none of them could ever hold a request again.
fn tainted(state: &State) -> BTreeSet<HostId> {
    state
        .records
        .iter()
        .filter(|(_, record)| record.taint.is_some())
        .map(|(host, _)| host.clone())
        .collect()
}

impl Registry {
    /// Whether some host in the table could ever hold a request for `resources` that
    /// requires `required`: the check a whole-node start makes before it answers
    /// `NoCapableHost`, without a request's own bars. A host fits when its booked shape
    /// satisfies both and it has not reported a taint; a busy, lost or reserved host counts,
    /// since it could free. With no host in the table it is `false`.
    pub fn could_ever_hold(&self, resources: &Resources, required: &Capabilities) -> bool {
        let state = self.lock_at(self.shared.clock.now());
        let tainted = tainted(&state);
        let listed = state.book.hosts();
        listed
            .iter()
            .any(|host| placement::could_hold(host, resources, required, &tainted))
    }

    /// Every host as the start's caps and the placer's round read it, in id order.
    pub(super) fn host_facts(&self, state: &State, now: Timestamp) -> Vec<HostFacts> {
        let booked = state.book.leases();
        let grace_ms = self.grace_ms();
        let facts = state.records.iter().map(|(id, record)| {
            let live = record.state(now, grace_ms) == HostState::Live;
            // A host being scaled away is leaving the pool: no reservation drains it.
            let capability = match (record.taint.is_some(), record.leaving.is_some(), live) {
                (true, _, _) => Capability::Tainted,
                (false, true, _) => Capability::Releasing,
                (false, false, true) => Capability::Capable,
                (false, false, false) => Capability::Lost,
            };
            let quiet = record.quiet(&state.runs, id);
            let leases = booked.get(id).map(Vec::as_slice).unwrap_or_default();
            HostFacts {
                id: id.clone(),
                capability,
                open: record.bookable(now, grace_ms, quiet) & leases.is_empty(),
                held: record.exclusive.held_since(),
            }
        });
        facts.collect()
    }

    /// What the registry's books say for one pass of the placer's whole-node queue: the
    /// pool's settings, how it stands each host, whether each is open, and the lives of
    /// the leases. The requests and the hosts held for them are the placer's own, so the
    /// round carries none ([`WholeRound::waiting`], [`WholeRound::outside`]).
    pub(super) fn hosts_round(
        &self,
        state: &State,
        settings: &WholeNodeConfig,
        now: Timestamp,
    ) -> WholeRound {
        let grace = self.grace_ms();
        let facts = self.host_facts(state, now);
        let open: BTreeSet<&HostId> = facts.iter().filter(|f| f.open).map(|f| &f.id).collect();
        let hosts = state.records.iter().map(|(id, record)| WholeHost {
            id: id.clone(),
            standing: standing(record, now, grace),
            open: open.contains(id),
        });
        let source = self.shared.lives.get().map(|lives| &**lives);
        let booked = state.book.leases().into_values().flatten();
        let lives = source.map_or_else(std::collections::BTreeMap::new, |source| {
            let known = booked.filter_map(|lease| {
                let profiles = state.runs.profiles_of(&lease);
                Some((lease, lives::longest_p90(&profiles, source)?))
            });
            known.collect()
        });
        WholeRound {
            settings: settings.clone(),
            waiting: Vec::new(),
            hosts: hosts.collect(),
            outside: crate::MarkedTails::new(),
            lives,
        }
    }

    /// Books `spec`'s lease as [`js_provider::HostBook`] does. A `Whole` class is
    /// committed, not granted: the booking is made only on a host that is bookable (live,
    /// serving, clean), quiet (no held run, none live here) and clear of another
    /// whole-node lease or a restore, and the answer is `Unknown` so the lease stays
    /// pending. A request that used up its tries is answered
    /// `Refused(ExclusiveNotReady)` once (see [`Registry::not_ready`]). One request id holds at most one whole host: a start
    /// of an id that another lease of this pool holds, committed or ready, books nothing
    /// and is answered `NoCapacity` (it waits) until that lease ends. Only a `Whole` class
    /// is held to those rules; a `BookAll` start never reads a request's record. It reads `Present` once the agent reports the host ready
    /// ([`Registry::heartbeat`]), and a repeated start answers `Unknown` until then and
    /// `Started` after.
    pub(crate) fn start(&self, classes: &[ResourceClass], spec: &StartSpec) -> StartAck {
        let now = self.shared.clock.now();
        let grace = self.grace_ms();
        let mut guard = self.lock_at(now);
        let state = &mut *guard;
        let lease = spec.lease();
        let key = RequestKey::of(spec);
        let whole = matches!(spec.class().admission(), Admission::Whole);
        let probe = spec.request().probe_host.as_ref();
        let held = state.book.read(&lease);
        let pending = match &held {
            Observed::Present { host } => state
                .records
                .get(host)
                .is_some_and(|record| record.exclusive.awaits_ready()),
            Observed::Absent | Observed::Unknown => false,
        };
        if pending {
            return StartAck::Unknown;
        }
        let placer = self.placer();
        // A class the pool lists is leasable; one that books part of a host shares it.
        let gate = HostBook::listed_gate(classes, spec);
        // A probe of a whole-node class is not placed: its host would be prepared and
        // held as the queue of whole-node requests decides, which no probe rule judges.
        // The book answers it, so an ended lease, a held one and the class gate come
        // first, as for any start, and it books nothing (`ProbeUnsupported`).
        if whole & probe.is_some() {
            let declined = |_: &[Candidate], _: &dyn Fn() -> Vec<Candidate>| None;
            let ack = state.book.start_over(spec, gate, &|_, _| false, &declined);
            return match ack {
                StartAck::Refused(RefusalCode::NoCapacity | RefusalCode::NoFit) => {
                    StartAck::Refused(RefusalCode::ProbeUnsupported)
                }
                other => other,
            };
        }
        if whole {
            // One request id holds at most one whole host: a start of it under another
            // lease waits until the lease that holds it ends.
            let held_by_other = spec
                .request_id()
                .is_some_and(|id| placer.whole_held_by_other(id, &lease));
            if held_by_other {
                return StartAck::Refused(RefusalCode::NoCapacity);
            }
            // A request that used up its tries is not placed again: the placer keeps its
            // record and says so.
            if placer.whole_start(&key) {
                return StartAck::Refused(RefusalCode::ExclusiveNotReady);
            }
        }
        let shares = !spec.class().admission().books_whole_host();
        let fresh = !matches!(held, Observed::Present { .. });
        // The pool's shape fits the request or no host ever will: the book answers
        // `NoFit` when no listed host fits, and every host is booked as the pool's shape,
        // so that happens for a request the pool's shape fits only when no host is listed
        // yet. A host could still join, which is capacity, not fit. In a pool that places
        // by hello the pool's shape is only a floor: a capability it does not name is the
        // hosts' to meet, so the book answers for those (`PoolConfig::fits`).
        let wanted = spec.request();
        let fits_pool = self.shared.config.fits(wanted);
        // A whole-node request that fits the pool is refused for lack of a host only when
        // no host could ever be given whole; otherwise it waits, if it has an id.
        let listed = state.book.hosts();
        let tainted = tainted(state);
        // The hosts the placer bars the request from, and until when.
        let record = placer.whole_record(&key);
        let kept_off = record.map(|record| record.exclusions).unwrap_or_default();
        let for_life = kept_off
            .iter()
            .filter(|(_, bar)| **bar == Exclusion::ForLife);
        let for_life: BTreeSet<HostId> = for_life.map(|(host, _)| host.clone()).collect();
        let asking = Asking {
            spec,
            hosts: &listed,
            tainted: &tainted,
            for_life: &for_life,
        };
        // A host that could be launched counts as one that could ever serve the request.
        let launch = state
            .launchable
            .as_ref()
            .is_some_and(|launchable| launchable.allows(&Demand::from_request(wanted)));
        let capable = placement::capable(&asking) | launch;
        let queueing = [whole, fresh, fits_pool, gate.is_none()];
        let regain = Rules::of(&self.shared.config).regain_grace_ms;
        let place = spec
            .request_id()
            .filter(|_| queueing.into_iter().all(|ok| ok));
        let verdict = place.map(|id| {
            let grace = std::time::Duration::from_millis(regain);
            placer.whole_present(id, lease, wanted, capable, grace)
        });
        let settings = whole_settings(&self.shared.config);
        self.run_pass(state, &settings, now);
        let hosts = self.host_facts(state, now);
        // The hosts set aside after the pass, read once: the start's gates, backfill and
        // the record of the lease read them as they stand before the booking.
        let set_aside = self.marks_now(state);
        let holder = |host: &HostId| {
            let mark = set_aside.iter().find(|mark| mark.host == *host);
            mark.map(|mark| &mark.request)
        };
        let room = room(&set_aside, &hosts, &settings);
        // How the request waited, read before the booking takes it off the wait list.
        let waiter = spec.request_id().and_then(|id| placer.whole_waiter(id));
        // A probe is judged by the placer, over what is known of its host: the rule is not
        // the registry's. A host it is admitted to is eligible past its cordon.
        let judged = probe.map(|pin| {
            let listed = listed.iter().find(|host| host.host == *pin);
            let record = state.records.get(pin);
            let live = record.filter(|record| record.state(now, grace) == HostState::Live);
            let known = live.zip(listed).map(|(record, listed)| ProbeHost {
                shape: listed.shape.clone(),
                cordoned: record.cordoned(now),
                phase: record.phase,
                taint: record.taint.as_ref().map(|taint| taint.reason.clone()),
                leaving: record.leaving.is_some(),
            });
            placer.admit_probe(spec, pin, known.as_ref())
        });
        let admitted = judged == Some(Ok(()));
        let unjudged = judged.is_none_or(|judged| judged.is_ok());
        // The hosts that carry a lease: a lease that shares one may meet the runs of the
        // leases there, but nothing else (a run the registry does not know, or one of a
        // lease that has ended), whose limits no admission counts.
        let occupied = state.book.leases();
        let State {
            book,
            records,
            runs,
            tail,
            ..
        } = state;
        // A start of a class that shares a host, not a probe, may be placed on a host a
        // whole-node request has set aside, as backfill: the placer is shown the host and
        // the time left until its tail ends, and books it only for work that ends by then.
        let backfills = shares & probe.is_none();
        let marked = match backfills {
            true => tails_left(&set_aside, now),
            false => crate::MarkedTails::new(),
        };
        let eligible = |id: &HostId, _: &Shape| {
            let record = records.get(id);
            // Only a whole-node request is kept off a host its earlier tries failed on.
            let barred = whole & kept_off.get(id).is_some_and(|bar| bar.holds(now));
            let bookable = record.is_some_and(|record| {
                let beside = |leases: &Vec<LeaseId>| record.clear_beside(runs, id, leases);
                let quiet = match (shares, occupied.get(id)) {
                    (true, Some(leases)) => beside(leases),
                    _ => record.quiet(runs, id),
                };
                record.bookable_as(now, grace, quiet, admitted)
            });
            let set_aside = holder(id);
            let allowed = may_start(set_aside, whole, spec.request_id(), room)
                | (backfills & set_aside.is_some());
            [bookable, !barred, allowed, unjudged]
                .into_iter()
                .all(|ok| ok)
        };
        // The placer is shown every host that qualifies, and may read every host listed. It
        // runs under this registry's lock. A start that fits no host by the pool's percent
        // is not shown to the placer: it is `NoFit` (see the answer below).
        let choose = |open: &[Candidate], every: &dyn Fn() -> Vec<Candidate>| {
            fits_pool
                .then(|| placer.pick_over(spec, open, &marked, every))
                .flatten()
        };
        let ack = book.start_over(spec, gate, &eligible, &choose);
        // A probe no host took for the placer's refusal is answered that refusal; any
        // other answer (an ended lease, a held one, the class gate) stands.
        let ack = match (judged, ack) {
            (Some(Err(code)), StartAck::Refused(RefusalCode::NoCapacity | RefusalCode::NoFit)) => {
                StartAck::Refused(code)
            }
            (_, ack) => ack,
        };
        let started = matches!(ack, StartAck::Started { .. });
        let ended = matches!(ack, StartAck::Refused(RefusalCode::LeaseEnded));
        let commit = [whole, fresh].into_iter().all(|ok| ok);
        // When the request was first presented, read before it leaves the wait list.
        let id = spec.request_id();
        let since = waiter.as_ref().map(|waiter| waiter.since);
        // A request that is placed, or whose lease had ended, waits no more.
        if [started & commit, ended].into_iter().any(|leaves| leaves) {
            spec.request_id().into_iter().for_each(|id| {
                placer.whole_cancel(id);
            });
        }
        if let (StartAck::Started { host }, true) = (&ack, fresh) {
            let end = now.plus(wanted.max_wall);
            tail.started(holder(host), spec.request_id());
            let own_mark = id.is_some_and(|id| holder(host) == Some(id));
            let marked = waiter.as_ref().is_some_and(|waiter| waiter.marked);
            let queue = match whole {
                true => Queue::whole(now, since, (own_mark, marked)),
                false => Queue::ORDINARY,
            };
            let here = book.leases().remove(host).unwrap_or_default();
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                record.bounds.retain(|bound| here.contains(&bound.lease));
                record.bounds.push(Bound {
                    lease,
                    end,
                    started_at: now.millis(),
                    queue,
                });
            });
        }
        if let (StartAck::Started { host }, true) = (&ack, commit) {
            // The host was eligible, so it has a record.
            let record = records.get_mut(host);
            record.into_iter().for_each(|record| {
                record.exclusive = Exclusive::Committed { lease, at: now };
            });
            placer.whole_commit(lease, &key);
            return StartAck::Unknown;
        }
        let declined = matches!(
            ack,
            StartAck::Refused(RefusalCode::NoFit | RefusalCode::NoCapacity)
        );
        match (ack, fits_pool, whole && declined) {
            (_, true, true) => placement::answer(verdict, capable),
            (StartAck::Refused(RefusalCode::NoFit), true, false) => {
                StartAck::Refused(RefusalCode::NoCapacity)
            }
            // A host's room is its whole memory when the placer keeps the percent: a
            // request above the share fits a host but never the pool.
            (StartAck::Refused(RefusalCode::NoCapacity), false, _) => {
                StartAck::Refused(RefusalCode::NoFit)
            }
            (other, _, _) => other,
        }
    }
}

/// Whether the caps let one more host be set aside as a quiet host is, with the hosts of
/// `set_aside` and those `hosts` hold whole: an assigned host drains nothing, so it is not
/// held to `max_draining`.
fn room(set_aside: &[Reservation], hosts: &[HostFacts], settings: &WholeNodeConfig) -> bool {
    let held = hosts.iter().filter(|fact| fact.held.is_some()).count();
    let capabilities: Vec<Capability> = hosts.iter().map(|fact| fact.capability).collect();
    let counts = Counts {
        whole: set_aside.len() + held,
        reserved: 0,
        consumer_whole: 0,
    };
    let limits = Limits {
        max_whole_node: settings.max_whole_node,
        capable_hosts: caps::capable_hosts(&capabilities),
        keep_for_ordinary: settings.keep_for_ordinary,
        max_draining: settings.max_draining,
        per_consumer: None,
    };
    caps::may_reserve(counts, limits).is_ok()
}

/// Every host of `set_aside`, with how long from `now` until the end of its tail as the
/// mark took it: zero once that is past, `None` when it is not known (an assigned mark, or
/// a host with a lease that had neither an estimate nor a bound).
fn tails_left(set_aside: &[Reservation], now: Timestamp) -> crate::MarkedTails {
    let left = |end: u64| std::time::Duration::from_millis(end.saturating_sub(now.millis()));
    let tails = set_aside
        .iter()
        .map(|mark| (mark.host.clone(), mark.tail_end.map(left)));
    tails.collect()
}

/// Whether a start may book `host`, as the marks leave it (`holder`: the request it is set
/// aside for). A marked host books only to the request it is marked for, and only to a
/// whole-node start that names it. An unmarked host books to any start, except that a
/// whole-node start with a request id books only the host it was marked, and one without
/// an id needs `room` under the caps.
fn may_start(
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
