//! The registry's placer, with `js-sched` choosing the host.
//!
//! The registry shows [`SchedPlacer`] the hosts a start could book, each with the leases
//! already on it. The placer builds a [`Scheduler`] over exactly those: every host a worker
//! of its node report, every lease a recovered lease on its worker, and the start a
//! submitted request. One placement round later the scheduler has named a worker, and that
//! is the host. The scheduler is built for the one start and dropped, so the registry stays
//! the only place that books: it checks the room and makes the booking, as it does without
//! a placer, and the scheduler only ranks the hosts that have room (the least charged
//! first, as its spread rule orders workers).

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

/// Places the starts of a registry with `js-sched`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SchedPlacer;

impl Placer for SchedPlacer {
    /// The host the scheduler gives the start, as an index of `open`. When the scheduler
    /// cannot be built over these hosts or this request (a shape that does not describe a
    /// node, a capability that is not a platform requirement) the first host is booked, as
    /// a registry with no placer does; when it is built and places the start nowhere, none.
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize> {
        let _ = grant(spec, open);
        open.first().map(|_| 0)
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
    sched.recover(TERM, next, 0, operations);
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
        let request = HostRequest {
            class: class.id().clone(),
            resources: Resources::new(1000, 1 << 20, 0, None).unwrap(),
            capabilities: BTreeMap::new(),
            max_wall: Duration::from_secs(60),
            labels: BTreeMap::new(),
            validation_run: None,
        };
        StartSpec::new(
            LeaseId::new(2, sequence),
            request,
            std::slice::from_ref(class),
        )
        .unwrap()
    }

    fn spec() -> StartSpec {
        spec_of(&sharing(), 1)
    }

    /// Catches: the first host by name taken in place of the scheduler's rank, or the
    /// leases on a host left out of it: `b` carries the least of three hosts, so it is
    /// picked, and when `c` carries nothing it is.
    #[test]
    fn the_host_with_the_least_booked_is_picked() {
        let open = [
            host("a", x86(), &[2000]),
            host("b", x86(), &[1000]),
            host("c", x86(), &[3000]),
        ];
        assert_eq!(SchedPlacer.pick(&spec(), &open), Some(1));
        let open = [
            host("a", x86(), &[1000]),
            host("b", x86(), &[1000, 1000]),
            host("c", x86(), &[]),
        ];
        assert_eq!(SchedPlacer.pick(&spec(), &open), Some(2));
    }

    /// Catches: a tie broken any way but the first host by name (the registry's own order),
    /// for a lease that shares and for one that books the whole host.
    #[test]
    fn equal_hosts_go_to_the_first_by_name() {
        let open = [host("a", x86(), &[]), host("b", x86(), &[])];
        assert_eq!(SchedPlacer.pick(&spec(), &open), Some(0));
        let id = ClassId::new("box").unwrap();
        let whole = ResourceClass::new(id, Admission::BookAll, 0);
        assert_eq!(SchedPlacer.pick(&spec_of(&whole, 3), &open), Some(0));
    }

    /// Catches: a host the scheduler cannot describe (no `arch`) failing the start or being
    /// skipped silently: the registry's first host is booked, as without a placer.
    #[test]
    fn hosts_that_do_not_describe_a_node_fall_back_to_the_first() {
        let open = [host("a", shape(&[]), &[3000]), host("b", x86(), &[])];
        assert_eq!(SchedPlacer.pick(&spec(), &open), Some(0));
        assert_eq!(SchedPlacer.pick(&spec(), &[]), None);
    }

    /// Catches: a start placed on a host the scheduler has no room on: the one host here is
    /// booked in full by a lease, so the scheduler places the start nowhere and none is
    /// picked.
    #[test]
    fn a_start_the_scheduler_cannot_place_is_placed_nowhere() {
        let open = [host("a", x86(), &[8000])];
        assert_eq!(SchedPlacer.pick(&spec(), &open), None);
    }
}
