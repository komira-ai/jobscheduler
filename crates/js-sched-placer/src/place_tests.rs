//! The tests of [`super::SchedPlacer`].

use std::collections::BTreeMap;
use std::time::Duration;

use js_sched::Backfill;
use js_types::Held;
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
/// skipped silently: the first host is booked, and the fallback is counted as unmappable. With no host there is nothing to book or count.
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

/// Catches: a lease the fallback booked left out of the engine, so that the next start
/// meeting its host finds the two pictures apart and rebuilds the engine. The first
/// start (the engine knows neither host yet) builds it once; the odd request falls back
/// to `a`, and the next start, shown `a` with that lease on it, agrees with the engine
/// and builds nothing more.
#[test]
fn a_lease_the_fallback_booked_is_taken_up_without_a_rebuild() {
    let placer = SchedPlacer::default();
    let open = [host("a", x86(), &[]), host("b", x86(), &[])];
    let odd = spec_with(&sharing(), 1, &[("colour", "red")]);
    assert_eq!(placer.pick(&odd, &open, &unmarked()), Some(0));
    assert_eq!((placer.resynced(), placer.built()), (1, 2));
    let booked = Held {
        lease: odd.lease(),
        resources: odd.request().resources.clone(),
        whole: false,
    };
    let mut after = open.clone();
    after[0].leases.push(booked);
    assert_eq!(
        placer.pick(&spec_of(&sharing(), 2), &after, &unmarked()),
        Some(0)
    );
    assert_eq!((placer.resynced(), placer.built()), (1, 2));
    assert_eq!(placer.fallbacks().unmappable, 1);
}

/// Catches: a start the engine places nowhere booked anyway on the registry's first
/// host, past the engine's own rules (here its one host is booked in full by a lease,
/// which the candidate lists though the registry offered it): once the engine agrees
/// with the registry's books, its decline stands, no host is booked and the start is
/// counted as declined, not as unmappable.
#[test]
fn a_start_the_scheduler_declines_books_nothing_and_is_counted() {
    let placer = SchedPlacer::default();
    let open = [host("a", x86(), &[8000])];
    assert_eq!(placer.pick(&spec(), &open, &unmarked()), None);
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
    let settings = Settings {
        backfill: Backfill::enforced(Duration::MAX),
        ..Settings::default()
    };
    let clock = js_types::ManualClock::new(js_types::Timestamp::from_millis(0));
    SchedPlacer::new(settings, clock)
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

/// Catches: a start the scheduler declines booked on a marked host or on the first
/// unmarked one. `a` is marked with a tail too short for the 60 s lease and `b` is full
/// in the scheduler's eyes: the scheduler places it nowhere and nothing is booked; with
/// `b` marked too, the same. Each decline is counted.
#[test]
fn a_declined_start_books_no_host_marked_or_not() {
    let placer = backfilling();
    let open = [host("a", x86(), &[1000]), host("b", x86(), &[8000])];
    let short = marks(&[("a", Some(100))]);
    assert_eq!(placer.pick(&spec(), &open, &short), None);
    let both = marks(&[("a", Some(100)), ("b", Some(600))]);
    assert_eq!(placer.pick(&spec(), &open, &both), None);
    let counted = Fallbacks {
        unmappable: 0,
        declined: 2,
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

/// Catches: the start of a whole-node request the engine queues placed as an ordinary
/// start (it would book the first host it is shown, not the one granted to it), or
/// counted as a decline while it waits. The placer keeps the queue over its own wait
/// list: `r` is presented there, `a` is not bookable, so `r` is granted the quiet `b`.
/// Shown `a` alone, its start books nothing; shown both, it books `b`. Nothing is
/// counted.
#[test]
fn a_whole_node_start_books_the_host_of_its_pending_grant() {
    use js_registry::{Presented, Standing, WholeHost, WholeNodeConfig};
    let placer = SchedPlacer::default();
    let open = [host("a", x86(), &[]), host("b", x86(), &[])];
    for candidate in &open {
        placer.host_up(&candidate.host, &candidate.shape);
    }
    let id = js_types::RequestId::new("r").unwrap();
    let start = spec_of(&ResourceClass::whole_node(), 7).with_request_id(id.clone());
    let grace = Duration::from_secs(600);
    let presented = placer.whole_present(&id, start.lease(), start.request(), true, grace);
    assert_eq!(presented, Presented::Waiting(0));
    let stand = |name: &str, open: bool| WholeHost {
        id: HostId::new(name).unwrap(),
        standing: Standing::Live,
        open,
    };
    let round = WholeRound {
        settings: WholeNodeConfig::dedicated(2),
        waiting: Vec::new(),
        hosts: vec![stand("a", false), stand("b", true)],
        outside: MarkedTails::new(),
        lives: BTreeMap::new(),
    };
    let marks = placer.whole_round(&round).unwrap();
    let held: Vec<(String, bool)> = marks
        .iter()
        .map(|mark| (mark.host.to_string(), mark.assigned))
        .collect();
    assert_eq!(held, [("b".to_owned(), true)]);
    assert_eq!(placer.pick(&start, &open[..1], &unmarked()), None);
    assert_eq!(placer.pick(&start, &open, &unmarked()), Some(1));
    let none = Fallbacks {
        unmappable: 0,
        declined: 0,
    };
    assert_eq!(placer.fallbacks(), none);
}

/// Catches: the default placer backfilling: it books no marked host, whatever its
/// tail.
#[test]
fn the_default_placer_books_no_marked_host() {
    let open = [host("a", x86(), &[1000]), host("b", x86(), &[1000])];
    let tails = marks(&[("a", Some(3_600)), ("b", Some(3_600))]);
    assert_eq!(SchedPlacer::default().pick(&spec(), &open, &tails), None);
}
