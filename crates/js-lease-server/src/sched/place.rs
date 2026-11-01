//! The registry's placer, with `js-sched` choosing the host.
//!
//! The registry shows [`SchedPlacer`] the hosts a start could book, each with the leases
//! already on it. The placer builds a [`Scheduler`] over exactly those: every host a worker
//! of its node report, every lease a recovered lease on its worker, and the start a
//! submitted request. One placement round later the scheduler has named a worker, and that
//! is the host. The scheduler is built for the one start and dropped, so the registry stays
//! the only place that books: it checks the room and makes the booking, as it does without
//! a placer, and the scheduler only ranks the hosts that have room, by its own score (the
//! host the lease fits most tightly; equal hosts by a hash of the lease and the host).

use std::sync::atomic::{AtomicU64, Ordering};

use js_pool_static::Candidate;
use js_provider::StartSpec;
use js_registry::Placer;
use js_sched::harness::feed;
use js_sched::{DaemonInstance, Event, Recovered, RecoveredLease, Request, Scheduler};
use js_types::sched::{
    Effect, FarmTime, LeaseId as SchedLeaseId, LeaseKind, OperationId, Qos, WaiterId, WorkKey,
};

use super::map::{self, MapError};

/// The term the scheduler is built for. It is one start long, so the number only has to
/// differ from the leases it recovers, which are numbered in term 0.
const TERM: u64 = 1;

/// Places the starts of a registry with `js-sched`, and counts the starts it could not.
#[derive(Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SchedPlacer {
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
    /// The host the scheduler gives the start, as an index of `open`. When the scheduler
    /// cannot be built over these hosts or this request (a shape that does not describe a
    /// node, a capability that is not a platform requirement) the first host is booked, as
    /// a registry with no placer does; when it is built and places the start nowhere, none.
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize> {
        match grant(spec, open) {
            Ok(chosen) => chosen,
            Err(_) => open.first().map(|_| 0),
        }
    }
}

/// The recovered operation, numbered `number`, that stands for the lease `held` on
/// `candidate`.
fn recovered(number: u64, candidate: &Candidate, held: &js_pool_static::Held) -> Recovered {
    let request = Request {
        key: WorkKey::new("held", held.lease.to_string()),
        qos: Qos::Batch,
        kind: LeaseKind::Action,
        resources: map::booking(&held.resources),
        needs: js_caps::Request::default(),
        hermetic: false,
        do_not_cache: true,
        whole_worker: held.whole,
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
/// places it nowhere.
///
/// # Errors
/// [`MapError`] when a host or the request has no scheduler form.
fn grant(spec: &StartSpec, open: &[Candidate]) -> Result<Option<usize>, MapError> {
    let now = FarmTime::default();
    let mut sched = Scheduler::new(TERM);
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
    }
    let mut request = map::job(spec.request(), &spec.lease().to_string())?;
    request.whole_worker = spec.class().admission().books_whole_host();
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use js_pool_static::Held;
    use js_types::{Admission, ClassId, HostId, LeaseId, Request as HostRequest};
    use js_types::{ResourceClass, Resources, Shape};

    use super::*;

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
        assert_eq!(SchedPlacer::default().pick(&spec(), &open), Some(2));
        let open = [
            host("a", x86(), &[1000]),
            host("b", x86(), &[1000, 1000]),
            host("c", x86(), &[]),
        ];
        assert_eq!(SchedPlacer::default().pick(&spec(), &open), Some(1));
    }

    /// Catches: equal hosts always broken the same way (the first by name, or the lease
    /// number left out of the scheduler's tie-break hash): the scheduler breaks a tie by a
    /// hash of the lease and the host, which prefers `b` for lease 1 and `a` for lease 3.
    /// A lease that books the whole host takes the first by name.
    #[test]
    fn equal_hosts_are_told_apart_by_the_lease() {
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 1), &open),
            Some(1)
        );
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&sharing(), 3), &open),
            Some(0)
        );
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        assert_eq!(
            SchedPlacer::default().pick(&spec_of(&whole, 1), &open),
            Some(0)
        );
    }

    /// Catches: a host the scheduler cannot describe (no `arch`) failing the start or being
    /// skipped silently: the registry's first host is booked, as without a placer, and the
    /// fallback is counted as unmappable. With no host there is nothing to book or count.
    #[test]
    fn hosts_that_do_not_describe_a_node_fall_back_to_the_first_and_are_counted() {
        let placer = SchedPlacer::default();
        let open = [host("a", shape(&[]), &[3000]), host("b", x86(), &[])];
        assert_eq!(placer.pick(&spec(), &open), Some(0));
        assert_eq!(placer.pick(&spec(), &[]), None);
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
        assert_eq!(placer.pick(&spec(), &open), Some(1));
        let odd = spec_with(&sharing(), 1, &[("colour", "red")]);
        assert_eq!(placer.pick(&odd, &open), Some(0));
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
        assert_eq!(placer.pick(&spec(), &open), Some(0));
        let counted = Fallbacks {
            unmappable: 0,
            declined: 1,
        };
        assert_eq!(placer.fallbacks(), counted);
    }
}
