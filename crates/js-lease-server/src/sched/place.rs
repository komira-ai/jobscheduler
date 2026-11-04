//! The registry's placer, with `js-sched` choosing the host.
//!
//! The registry shows [`SchedPlacer`] the hosts a start could book, each with the leases
//! already on it. The placer builds a [`Scheduler`] over exactly those: every host a worker
//! of its node report, every lease a recovered lease on its worker, and the start a
//! submitted request. One placement round later the scheduler has named a worker, and that
//! is the host. The scheduler is built for the one start and dropped, so the registry stays
//! the only place that books: it checks the room and makes the booking, as it does without
//! a placer, and the scheduler only picks among the hosts that have room.
//!
//! How it picks depends on the lease's class:
//!
//! - A class that books part of a host: by the scheduler's score, the host the lease fits
//!   most tightly (so the most booked first), and equal hosts by a hash of the lease and the
//!   host, so two leases can break the same tie differently.
//! - A class that books the whole host: no score. The registry offers such a lease only
//!   hosts that carry no lease, and the scheduler gives a worker that a request would own
//!   whole no score and takes the first by worker name, which is the host id here
//!   ([`map::worker`]). So the lease takes the first open host by id, whatever its number
//!   or the order the hosts are fed in: the host a registry with no placer books.
//!
//! A host a whole-node request has set aside (a mark of the registry) is shown only to a
//! start that may backfill it (a class that books part of a host, not a probe), with the
//! time left until its tail ends as the mark took it. The scheduler holds that host for the
//! request outside it ([`Scheduler::reserve_outside`]) and places the start there only if
//! the start's max wall time (which the lease table enforces) ends, with a minute to spare,
//! by then ([`Scheduler::with_backfill`]). A start it places elsewhere or nowhere never
//! falls back to a marked host.
//!
//! Cost: the scheduler is rebuilt for every start over all the open hosts and their leases,
//! under the registry's lock, so a start is linear in hosts plus leases. A cache would have
//! to follow every booking and stop; none is kept.

use std::sync::atomic::{AtomicU64, Ordering};

use js_provider::Candidate;
use js_provider::{RefusalCode, StartSpec};
use js_registry::{MarkedTails, Placer, ProbeHost};
use js_sched::harness::feed;
use js_sched::{Backfill, DaemonInstance, Event, Recovered, RecoveredLease, Request, Scheduler};
use js_types::HostId;
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};

use tracing::warn;

use super::map::{self, MapError};

/// The term the scheduler is built for. It is one start long, so the number only has to
/// differ from the leases it recovers, which are numbered in term 0.
const TERM: u64 = 1;

/// Places the starts of a registry with `js-sched`, and counts the starts it could not.
/// The default backfills no host a whole-node request has set aside.
#[derive(Debug, Default)]
pub(crate) struct SchedPlacer {
    /// Whether a start may backfill a host set aside, and up to which wall time.
    backfill: Backfill,
    unmappable: AtomicU64,
    declined: AtomicU64,
}

/// How many starts fell back to the registry's first host, by cause.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fallbacks {
    /// The scheduler could not be built over the hosts or the request.
    pub(crate) unmappable: u64,
    /// The scheduler was built and placed the start nowhere.
    pub(crate) declined: u64,
}

impl SchedPlacer {
    /// A placer that backfills a host set aside as `backfill` allows.
    pub(crate) fn new(backfill: Backfill) -> Self {
        Self {
            backfill,
            ..Self::default()
        }
    }

    /// The fallbacks so far.
    #[cfg(test)]
    pub(crate) fn fallbacks(&self) -> Fallbacks {
        Fallbacks {
            unmappable: self.unmappable.load(Ordering::Relaxed),
            declined: self.declined.load(Ordering::Relaxed),
        }
    }
}

impl Placer for SchedPlacer {
    /// The host the scheduler gives the start, as an index of `open`.
    ///
    /// When the scheduler cannot be built over these hosts or this request (a shape that
    /// does not describe a node, a capability that is not a platform requirement), or is
    /// built and places the start nowhere though the registry found room for it, the first
    /// host not in `marked` is booked, as a registry with no placer does: the registry's
    /// check of the room stands, and a difference between the two models must not leave a
    /// lease retrying for ever. Each such start is counted by cause and logged. A host in
    /// `marked` is booked only when the scheduler places the start there as backfill; with
    /// no other host, none is.
    ///
    /// The scheduler is built afresh for each start over every open host and every lease on
    /// them, so a start costs time in their number (hosts plus leases), and the registry's
    /// lock is held for it. Nothing is cached between starts.
    ///
    /// A probe never falls back: the scheduler places it on its host, or the start books
    /// none (`NoCapacity`). Its host is cordoned in the scheduler, as the registry found it
    /// when the probe was admitted there ([`Placer::admit_probe`]).
    fn pick(&self, spec: &StartSpec, open: &[Candidate], marked: &MarkedTails) -> Option<usize> {
        if spec.request().probe_host.is_some() {
            return grant(spec, open, marked, self.backfill).ok().flatten();
        }
        let (counter, cause) = match grant(spec, open, marked, self.backfill) {
            Ok(Some(at)) => return Some(at),
            Ok(None) => (
                &self.declined,
                "the scheduler placed the start nowhere".to_owned(),
            ),
            Err(err) => (&self.unmappable, err.to_string()),
        };
        let first = open.iter().position(|c| !marked.contains_key(&c.host));
        first.inspect(|_| {
            let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
            warn!(lease = %spec.lease(), %cause, count, "placement fell back to the first host");
        })
    }

    /// The probe rule of the scheduler ([`js_sched::admit_probe`]) over what the registry
    /// knows of the probe's host ([`super::probe::admit`]).
    fn admit_probe(
        &self,
        spec: &StartSpec,
        pin: &HostId,
        host: Option<&ProbeHost>,
    ) -> Result<(), RefusalCode> {
        super::probe::admit(spec, pin, host)
    }
}

/// The recovered operation, numbered `number`, that stands for the lease `held` on
/// `candidate`.
fn recovered(number: u64, candidate: &Candidate, held: &js_provider::Held) -> Recovered {
    let request = Request {
        key: WorkKey::new("held", held.lease.to_string()),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: map::booking(&held.resources),
        needs: js_caps::Request::default(),
        hermetic: false,
        do_not_cache: true,
        whole_worker: held.whole,
        pin: None,
    };
    Recovered {
        operation: OperationId(number),
        request,
        waiter: WaiterId(number),
        lease: Some(RecoveredLease {
            lease: SchedLeaseId::new(0, number),
            worker: map::worker(&candidate.host),
            cost: None,
        }),
        losses: 0,
        fate: None,
    }
}

/// The index of the host the scheduler places `spec` on among `open`, or `None` when it
/// places it nowhere. A host in `marked` is held for the request that set it aside, until
/// the end of its tail, and takes the start only as `backfill` allows.
///
/// # Errors
/// [`MapError`] when a host or the request has no scheduler form.
fn grant(
    spec: &StartSpec,
    open: &[Candidate],
    marked: &MarkedTails,
    backfill: Backfill,
) -> Result<Option<usize>, MapError> {
    let now = FarmTime::default();
    let mut sched = Scheduler::new(TERM).with_backfill(backfill);
    let held = open
        .iter()
        .flat_map(|candidate| candidate.leases.iter().map(move |held| (candidate, held)));
    let operations: Vec<Recovered> = (0..)
        .zip(held)
        .map(|(number, (candidate, held))| recovered(number, candidate, held))
        .collect();
    let next = OperationId(u64::try_from(operations.len()).unwrap_or(u64::MAX));
    // The lease the scheduler grants is numbered as the registry's lease is, so the
    // tie-break between equal hosts (a hash of the lease and the host) varies by lease.
    sched.recover(TERM, next, spec.lease().sequence(), operations);
    for candidate in open {
        let (capacity, caps) = map::host(&candidate.shape)?;
        let up = Event::WorkerUp {
            worker: map::worker(&candidate.host),
            instance: DaemonInstance::new(candidate.host.as_str()),
            predecessor_stopped: false,
            capacity,
            caps,
        };
        feed(&mut sched, now, up);
        // The scheduler's clock starts now, at zero: a tail ends the time it has left.
        if let Some(left) = marked.get(&candidate.host) {
            let end = left.map(|left| millis(left).saturating_add(now.as_millis()));
            let start = end.map(FarmTime::from_millis);
            sched.reserve_outside(map::worker(&candidate.host), start);
        }
    }
    let mut request = map::job(spec.request(), &spec.lease().to_string())?;
    request.whole_worker = spec.class().admission().books_whole_host();
    // A probe's host is shown only once it was admitted there: it is cordoned.
    if let Some(pin) = &request.pin {
        feed(
            &mut sched,
            now,
            Event::Cordon {
                worker: pin.clone(),
            },
        );
    }
    // The lease table ends the lease at its wall time: the end backfill reads.
    sched.set_max_wall(sched.next_operation(), spec.request().max_wall);
    let submit = Event::Submit {
        waiter: WaiterId(u64::MAX),
        request,
    };
    let mut effects = feed(&mut sched, now, submit);
    effects.extend(feed(&mut sched, now, Event::Tick));
    let placed = effects.iter().find_map(|effect| match effect {
        Effect::Start(start) => Some(start.worker.clone()),
        _ => None,
    });
    Ok(placed.and_then(|worker| {
        open.iter()
            .position(|candidate| map::worker(&candidate.host) == worker)
    }))
}

/// `duration` in milliseconds, saturating.
fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use js_provider::Held;
    use js_types::{Admission, ClassId, HostId, LeaseId, Request as HostRequest};
    use js_types::{ResourceClass, Resources, Shape};

    use super::*;

    /// No host set aside.
    fn unmarked() -> MarkedTails {
        MarkedTails::new()
    }

    fn shape(arch: &[(&str, &str)]) -> Shape {
        let caps = arch
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Shape::new(Resources::new(8000, 16 << 30, 0, None).unwrap(), caps)
    }

    fn x86() -> Shape {
        shape(&[("arch", "x86_64")])
    }

    /// A host named `name` carrying one lease of each of `cpus` millis, which share it.
    fn host(name: &str, shape: Shape, cpus: &[u64]) -> Candidate {
        let leases = (1..).zip(cpus).map(|(n, cpu)| Held {
            lease: LeaseId::new(1, n),
            resources: Resources::new(*cpu, 1 << 20, 0, None).unwrap(),
            whole: false,
        });
        Candidate {
            host: HostId::new(name).unwrap(),
            shape,
            leases: leases.collect(),
        }
    }

    fn sharing() -> ResourceClass {
        let id = ClassId::new("share").unwrap();
        ResourceClass::new(id, Admission::Book(Default::default()), 0)
    }

    fn spec_of(class: &ResourceClass, sequence: u64) -> StartSpec {
        spec_with(class, sequence, &[])
    }

    fn spec_with(class: &ResourceClass, sequence: u64, caps: &[(&str, &str)]) -> StartSpec {
        let request = HostRequest {
            class: class.id().clone(),
            resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
            capabilities: caps
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
            probe_host: None,
        };
        let known = std::slice::from_ref(class);
        StartSpec::new(LeaseId::new(2, sequence), request, known).unwrap()
    }

    fn spec() -> StartSpec {
        spec_of(&sharing(), 1)
    }

    /// Catches: the first host by name taken in place of the scheduler's rank, or the
    /// leases on a host left out of it. The scheduler's score prefers the host the lease
    /// fits most tightly, which here is the one that carries most: `c`, then `b`.
    #[test]
    fn the_host_the_lease_fits_most_tightly_is_picked() {
        let open = [
            host("a", x86(), &[2000]),
            host("b", x86(), &[1000]),
            host("c", x86(), &[3000]),
        ];
        assert_eq!(
            SchedPlacer::default().pick(&spec(), &open, &unmarked()),
            Some(2)
        );
        let open = [
            host("a", x86(), &[1000]),
            host("b", x86(), &[1000, 1000]),
            host("c", x86(), &[]),
        ];
        assert_eq!(
            SchedPlacer::default().pick(&spec(), &open, &unmarked()),
            Some(1)
        );
    }

    /// Catches: equal hosts always broken the same way (the first by name, or the lease
    /// number left out of the scheduler's tie-break hash): the scheduler breaks a tie by a
    /// hash of the lease and the host, which prefers `b` for lease 1 and `a` for lease 3.
    /// A lease that books the whole host takes the first by name.
    #[test]
    fn equal_hosts_are_told_apart_by_the_lease() {
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 1), &open, &unmarked()),
            Some(1)
        );
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 3), &open, &unmarked()),
            Some(0)
        );
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&whole, 1), &open, &unmarked()),
            Some(0)
        );
    }

    /// Catches: the module header's account of a whole-host lease going stale. A lease that
    /// books the whole host is offered only empty hosts, and takes the first by host id
    /// whatever the order they are offered in and whatever its number, where a lease that
    /// shares is placed by the hash tie-break and so lands on more than one of them.
    #[test]
    fn a_whole_host_lease_takes_the_first_open_host_by_id() {
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        let orders = [["a", "b", "c"], ["c", "b", "a"], ["b", "c", "a"]];
        let mut shared_hosts = std::collections::BTreeSet::new();
        for names in orders {
            let open: Vec<Candidate> = names.iter().map(|name| host(name, x86(), &[])).collect();
            let first = names.iter().position(|name| *name == "a");
            for sequence in 1..=16 {
                let placer = SchedPlacer::default();
                let picked = placer.pick(&spec_of(&whole, sequence), &open, &unmarked());
                assert_eq!(picked, first, "order {names:?}, lease {sequence}");
                let shared = placer
                    .pick(&spec_of(&sharing(), sequence), &open, &unmarked())
                    .unwrap();
                shared_hosts.insert(names[shared]);
                let none = Fallbacks {
                    unmappable: 0,
                    declined: 0,
                };
                assert_eq!(placer.fallbacks(), none);
            }
        }
        assert!(shared_hosts.len() > 1, "{shared_hosts:?}");
    }

    /// Catches: a host the scheduler cannot describe (no `arch`) failing the start or being
    /// skipped silently: the registry's first host is booked, as without a placer, and the
    /// fallback is counted as unmappable. With no host there is nothing to book or count.
    #[test]
    fn hosts_that_do_not_describe_a_node_fall_back_to_the_first_and_are_counted() {
        let placer = SchedPlacer::default();
        let open = [host("a", shape(&[]), &[3000]), host("b", x86(), &[])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), Some(0));
        assert_eq!(placer.pick(&spec(), &[], &unmarked()), None);
        let counted = Fallbacks {
            unmappable: 1,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: a request the scheduler cannot describe (a capability that is not a
    /// platform requirement) failing the start or being placed nowhere: the first host is
    /// booked, where the scheduler would have taken `b`, which carries the more, and the
    /// fallback is counted.
    #[test]
    fn a_request_that_is_not_a_platform_requirement_falls_back_to_the_first() {
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[]), host("b", x86(), &[3000])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), Some(1));
        let odd = spec_with(&sharing(), 1, &[("colour", "red")]);
        assert_eq!(placer.pick(&odd, &open, &unmarked()), Some(0));
        assert_eq!(placer.fallbacks().unmappable, 1);
    }

    /// Catches: a start the registry found room for becoming `no-capacity` because the
    /// scheduler disagrees (here its one host is booked in full by a lease, which the
    /// candidate lists though the registry offered it): the registry's first host is booked
    /// and the fallback is counted as declined, not as unmappable.
    #[test]
    fn a_start_the_scheduler_declines_falls_back_to_the_first_and_is_counted() {
        let placer = SchedPlacer::default();
        let open = [host("a", x86(), &[8000])];
        assert_eq!(placer.pick(&spec(), &open, &unmarked()), Some(0));
        let counted = Fallbacks {
            unmappable: 0,
            declined: 1,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// `spec_with` numbered `sequence` with `caps`, a probe pinned to `pin`.
    fn probe_with(sequence: u64, pin: &str, caps: &[(&str, &str)]) -> StartSpec {
        let mut request = spec_with(&sharing(), sequence, caps).request().clone();
        request.probe_host = Some(HostId::new(pin).unwrap());
        StartSpec::new(LeaseId::new(2, sequence), request, &[sharing()]).unwrap()
    }

    /// Catches: a probe that falls back to the first host when the scheduler places it
    /// nowhere (its host full), which would oversubscribe the host, and a probe the
    /// scheduler refuses because its host is not cordoned in the scheduler (it would never
    /// be placed). Neither is counted as a fallback.
    #[test]
    fn a_probe_is_placed_on_its_cordoned_host_or_nowhere_and_never_falls_back() {
        let placer = SchedPlacer::default();
        let probe = probe_with(1, "a", &[]);
        assert_eq!(
            placer.pick(&probe, &[host("a", x86(), &[3000])], &unmarked()),
            Some(0)
        );
        assert_eq!(
            placer.pick(&probe, &[host("a", x86(), &[8000])], &unmarked()),
            None
        );
        let odd = probe_with(2, "a", &[("colour", "red")]);
        assert_eq!(
            placer.pick(&odd, &[host("a", x86(), &[])], &unmarked()),
            None
        );
        let none = Fallbacks {
            unmappable: 0,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), none);
    }

    /// Catches: a probe whose request or host the scheduler cannot describe judged by
    /// some other rule, or admitted: it is refused as a probe the server cannot place.
    #[test]
    fn a_probe_with_no_scheduler_form_is_refused_as_unsupported() {
        let placer = SchedPlacer::default();
        let a = HostId::new("a").unwrap();
        let known = |shape| ProbeHost {
            shape,
            cordoned: true,
            phase: js_agent_wire::Phase::Serving,
            taint: None,
            leaving: false,
        };
        let probe = probe_with(1, "a", &[]);
        assert_eq!(placer.admit_probe(&probe, &a, Some(&known(x86()))), Ok(()));
        let unsupported = Err(RefusalCode::ProbeUnsupported);
        let no_arch = known(shape(&[]));
        assert_eq!(placer.admit_probe(&probe, &a, Some(&no_arch)), unsupported);
        let odd = probe_with(2, "a", &[("colour", "red")]);
        assert_eq!(
            placer.admit_probe(&odd, &a, Some(&known(x86()))),
            unsupported
        );
        let unknown = Err(RefusalCode::ProbeHostUnknown);
        assert_eq!(placer.admit_probe(&probe, &a, None), unknown);
    }

    /// A placer that backfills, trusting any wall time.
    fn backfilling() -> SchedPlacer {
        SchedPlacer::new(Backfill::enforced(Duration::MAX))
    }

    /// `hosts` marked, each with the time left until its tail ends in seconds (`None`: not
    /// known).
    fn marks(hosts: &[(&str, Option<u64>)]) -> MarkedTails {
        let left = |s: &Option<u64>| s.map(Duration::from_secs);
        let named = hosts
            .iter()
            .map(|(h, s)| (HostId::new(h).unwrap(), left(s)));
        named.collect()
    }

    /// Catches: a start the scheduler cannot build (a host with no `arch`) falling back
    /// onto a marked host. With the marked `a` first and the unmarked `b` after it, `b` is
    /// booked and counted; with every host marked, none is booked and nothing is counted.
    #[test]
    fn an_unmappable_start_falls_back_to_the_first_unmarked_host_only() {
        let placer = backfilling();
        let open = [host("a", x86(), &[]), host("b", shape(&[]), &[])];
        let a_marked = marks(&[("a", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &a_marked), Some(1));
        let both = marks(&[("a", Some(600)), ("b", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &both), None);
        let counted = Fallbacks {
            unmappable: 1,
            declined: 0,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: a start the scheduler declines falling back onto a marked host. `a` is
    /// marked with a tail too short for the 60 s lease and `b` is full in the scheduler's
    /// eyes: the scheduler places it nowhere, and the registry's `b` is booked and counted;
    /// with `b` marked too, none is booked and nothing more is counted.
    #[test]
    fn a_declined_start_falls_back_to_the_first_unmarked_host_only() {
        let placer = backfilling();
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[8000])];
        let short = marks(&[("a", Some(100))]);
        assert_eq!(placer.pick(&spec(), &open, &short), Some(1));
        let both = marks(&[("a", Some(100)), ("b", Some(600))]);
        assert_eq!(placer.pick(&spec(), &open, &both), None);
        let counted = Fallbacks {
            unmappable: 0,
            declined: 1,
        };
        assert_eq!(placer.fallbacks(), counted);
    }

    /// Catches: one marked host's tail read for another. Four marked hosts, each with a
    /// lease: `a`'s tail has 100 s left (too short for a 60 s lease and its minute), `b`'s
    /// is unknown, `c`'s is past, and `d`'s has 600 s left. In every order they are offered
    /// in, the lease goes to `d`; with `d` unmarked and full it goes nowhere.
    #[test]
    fn each_marked_host_is_held_to_its_own_tail() {
        let tails = marks(&[
            ("a", Some(100)),
            ("b", None),
            ("c", Some(0)),
            ("d", Some(600)),
        ]);
        let orders = [
            ["a", "b", "c", "d"],
            ["d", "c", "b", "a"],
            ["c", "a", "d", "b"],
        ];
        for names in orders {
            let open: Vec<Candidate> = names.iter().map(|n| host(n, x86(), &[1000])).collect();
            let picked = backfilling().pick(&spec(), &open, &tails).unwrap();
            assert_eq!(names[picked], "d", "{names:?}");
        }
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[1000])];
        let none_fit = marks(&[("a", Some(100)), ("b", Some(119))]);
        assert_eq!(backfilling().pick(&spec(), &open, &none_fit), None);
        let fits = marks(&[("a", Some(100)), ("b", Some(120))]);
        assert_eq!(backfilling().pick(&spec(), &open, &fits), Some(1));
    }

    /// Catches: the default placer backfilling: it books no marked host, whatever its
    /// tail.
    #[test]
    fn the_default_placer_books_no_marked_host() {
        let open = [host("a", x86(), &[1000]), host("b", x86(), &[1000])];
        let tails = marks(&[("a", Some(3_600)), ("b", Some(3_600))]);
        assert_eq!(SchedPlacer::default().pick(&spec(), &open, &tails), None);
    }
}
