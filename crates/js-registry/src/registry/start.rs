//! The start path of the registry: the guards a start meets, the pass that places waiting
//! whole-node requests, and the host facts that pass reads.

use std::collections::{BTreeMap, BTreeSet};

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
use crate::placer::{ProbeHost, Standing, WaitingWhole, WholeHost, WholeRound};
use crate::whole::caps::Capability;
use crate::whole::cost::HostLoad;
use crate::whole::exclusive::Exclusive;
use crate::whole::placement::{self, Asking};
use crate::whole::requests::{RequestKey, Rules, whole_settings};
use crate::whole::reserve::{self, HostFacts, Pass};
use crate::whole_config::WholeNodeConfig;

/// The latest instant a [`Timestamp`] holds: where the end of a lease with an endless max
/// wall saturates.
const ENDLESS: Timestamp = Timestamp::MAX;

/// The bound [`js_whole::cost::tail`] reads as none.
const NO_BOUND: u64 = u64::MAX;

/// How the whole-node queue of a placer stands the host of `record` at `now`
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

    /// Every host as the reservation pass reads it, in id order. Every host is booked as
    /// the pool's shape.
    pub(super) fn host_facts(&self, state: &State, now: Timestamp) -> Vec<HostFacts> {
        let booked = state.book.leases();
        let grace_ms = self.grace_ms();
        let source = self.shared.lives.get().map(|lives| &**lives);
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
            // Every lease booked here that the registry started has its own bound; a lease
            // put back after a restart has none.
            let known: Vec<&Bound> = record
                .bounds
                .iter()
                .filter(|bound| leases.contains(&bound.lease))
                .collect();
            let started: Vec<(LeaseId, u64)> = known
                .iter()
                .map(|bound| (bound.lease, bound.started_at))
                .collect();
            // With the estimate feed on, a bound at the end of time (a max wall so long that
            // the end saturates) is no bound: the lease's end is its estimate, or unknown.
            // Off, it is kept as the far bound it always was.
            let end_bound = |bound: &&Bound| match (source.is_some(), bound.end == ENDLESS) {
                (true, true) => NO_BOUND,
                _ => bound.end.millis(),
            };
            HostFacts {
                id: id.clone(),
                capability,
                open: record.bookable(now, grace_ms, quiet) & leases.is_empty(),
                held: record.exclusive.held_since(),
                load: HostLoad {
                    id: id.to_string(),
                    leases: leases.len(),
                    end_bounds: known.iter().map(end_bound).collect(),
                    est_ends: lives::est_ends(source, &state.runs, &started),
                },
            }
        });
        facts.collect()
    }

    /// Sets hosts aside for the waiting requests, in queue order. A placer that keeps the
    /// whole-node queue ([`crate::Placer::whole_round`], the engine of `--placement
    /// js-sched`) runs the pass and the registry takes its marks; the registry's own pass
    /// ([`crate::whole::reserve`]) is then not run, and stays the pass of every other
    /// placement. Gives up and commits nothing else, and stops nothing.
    pub(super) fn place_waiting(
        &self,
        state: &mut State,
        settings: &WholeNodeConfig,
        hosts: &[HostFacts],
        now: Timestamp,
    ) {
        let pass = Pass {
            now,
            settings,
            hosts,
        };
        let placer = self.shared.placer.get();
        let round = placer.map(|placer| (placer, self.round_of(state, settings, now)));
        let placed = round.and_then(|(placer, round)| Some((placer.whole_round(&round)?, round)));
        if let Some((marks, round)) = placed {
            let presented = round.waiting.into_iter().map(|waiting| waiting.id);
            let presented: BTreeSet<RequestId> = presented.collect();
            // A mark put back from the ledger whose host never came back, or is tainted,
            // goes as it does in the registry's own pass.
            state.dropped += state.marks.stand(&pass);
            state.marks.adopt(marks, &presented);
            return;
        }
        let State {
            marks,
            waiting,
            requests,
            dropped,
            ..
        } = state;
        *dropped += reserve::advance(marks, waiting, requests, pass);
    }

    /// What a placer that keeps the whole-node queue reads of the registry's books for one
    /// pass ([`WholeRound`]).
    fn round_of(&self, state: &State, settings: &WholeNodeConfig, now: Timestamp) -> WholeRound {
        let grace = self.grace_ms();
        let waiting = state.waiting.waiting().map(|(id, request, since)| {
            let key = RequestKey::from(id);
            let record = state.requests.record(&key);
            WaitingWhole {
                id: id.clone(),
                max_wall: request.max_wall,
                since,
                give_ups: state.requests.give_ups(&key),
                exclusions: record.map(|r| r.exclusions.clone()).unwrap_or_default(),
                mark: state.marks.reservation_of(id),
            }
        });
        let waiting: Vec<WaitingWhole> = waiting.collect();
        let presented: BTreeSet<RequestId> = waiting.iter().map(|w| w.id.clone()).collect();
        let hosts = state.records.iter().map(|(id, record)| WholeHost {
            id: id.clone(),
            standing: standing(record, now, grace),
        });
        let source = self.shared.lives.get().map(|lives| &**lives);
        let booked = state.book.leases().into_values().flatten();
        let lives = source.map_or_else(BTreeMap::new, |source| {
            let known = booked.filter_map(|lease| {
                let profiles = state.runs.profiles_of(&lease);
                Some((lease, lives::longest_p90(&profiles, source)?))
            });
            known.collect()
        });
        WholeRound {
            settings: settings.clone(),
            waiting,
            hosts: hosts.collect(),
            outside: state.marks.outside(&presented, now),
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
        // A probe of a whole-node class is not placed: its host would be prepared and
        // held as the queue of whole-node requests decides, which no probe rule judges.
        // The book answers it, so an ended lease, a held one and the class gate come
        // first, as for any start, and it books nothing (`ProbeUnsupported`).
        if whole & probe.is_some() {
            let gate = match self.shared.placer.get() {
                Some(_) => HostBook::listed_gate(classes, spec),
                None => HostBook::class_gate(classes, spec),
            };
            return state.book.start_where(spec, gate, &|_, _| false);
        }
        if whole {
            // One request id holds at most one whole host: a start of it under another
            // lease waits until the lease that holds it ends.
            let id_held = spec.request_id().is_some();
            if id_held & state.requests.held_by_other(&key, &lease) {
                return StartAck::Refused(RefusalCode::NoCapacity);
            }
            // A request that used up its tries is not placed again.
            if state.requests.start(&key, now) {
                return StartAck::Refused(RefusalCode::ExclusiveNotReady);
            }
        }
        // With a placer, a class that books part of a host is leasable: leases share.
        let placer = self.shared.placer.get();
        let gate = match placer {
            Some(_) => HostBook::listed_gate(classes, spec),
            None => HostBook::class_gate(classes, spec),
        };
        let shares = placer.is_some() & !spec.class().admission().books_whole_host();
        let fresh = !matches!(held, Observed::Present { .. });
        // The pool's shape fits the request or no host ever will: the book answers
        // `NoFit` when no listed host fits, and every host is booked as the pool's shape,
        // so that happens for a request the pool's shape fits only when no host is listed
        // yet. A host could still join, which is capacity, not fit.
        let floor = self.shared.config.fit();
        let wanted = spec.request();
        let fits_pool = floor.satisfies(&wanted.resources, &wanted.capabilities);
        // A whole-node request that fits the pool is refused for lack of a host only when
        // no host could ever be given whole; otherwise it waits, if it has an id.
        let listed = state.book.hosts();
        let tainted = tainted(state);
        let asking = Asking {
            spec,
            key: &key,
            hosts: &listed,
            tainted: &tainted,
        };
        // A host that could be launched counts as one that could ever serve the request.
        let launch = state
            .launchable
            .as_ref()
            .is_some_and(|launchable| launchable.allows(&Demand::from_request(wanted)));
        let capable = placement::capable(&asking, &state.requests) | launch;
        let queueing = [whole, fresh, fits_pool, gate.is_none()];
        let regain = Rules::of(&self.shared.config).regain_grace_ms;
        let place = spec
            .request_id()
            .filter(|_| queueing.into_iter().all(|ok| ok));
        let verdict = place.map(|id| {
            let waiting = &mut state.waiting;
            waiting.present(id, lease, wanted, capable, now, regain)
        });
        let settings = whole_settings(&self.shared.config);
        let hosts = self.host_facts(state, now);
        self.place_waiting(state, &settings, &hosts, now);
        let pass = Pass {
            now,
            settings: &settings,
            hosts: &hosts,
        };
        let room = reserve::has_room(&state.marks, &pass, true);
        // A probe is judged by the placer, over what is known of its host: the rule is not
        // the registry's. A host it is admitted to is eligible past its cordon.
        let judged = probe.zip(placer).map(|(pin, placer)| {
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
            requests,
            waiting,
            marks,
            tail,
            ..
        } = state;
        // A start of a class that shares a host, not a probe, may be placed on a host a
        // whole-node request has set aside, as backfill: the placer is shown the host and
        // the time left until its tail ends, and books it only for work that ends by then.
        let backfills = shares & probe.is_none();
        let marked = match backfills {
            true => marks.tails_left(now),
            false => crate::MarkedTails::new(),
        };
        let eligible = |id: &HostId, _: &Shape| {
            let record = records.get(id);
            // Only a whole-node request is kept off a host its earlier tries failed on.
            let barred = whole & requests.excluded(&key, id, now);
            let bookable = record.is_some_and(|record| {
                let beside = |leases: &Vec<LeaseId>| record.clear_beside(runs, id, leases);
                let quiet = match (shares, occupied.get(id)) {
                    (true, Some(leases)) => beside(leases),
                    _ => record.quiet(runs, id),
                };
                record.bookable_as(now, grace, quiet, admitted)
            });
            let set_aside = marks.holder(id);
            let allowed = reserve::may_start(set_aside, whole, spec.request_id(), room)
                | (backfills & set_aside.is_some());
            [bookable, !barred, allowed, unjudged]
                .into_iter()
                .all(|ok| ok)
        };
        // Without a placer the book books the first host that qualifies, stopping at it;
        // with one it is shown every host that qualifies, and may read every host listed.
        // The placer runs under this registry's lock.
        let ack = match placer {
            Some(placer) => {
                // A start that fits no host by the pool's percent is not shown to the
                // placer: it is `NoFit` (see the answer below).
                let choose = |open: &[Candidate], every: &dyn Fn() -> Vec<Candidate>| {
                    fits_pool
                        .then(|| placer.pick_over(spec, open, &marked, every))
                        .flatten()
                };
                book.start_over(spec, gate, &eligible, &choose)
            }
            None => book.start_where(spec, gate, &eligible),
        };
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
        let since = id.and_then(|id| waiting.queue().find(|(queued, _)| *queued == id));
        let since = since.map(|(_, since)| since);
        // A request that is placed, or whose lease had ended, waits no more.
        if [started & commit, ended].into_iter().any(|leaves| leaves) {
            spec.request_id().into_iter().for_each(|id| {
                waiting.cancel(id);
            });
        }
        if let (StartAck::Started { host }, true) = (&ack, fresh) {
            let end = now.plus(wanted.max_wall);
            tail.started(marks.holder(host), spec.request_id());
            let own_mark = id.is_some_and(|id| marks.holder(host) == Some(id));
            let marked = id.is_some_and(|id| marks.ever_marked(id));
            let queue = match whole {
                true => Queue::whole(now, since, (own_mark, marked), requests.give_ups(&key)),
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
            requests.commit(lease, key);
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
