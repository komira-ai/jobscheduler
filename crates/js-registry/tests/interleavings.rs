//! The interleavings of the design (section 13.2) between work reaching a host and the
//! scaler marking it: a lease placement against `drain` and against `release_host`, a
//! direct run start against both, and a run started by a lease booked before the mark.
//! The scripts are drawn from seeds of the simulation kernel's random stream, so a failing
//! seed is a complete report and the same seeds give the same scripts everywhere. The
//! registry's calls are atomic under its lock, so the order of the calls is the
//! interleaving; nothing sleeps and nothing depends on timing.

mod support;

use js_provider::{Provider, StartAck};
use js_registry::{Busy, HostMark, HostView, RunRefusal};
use js_sim::SimRng;
use js_types::LeaseId;
use support::world::job;
use support::*;

/// One call on the registry, drawn from the seed.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// A lease placement (`Provider::start`).
    StartLease,
    /// A direct run start of the booked lease (`Registry::queue_run`).
    StartRun,
    /// The booked lease ends (`Provider::stop`).
    StopLease,
    Drain,
    ReleaseHost,
    Undrain,
    /// The scaler marks the host releasing whether or not it is quiet (`mark_ending`):
    /// the only call that marks a host that holds a lease.
    MarkEnding,
}

const OPS: [Op; 7] = [
    Op::StartLease,
    Op::StartRun,
    Op::StopLease,
    Op::Drain,
    Op::ReleaseHost,
    Op::Undrain,
    Op::MarkEnding,
];

/// How many times each outcome happened over the seeds, so the test can see that the
/// rules were reached and cannot pass by never meeting them.
#[derive(Default)]
struct Tally {
    leases_started: usize,
    leases_refused_on_marked: usize,
    runs_started: usize,
    runs_refused_on_marked: usize,
    drained: usize,
    drain_refused_for_lease: usize,
    released: usize,
    ended_marked_with_lease: usize,
}

fn view(fx: &Fixture) -> HostView {
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|h| h.id == host("a")).unwrap()
}

fn marked(view: &HostView) -> bool {
    view.mark
        .is_some_and(|mark| mark == HostMark::Draining || mark == HostMark::Releasing)
}

/// Plays the script `seed` draws on a pool of one host, and checks after every call what
/// the design promises: a mark on a quiet host (`drain`, `release_host`) is made only
/// when no lease is booked; a lease is placed and a run started only on a host with no
/// mark; a host holds a lease and a mark together only after `mark_ending`; and the host
/// never stops being heard from.
fn play(seed: u64, tally: &mut Tally) {
    let fx = fixture(1);
    fx.join("a", "t");
    let a = host("a");
    let mut provider = fx.registry.provider();
    let mut rng = SimRng::from_seed(seed);
    let mut booked: Option<LeaseId> = None;
    let mut next = 0u64;
    let mut forced = false;
    for step in 0..16 {
        let op = OPS[usize::try_from(rng.below(7)).unwrap()];
        let before = view(&fx);
        let tag = format!("seed {seed} step {step} {op:?}");
        match op {
            Op::StartLease => {
                next += 1;
                match provider.start(spec(next)) {
                    StartAck::Started { .. } => {
                        assert!(!marked(&before), "{tag}: booked a marked host");
                        booked = Some(lease(next));
                        tally.leases_started += 1;
                    }
                    _ => tally.leases_refused_on_marked += usize::from(marked(&before)),
                }
            }
            Op::StartRun => {
                let Some(held) = booked.clone() else { continue };
                match fx.registry.queue_run(&a, &held, job(&format!("r{step}"))) {
                    Ok(_) => {
                        assert!(!marked(&before), "{tag}: started a run on a marked host");
                        tally.runs_started += 1;
                    }
                    Err(RunRefusal::NoCapacity) => {
                        tally.runs_refused_on_marked += usize::from(marked(&before));
                    }
                    Err(_) => {}
                }
            }
            Op::StopLease => {
                if let Some(held) = booked.take() {
                    provider.stop(&held);
                }
            }
            Op::Drain => match fx.registry.drain(&a) {
                Ok(()) => {
                    assert!(before.lease.is_none(), "{tag}: drained a leased host");
                    tally.drained += 1;
                }
                Err(Busy::Lease) => tally.drain_refused_for_lease += 1,
                Err(_) => {}
            },
            Op::ReleaseHost => {
                if fx.registry.release_host(&a).is_ok() {
                    assert!(
                        before.lease.is_none() || forced,
                        "{tag}: released a leased host"
                    );
                    tally.released += 1;
                }
            }
            Op::Undrain => {
                let _ = fx.registry.undrain(&a);
            }
            Op::MarkEnding => {
                fx.registry.mark_ending(&a);
                forced = true;
            }
        }
        let after = view(&fx);
        assert!(
            !marked(&after) || after.lease.is_none() || forced,
            "{tag}: a marked host holds a lease without mark_ending"
        );
        assert_eq!(after.state, js_registry::HostState::Live, "{tag}");
    }
    let end = view(&fx);
    tally.ended_marked_with_lease += usize::from(marked(&end) && end.lease.is_some());
}

/// Asserts the interleavings (a) a lease placement against `drain`, (b) a lease placement
/// against `release_host`, (c) a direct run start against `release_host` and (d) a run
/// started by a lease booked before the mark, over 300 seeded scripts of 16 calls each:
/// in every order either the work runs and the host is kept (a drain of a leased host is
/// refused `Busy::Lease`), or the host is marked and the work is refused (a lease is not
/// placed on it, a run is not started on it). The tally shows both kinds of outcome
/// happened. It catches a placement or a run start that does not read the mark, and a mark
/// that does not read the booking.
#[test]
fn work_and_marks_never_meet_in_any_order() {
    let mut tally = Tally::default();
    for seed in 0..300 {
        play(seed, &mut tally);
    }
    for (count, what) in [
        (tally.leases_started, "a lease placed"),
        (
            tally.leases_refused_on_marked,
            "a lease refused on a marked host",
        ),
        (tally.runs_started, "a run started"),
        (
            tally.runs_refused_on_marked,
            "a run refused on a marked host",
        ),
        (tally.drained, "a host drained"),
        (tally.drain_refused_for_lease, "a drain refused for a lease"),
        (tally.released, "a host marked releasing"),
        (
            tally.ended_marked_with_lease,
            "a leased host marked by mark_ending",
        ),
    ] {
        assert!(count > 0, "no script produced: {what}");
    }
}
