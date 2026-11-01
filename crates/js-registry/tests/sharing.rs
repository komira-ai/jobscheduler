//! Leases that share a host, with a placer set: the class that books part of a host is
//! admitted, the placer picks among the hosts with room, a host that lists the runs of
//! its first lease takes a second, a restored lease keeps what it booked, and every
//! lease of a host counts toward its tail. Without a placer none of it changes.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use js_agent_wire::{Command, Heartbeat, HeldRun, HeldState, Hello};
use js_pool_static::Candidate;
use js_provider::{Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{Placer, PoolConfig, RunRefusal};
use js_types::{Admission, ClassId, Dims, Request, ResourceClass, Resources};
use support::world::*;
use support::*;

fn book() -> ResourceClass {
    ResourceClass::new(
        ClassId::new("book").unwrap(),
        Admission::Book(Dims::new()),
        0,
    )
}

/// A `book` lease of one core and 1 MiB, enough for one run of [`job`].
fn sharing_spec(sequence: u64, wall_s: u64) -> StartSpec {
    sharing_spec_of(
        sequence,
        wall_s,
        Resources::new(1000, 1 << 20, 0, None).unwrap(),
    )
}

/// A `book` lease that books `resources`.
fn sharing_spec_of(sequence: u64, wall_s: u64, resources: Resources) -> StartSpec {
    let request = Request {
        class: book().id().clone(),
        max_wall: Duration::from_secs(wall_s),
        resources,
        ..request()
    };
    StartSpec::new(lease(sequence), request, &[book()]).unwrap()
}

/// A run named `run` with limits of `cpu_millis` and `memory_bytes`.
fn job_of(run: &str, cpu_millis: u64, memory_bytes: u64) -> js_agent_wire::JobSpec {
    let mut spec = job(run);
    spec.limits.cpu_millis = cpu_millis;
    spec.limits.memory_bytes = memory_bytes;
    spec
}

/// A placer that books the last candidate it is shown, and remembers which hosts it was
/// shown at each start.
#[derive(Default)]
struct Last(Mutex<Vec<Vec<String>>>);

impl Placer for Last {
    fn pick(&self, _: &StartSpec, open: &[Candidate]) -> Option<usize> {
        let shown = open.iter().map(|c| c.host.to_string()).collect();
        self.0.lock().unwrap().push(shown);
        open.len().checked_sub(1)
    }
}

/// A placer that books the host named for the lease's sequence.
struct Named(Vec<(u64, &'static str)>);

impl Placer for Named {
    fn pick(&self, spec: &StartSpec, open: &[Candidate]) -> Option<usize> {
        let sequence = spec.lease().sequence();
        let (_, want) = self.0.iter().find(|(n, _)| *n == sequence)?;
        open.iter().position(|c| c.host.as_str() == *want)
    }
}

struct Pool {
    fx: Fixture,
    provider: js_registry::RegisteredProvider,
}

/// A pool of `book` leases (4 cores, 8 GiB per host; a lease asks for one core) with the
/// hosts `hosts` joined.
fn pool(hosts: &[&str]) -> Pool {
    let name = js_types::ProviderId::new("builders").unwrap();
    let fx = fixture_of(PoolConfig::new(name, book(), pool_shape(), 4));
    hosts.iter().for_each(|h| {
        fx.join(h, "t");
    });
    let provider = fx.registry.provider();
    Pool { fx, provider }
}

fn on(ack: &StartAck) -> Option<&str> {
    started_on(ack)
}

/// Catches: the flag-off state changing. With no placer the class that books part of a
/// host is refused `UnsupportedClass`, as it always was, and nothing is booked.
#[test]
fn without_a_placer_a_class_that_shares_is_refused() {
    let mut p = pool(&["a"]);
    let ack = p.provider.start(sharing_spec(1, 600));
    assert_eq!(ack, StartAck::Refused(RefusalCode::UnsupportedClass));
    assert!(p.provider.booked().is_empty());
}

/// Catches: a placer that does not open the class, or a second lease refused though the
/// host has room: two leases start on the one host, the capacity listing shows both in the
/// order they were booked, and the fifth, past the host's four cores, is `NoCapacity`.
#[test]
fn with_a_placer_a_second_lease_starts_beside_the_first() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let first = p.provider.start(sharing_spec(1, 600));
    let second = p.provider.start(sharing_spec(2, 600));
    assert_eq!((on(&first), on(&second)), (Some("a"), Some("a")));
    assert_eq!(p.provider.capacity()[0].leases, vec![lease(1), lease(2)]);
    for sequence in 3..=4 {
        p.provider.start(sharing_spec(sequence, 600));
    }
    assert_eq!(
        p.provider.start(sharing_spec(5, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert_eq!(p.provider.capacity()[0].leases.len(), 4);
}

/// Catches: the placer not consulted, shown a host that has no room, or the registry
/// booking the first host by id: the placer takes the last candidate, so four leases fill
/// `b`; it is shown both hosts until `b` is full and then `a` alone, and the fifth lease
/// lands on `a`.
#[test]
fn the_placer_picks_among_the_hosts_with_room() {
    let mut p = pool(&["a", "b"]);
    let last = Arc::new(Last::default());
    p.fx.registry.set_placer(last.clone());
    let acks: Vec<StartAck> = (1..=5)
        .map(|n| p.provider.start(sharing_spec(n, 600)))
        .collect();
    let hosts: Vec<Option<&str>> = acks.iter().map(on).collect();
    assert_eq!(
        hosts,
        vec![Some("b"), Some("b"), Some("b"), Some("b"), Some("a")]
    );
    let seen = last.0.lock().unwrap().clone();
    let both = vec!["a".to_string(), "b".to_string()];
    assert_eq!(seen[..4], vec![both; 4]);
    assert_eq!(seen[4], vec!["a".to_string()]);
}

/// Catches: a placer that declines, or one that names a host past the candidates, booking
/// anyway: the start is `NoCapacity` and nothing is booked.
#[test]
fn a_placer_that_picks_none_books_none() {
    let mut p = pool(&["a"]);
    p.fx.registry
        .set_placer(Arc::new(Named(vec![(1, "nowhere")])));
    assert_eq!(
        p.provider.start(sharing_spec(1, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    assert!(p.provider.booked().is_empty());
}

/// Catches: the quiet rule applied to a lease that shares a host that is busy with its
/// first lease: a run of lease 1 is live on `a`, so the host is not quiet, and lease 2
/// still starts there.
#[test]
fn a_host_running_the_first_lease_takes_a_second() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    assert_eq!(on(&p.provider.start(sharing_spec(1, 600))), Some("a"));
    p.fx.registry
        .queue_run(&host("a"), &lease(1), job("r1"))
        .unwrap();
    assert_eq!(on(&p.provider.start(sharing_spec(2, 600))), Some("a"));
}

/// Catches: a lease that is put back booking the whole host whatever it shares: two
/// restored leases of 2 cores each fill the host's 4 cores, so a start of one more core is
/// `NoCapacity` until one of them is stopped, and then it starts.
#[test]
fn a_restored_sharing_lease_keeps_its_resources() {
    let mut p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let two = || Some(Resources::new(2000, 1, 0, None).unwrap());
    let seat = |n: u64| p.provider.seat_booked(&host("a"), lease(n), None, two());
    assert_eq!((seat(1), seat(2)), (Ok(()), Ok(())));
    p.fx.join("a", "t");
    assert_eq!(
        p.provider.start(sharing_spec(3, 600)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    p.provider.stop(&lease(1));
    assert_eq!(on(&p.provider.start(sharing_spec(3, 600))), Some("a"));
    assert_eq!(p.provider.capacity()[0].leases, vec![lease(2), lease(3)]);
}

/// Catches: a lease that shares a host admitted beside runs that no admission counts. A
/// share is restored on `a` and the host comes back listing a run the registry does not
/// know (an orphan of the run before the restart): a second share is refused, though the
/// host has room, until a heartbeat lists nothing. A run of the booked lease, queued and
/// then listed by the host, does not keep the next share out; a live run of a lease that
/// is no longer booked does, until it is gone.
#[test]
fn a_share_is_kept_off_a_host_that_holds_runs_of_no_booked_lease() {
    let mut p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let one = Some(Resources::new(1000, 1 << 20, 0, None).unwrap());
    let seated = p.provider.seat_booked(&host("a"), lease(1), None, one);
    assert_eq!(seated, Ok(()));
    let orphan = HeldRun {
        run_id: "orphan".into(),
        state: HeldState::Running,
    };
    let back = Hello {
        held: vec![orphan],
        ..hello("a")
    };
    let epoch = p.fx.hello(&back, &cred("t")).unwrap().epoch;
    let refused = StartAck::Refused(RefusalCode::NoCapacity);
    assert_eq!(p.provider.start(sharing_spec(2, 600)), refused);
    p.fx.beat("a", epoch);
    assert_eq!(on(&p.provider.start(sharing_spec(2, 600))), Some("a"));

    // A run of a booked lease, listed by the host, is one the share may meet.
    p.fx.registry
        .queue_run(&host("a"), &lease(1), job("r1"))
        .unwrap();
    let listing = Heartbeat {
        held: vec![HeldRun {
            run_id: "r1".into(),
            state: HeldState::Running,
        }],
        ..p.fx.heartbeat("a", epoch)
    };
    p.fx.registry.heartbeat(&listing, &cred("t")).unwrap();
    assert_eq!(on(&p.provider.start(sharing_spec(3, 600))), Some("a"));

    // Once lease 1 is no longer booked, its run still live keeps the next share out.
    p.provider.stop(&lease(1));
    p.fx.registry.heartbeat(&listing, &cred("t")).unwrap();
    assert_eq!(p.provider.start(sharing_spec(4, 600)), refused);
}

/// Catches: a lease that shares a host running past what it booked, on the host's whole
/// budget. Two leases share `a` (4 cores, 8 GiB), each booking 2 cores and 1 GiB. A run of
/// lease 1 past its share (one millicore or one byte over) is refused as past the shape,
/// though the host has room for it; its runs together may fill its share exactly, and one
/// millicore more is refused for capacity. Lease 2's share is untouched: it still runs its
/// full 2 cores and 1 GiB.
#[test]
fn a_run_is_held_to_the_share_its_lease_booked() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let share = Resources::new(2000, 1 << 30, 0, None).unwrap();
    for n in 1..=2 {
        let ack = p.provider.start(sharing_spec_of(n, 600, share.clone()));
        assert_eq!(on(&ack), Some("a"));
    }
    assert_runs_held_to_share(&p);
}

/// The checks of [`a_run_is_held_to_the_share_its_lease_booked`], over a host `a` where
/// leases 1 and 2 each book 2 cores and 1 GiB.
fn assert_runs_held_to_share(p: &Pool) {
    let queue = |n: u64, spec| p.fx.registry.queue_run(&host("a"), &lease(n), spec);
    let gib = 1 << 30;
    assert_eq!(
        queue(1, job_of("over-cpu", 2001, 1)).unwrap_err(),
        RunRefusal::LimitsExceedShape
    );
    assert_eq!(
        queue(1, job_of("over-mem", 1, gib + 1)).unwrap_err(),
        RunRefusal::LimitsExceedShape
    );
    queue(1, job_of("a1", 1500, gib / 2)).unwrap();
    queue(1, job_of("a2", 500, gib / 2)).unwrap();
    assert_eq!(
        queue(1, job_of("a3", 1, 1)).unwrap_err(),
        RunRefusal::NoCapacity
    );
    queue(2, job_of("b1", 2000, gib)).unwrap();
}

/// Catches: a lease put back after a restart whose runs are held to the whole host, or to
/// nothing: two shares are restored on `a` with what they booked, and their runs are held
/// to those shares as a lease that started here is.
#[test]
fn a_restored_lease_runs_within_its_share() {
    let p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let share = || Some(Resources::new(2000, 1 << 30, 0, None).unwrap());
    for n in 1..=2 {
        let seated = p.provider.seat_booked(&host("a"), lease(n), None, share());
        assert_eq!(seated, Ok(()));
    }
    p.fx.join("a", "t");
    assert_runs_held_to_share(&p);
}

/// Catches: a queued run that the host never listed, of a lease that is no longer booked,
/// let through by a share check that reads only the host's list. Lease 1 queues a run that
/// is not yet listed, lease 2 stays booked beside it, and lease 1 is stopped: a third
/// share is refused while the run is queued. The heartbeat after the stop carries the
/// stop of the run; the host's next heartbeat, which still does not list it, retires it,
/// and only then is the share admitted.
#[test]
fn an_unlisted_run_of_an_ended_lease_keeps_a_share_out_until_it_is_retired() {
    let mut p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let epoch = p.fx.join("a", "t");
    for n in 1..=2 {
        assert_eq!(on(&p.provider.start(sharing_spec(n, 600))), Some("a"));
    }
    p.fx.registry
        .queue_run(&host("a"), &lease(1), job("queued"))
        .unwrap();
    p.provider.stop(&lease(1));
    let refused = StartAck::Refused(RefusalCode::NoCapacity);
    assert_eq!(p.provider.start(sharing_spec(3, 600)), refused);
    let beat = || {
        let empty = p.fx.heartbeat("a", epoch);
        p.fx.registry.heartbeat(&empty, &cred("t")).unwrap()
    };
    let mut beats = 0;
    while p.provider.start(sharing_spec(3, 600)) == refused {
        beats += 1;
        assert!(beats <= 3, "the run of the stopped lease is never retired");
        let reply = beat();
        let stops = reply
            .commands
            .iter()
            .filter(|c| matches!(c, Command::Stop { .. }));
        assert!(beats > 1 || stops.count() == 1, "{reply:?}");
    }
    assert_eq!(p.provider.capacity()[0].leases, vec![lease(2), lease(3)]);
}

/// The world of the reservation tests with `book` leases beside its whole-node requests:
/// hosts `a` and `b`, a placer that books each lease on the host named for it.
fn tail_world(homes: Vec<(u64, &'static str)>) -> World {
    let mut w = World::new(&["a", "b"], plain);
    w.fx.registry.set_placer(Arc::new(Named(homes)));
    w.provider = w.provider.clone().with_classes(vec![whole(), book()]);
    w
}

/// Starts the `book` lease `sequence` of `wall_s` seconds.
fn lease_of(w: &mut World, sequence: u64, wall_s: u64) {
    let ack = w.provider.start(sharing_spec(sequence, wall_s));
    w.check();
    assert!(on(&ack).is_some(), "{ack:?}");
}

/// Catches: only the lease started last counted toward the host's tail. `a` carries a
/// lease that may run 900 s and one started after it that may run 100 s; `b` one of 600 s.
/// `a` ends last (900 s), so the whole-node request is set aside `b`. Counting the later
/// lease alone puts `a` at 100 s and sets it aside.
#[test]
fn every_lease_of_a_host_counts_toward_its_tail() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 900);
    lease_of(&mut w, 2, 100);
    lease_of(&mut w, 3, 600);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the end of the lease started first lost when the lease started after it is
/// stopped: `a` is left with its 900 s lease, so it ends after `b` (600 s) and the request
/// is set aside `b`. With the end lost `a` reads as empty and is set aside.
#[test]
fn stopping_the_later_lease_leaves_the_earlier_end_counted() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 900);
    lease_of(&mut w, 2, 100);
    lease_of(&mut w, 3, 600);
    w.stop(2);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Guard: stopping the lease started first leaves the later lease's end counted: `a` is
/// left with its 900 s lease and ends after `b` (600 s), so the request is set aside `b`.
#[test]
fn stopping_the_earlier_lease_leaves_the_later_end_counted() {
    let mut w = tail_world(vec![(1, "a"), (2, "a"), (3, "b")]);
    lease_of(&mut w, 1, 100);
    lease_of(&mut w, 2, 900);
    lease_of(&mut w, 3, 600);
    w.stop(1);
    w.ask(4, "q");
    assert_eq!(w.reserved_host("q"), Some("b".to_string()));
}

/// Catches: the stop of a lease other than the one started last counting nothing. Two
/// leases share `a`; the first stops: its end is counted (it has no estimate, as no source
/// of lives is set), and so is the second's after it.
#[test]
fn stopping_an_earlier_lease_on_a_shared_host_is_counted() {
    let mut p = pool(&["a"]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    p.provider.start(sharing_spec(1, 600));
    p.provider.start(sharing_spec(2, 600));
    p.provider.stop(&lease(1));
    assert_eq!(p.fx.registry.tail_counters().ended_unestimated, 1);
    p.provider.stop(&lease(2));
    assert_eq!(p.fx.registry.tail_counters().ended_unestimated, 2);
}

/// Guard: a lease put back after a restart has no bound and no start, so its stop counts
/// nothing in the tail counters, estimated or not.
#[test]
fn stopping_a_restored_lease_counts_nothing() {
    let mut p = pool(&[]);
    p.fx.registry.set_placer(Arc::new(Last::default()));
    let two = Some(Resources::new(2000, 1, 0, None).unwrap());
    p.provider
        .seat_booked(&host("a"), lease(1), None, two)
        .unwrap();
    p.fx.join("a", "t");
    p.provider.stop(&lease(1));
    let counted = p.fx.registry.tail_counters();
    assert_eq!((counted.ended_estimated, counted.ended_unestimated), (0, 0));
}
