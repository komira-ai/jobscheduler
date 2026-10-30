//! The marks of scale-down: `drain`, `undrain`, `release_host` and `remove`. What a marked
//! host refuses, when a mark is refused for a busy host, and the rule that a host is
//! released only while it is quiet, including every order in which a start, a drain, a
//! release and a report of work can reach the registry. The registry is driven directly
//! on a manual clock; nothing sleeps and no test asserts how long anything takes.

mod support;

use js_agent_wire::{HeldRun, HeldState};
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Busy, HostMark, HostState, HostView, RunRefusal};
use support::world::job;
use support::*;

fn held(run: &str) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state: HeldState::Running,
    }
}

/// A heartbeat of `host` that lists `runs`.
fn beat_listing(fx: &Fixture, host: &str, epoch: u64, runs: &[&str]) {
    let mut message = fx.heartbeat(host, epoch);
    message.held = runs.iter().map(|run| held(run)).collect();
    fx.registry.heartbeat(&message, &cred("t")).unwrap();
}

fn view(fx: &Fixture, name: &str) -> HostView {
    let hosts = fx.registry.hosts();
    hosts.into_iter().find(|h| h.id == host(name)).unwrap()
}

fn mark(fx: &Fixture, name: &str) -> Option<HostMark> {
    view(fx, name).mark
}

/// Asserts the happy path of the two marks on a quiet host: it is listed `Draining`, then
/// unmarked by `undrain`, and `drain` twice, `undrain` of an unmarked host and `release_host`
/// twice are each `Ok` and change nothing more. It catches a mark that is not visible to
/// the operator listing, and a repeat call that errors, which a scaler retrying after a
/// crash would meet.
#[test]
fn a_quiet_host_is_marked_and_unmarked() {
    let fx = fixture(2);
    fx.join("a", "t");
    let a = host("a");
    assert_eq!(mark(&fx, "a"), None);
    assert_eq!(fx.registry.undrain(&a), Ok(()));
    assert_eq!(fx.registry.drain(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    assert_eq!(fx.registry.drain(&a), Ok(()));
    assert_eq!(fx.registry.undrain(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), None);
    fx.registry.drain(&a).unwrap();
    assert_eq!(fx.registry.release_host(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Releasing));
    assert_eq!(fx.registry.release_host(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Releasing));
}

/// Asserts each refusal of a call that does not fit the host's mark: a host the registry
/// does not hold, `release_host` of a host that is not draining, and `drain` and `undrain`
/// of a releasing host, which stays releasing. It catches a releasing mark that a later
/// call quietly undoes, which would put a host back into service after the scaler
/// revoked its token.
#[test]
fn a_call_that_does_not_fit_the_mark_is_refused() {
    let fx = fixture(2);
    fx.join("a", "t");
    let (a, ghost) = (host("a"), host("ghost"));
    for call in [
        fx.registry.drain(&ghost),
        fx.registry.undrain(&ghost),
        fx.registry.release_host(&ghost),
    ] {
        assert_eq!(call, Err(Busy::UnknownHost));
    }
    assert_eq!(fx.registry.release_host(&a), Err(Busy::NotDraining));
    assert_eq!(mark(&fx, "a"), None);
    fx.registry.drain(&a).unwrap();
    fx.registry.release_host(&a).unwrap();
    assert_eq!(fx.registry.drain(&a), Err(Busy::Releasing));
    assert_eq!(fx.registry.undrain(&a), Err(Busy::Releasing));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Releasing));
}

/// Asserts that a draining host is never booked: with `a` draining, a lease goes to `b`;
/// with both marked, the next lease is refused `no-capacity`; after `undrain` it books on
/// `a`. It catches a placement path that does not read the mark.
#[test]
fn a_draining_host_takes_no_lease() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let mut provider = fx.registry.provider();
    fx.registry.drain(&host("a")).unwrap();
    assert_eq!(started_on(&provider.start(spec(1))), Some("b"));
    assert_eq!(
        provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    fx.registry.undrain(&host("a")).unwrap();
    assert_eq!(started_on(&provider.start(spec(2))), Some("a"));
}

/// Asserts that a releasing host is not booked either, and that the refusal is the same
/// `no-capacity` a full pool gives. It catches a release that stops at the draining mark's
/// checks.
#[test]
fn a_releasing_host_takes_no_lease() {
    let fx = fixture(1);
    fx.join("a", "t");
    let mut provider = fx.registry.provider();
    fx.registry.drain(&host("a")).unwrap();
    fx.registry.release_host(&host("a")).unwrap();
    assert_eq!(
        provider.start(spec(1)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that `drain` refuses a host that holds a lease with `Busy::Lease`, leaves it
/// unmarked and still booking its lease's runs, and succeeds once the lease is stopped and
/// the host lists nothing. It catches a drain that marks a leased host: its work would be
/// cut by the release that follows.
#[test]
fn drain_refuses_a_host_with_a_lease() {
    let fx = fixture(1);
    fx.join("a", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    assert_eq!(fx.registry.drain(&host("a")), Err(Busy::Lease));
    assert_eq!(mark(&fx, "a"), None);
    assert_eq!(view(&fx, "a").lease, Some(lease(1)));
    provider.stop(&lease(1));
    assert_eq!(fx.registry.drain(&host("a")), Ok(()));
}

/// Asserts that `retire` marks a host that holds a lease and one that lists a run, which
/// `drain` refuses, without cutting either: the lease stays booked and the run listed, no
/// new lease lands on the host, `release_host` still answers `Busy` until both are over, and
/// works after. It also asserts that retiring twice is `Ok`, a host the registry does not
/// hold is `UnknownHost`, and a releasing host stays releasing (`Busy::Releasing`). It
/// catches a retire that waits for quiet (a never-idle host would live on), one that cuts
/// the work, and one that undoes a release.
#[test]
fn retire_marks_a_busy_host_and_cuts_nothing() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    fx.join("b", "t");
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    beat_listing(&fx, "a", epoch, &["r1"]);
    let a = host("a");
    assert_eq!(fx.registry.drain(&a), Err(Busy::Lease));
    assert_eq!(fx.registry.retire(&a), Ok(()));
    assert_eq!(fx.registry.retire(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    assert_eq!(view(&fx, "a").lease, Some(lease(1)));
    assert_eq!(started_on(&provider.start(spec(2))), Some("b"));
    assert_eq!(fx.registry.release_host(&a), Err(Busy::Lease));
    provider.stop(&lease(1));
    assert_eq!(fx.registry.release_host(&a), Err(Busy::Runs));
    beat_listing(&fx, "a", epoch, &[]);
    assert_eq!(fx.registry.release_host(&a), Ok(()));
    assert_eq!(fx.registry.retire(&a), Err(Busy::Releasing));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Releasing));
    assert_eq!(fx.registry.retire(&host("ghost")), Err(Busy::UnknownHost));
}

/// Asserts that a retired host goes on taking runs for the lease booked on it: the client's
/// next run under that lease is queued, the plain `drain` and a repeat `retire` of a retired
/// host change nothing (the runs still queue, the host still shows draining), and `undrain`
/// ends the retire. It catches a retire that cuts the lease in effect by refusing its runs,
/// and a repeat call that downgrades the retire to a plain drain.
#[test]
fn a_retired_host_takes_runs_for_its_booked_lease() {
    let fx = fixture(1);
    fx.join("a", "t");
    let a = host("a");
    let mut provider = fx.registry.provider();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    fx.registry.retire(&a).unwrap();
    assert!(fx.registry.queue_run(&a, &lease(1), job("r1")).is_ok());
    assert_eq!(fx.registry.drain(&a), Ok(()));
    assert_eq!(fx.registry.retire(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    assert!(fx.registry.queue_run(&a, &lease(1), job("r2")).is_ok());
    assert_eq!(fx.registry.undrain(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), None);
}

/// Asserts what a retired host still refuses: a run under a lease that is not booked on it
/// (`not-held`, whether the lease is another host's or none at all) and a new lease, which
/// goes to the other host and is refused `no-capacity` when no other host is left. It
/// catches a retire that lets any run or any lease through.
#[test]
fn a_retired_host_refuses_runs_for_other_leases_and_new_leases() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let (a, b) = (host("a"), host("b"));
    let mut provider = fx.registry.provider();
    fx.registry.retire(&a).unwrap();
    assert_eq!(started_on(&provider.start(spec(1))), Some("b"));
    let other = fx.registry.queue_run(&a, &lease(1), job("r1"));
    assert_eq!(other.unwrap_err(), RunRefusal::NotHeld);
    let none = fx.registry.queue_run(&a, &lease(9), job("r1"));
    assert_eq!(none.unwrap_err(), RunRefusal::NotHeld);
    fx.registry.retire(&b).unwrap();
    assert_eq!(
        provider.start(spec(2)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a releasing host takes no run even for a lease seated on it: the release
/// has begun. It catches the retire exception reaching the releasing mark.
#[test]
fn a_releasing_host_takes_no_run() {
    let fx = fixture(1);
    fx.join("a", "t");
    let a = host("a");
    fx.registry.retire(&a).unwrap();
    fx.registry.release_host(&a).unwrap();
    fx.provider.seat(&a, lease(1), None).unwrap();
    let refused = fx.registry.queue_run(&a, &lease(1), job("r1"));
    assert_eq!(refused.unwrap_err(), RunRefusal::NoCapacity);
}

/// Asserts that a host that lists a run is busy with no lease at all: `drain` answers
/// `Busy::Runs`, and works once a heartbeat lists none. It catches a quiet check that
/// reads only the lease table: a run the host still holds after its lease ended would be
/// cut.
#[test]
fn drain_refuses_a_host_that_lists_a_run() {
    let fx = fixture(1);
    let epoch = fx.join("a", "t");
    beat_listing(&fx, "a", epoch, &["r1"]);
    assert_eq!(fx.registry.drain(&host("a")), Err(Busy::Runs));
    assert_eq!(mark(&fx, "a"), None);
    beat_listing(&fx, "a", epoch, &[]);
    assert_eq!(fx.registry.drain(&host("a")), Ok(()));
}

/// Asserts the race the quiet rule exists for, with the registry's calls in the order a
/// scaler and an agent can produce: the host is drained quiet, then a heartbeat lists a
/// run (work the host had started that the registry could not see), then `release_host`
/// arrives. It is refused `Busy::Runs`, the host stays draining and is not released; when
/// the run leaves the list, the same call succeeds. It catches a release that trusts the
/// drain's earlier check.
#[test]
fn release_host_refuses_a_host_that_became_busy_after_the_drain() {
    let fx = fixture(1);
    let epoch = fx.join("a", "t");
    let a = host("a");
    fx.registry.drain(&a).unwrap();
    beat_listing(&fx, "a", epoch, &["r1"]);
    assert_eq!(fx.registry.release_host(&a), Err(Busy::Runs));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    beat_listing(&fx, "a", epoch, &[]);
    assert_eq!(fx.registry.release_host(&a), Ok(()));
    assert_eq!(mark(&fx, "a"), Some(HostMark::Releasing));
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Drain,
    ReportRun,
    StartLease,
    Release,
}

/// Every order of `ops`.
fn orders(ops: &[Op]) -> Vec<Vec<Op>> {
    if ops.len() <= 1 {
        return vec![ops.to_vec()];
    }
    let mut all = Vec::new();
    for at in 0..ops.len() {
        let mut rest = ops.to_vec();
        let first = rest.remove(at);
        for mut tail in orders(&rest) {
            tail.insert(0, first);
            all.push(tail);
        }
    }
    all
}

/// What the orders produced, so the test can see that each outcome happened.
#[derive(Default)]
struct Tally {
    drained: usize,
    drain_busy_lease: usize,
    drain_busy_runs: usize,
    released: usize,
    release_busy_runs: usize,
    release_not_draining: usize,
    started: usize,
    refused: usize,
}

/// Plays `order` on a pool of one host and checks the rule after every call: a drain or a
/// release that succeeded found the host with no lease and no run listed; a lease that
/// started found it unmarked; and the host never ends both releasing and leased.
fn play(order: &[Op], tally: &mut Tally) {
    let fx = fixture(1);
    let epoch = fx.join("a", "t");
    let a = host("a");
    let mut provider = fx.registry.provider();
    let mut run_listed = false;
    for op in order {
        let before = view(&fx, "a");
        let leased = before.lease.is_some();
        match op {
            Op::Drain => match fx.registry.drain(&a) {
                Ok(()) => {
                    assert!(!leased && !run_listed, "{order:?}: drained a busy host");
                    tally.drained += 1;
                }
                Err(Busy::Lease) => tally.drain_busy_lease += 1,
                Err(Busy::Runs) => tally.drain_busy_runs += 1,
                Err(other) => panic!("{order:?}: drain answered {other:?}"),
            },
            Op::ReportRun => {
                beat_listing(&fx, "a", epoch, &["r1"]);
                run_listed = true;
            }
            Op::StartLease => match provider.start(spec(1)) {
                StartAck::Started { .. } => {
                    assert_eq!(before.mark, None, "{order:?}: booked a marked host");
                    assert!(!run_listed, "{order:?}: booked a host with a run");
                    tally.started += 1;
                }
                _ => tally.refused += 1,
            },
            Op::Release => match fx.registry.release_host(&a) {
                Ok(()) => {
                    assert!(!leased && !run_listed, "{order:?}: released a busy host");
                    tally.released += 1;
                }
                Err(Busy::Runs) => tally.release_busy_runs += 1,
                Err(Busy::NotDraining) => tally.release_not_draining += 1,
                Err(other) => panic!("{order:?}: release answered {other:?}"),
            },
        }
    }
    let end = view(&fx, "a");
    let cut = end.mark == Some(HostMark::Releasing) && end.lease.is_some();
    assert!(!cut, "{order:?}: a releasing host holds a lease");
    assert_eq!(end.state, HostState::Live);
}

/// Asserts the design's interleavings (a lease placement against `drain`, against
/// `release_host`, a run reported against `release_host`) by playing all 24 orders of the
/// four calls on a pool of one host: in every order either the work runs and the host is
/// kept, or the host is released and the work is refused. The calls are atomic under the
/// registry's lock, so the orders are the interleavings; nothing depends on timing. The
/// tally shows that every outcome (drained, busy for a lease, busy for a run, released,
/// not draining, lease started, lease refused) happened, so the test cannot pass by never
/// reaching the rule.
#[test]
fn a_host_is_released_only_while_quiet_in_every_order() {
    let mut tally = Tally::default();
    let all = orders(&[Op::Drain, Op::ReportRun, Op::StartLease, Op::Release]);
    assert_eq!(all.len(), 24);
    for order in &all {
        play(order, &mut tally);
    }
    for (count, what) in [
        (tally.drained, "drained"),
        (tally.drain_busy_lease, "drain busy for a lease"),
        (tally.drain_busy_runs, "drain busy for a run"),
        (tally.released, "released"),
        (tally.release_busy_runs, "release busy for a run"),
        (tally.release_not_draining, "release of an unmarked host"),
        (tally.started, "lease started"),
        (tally.refused, "lease refused"),
    ] {
        assert!(count > 0, "no order produced: {what}");
    }
}

/// Asserts that `remove` drops only a releasing host: an unmarked or draining host is
/// `Busy::NotReleasing` and stays listed, an unknown host is `Ok`, and after the removal
/// the host is gone from the listing and from the bindings, its place in the machine limit
/// is free (a pool of one takes another host), and the same name can register again as an
/// unmarked host. It catches a remove that frees nothing, one that removes a host still in
/// use, and a mark that outlives the host.
#[test]
fn remove_drops_a_releasing_host_and_frees_its_place() {
    let fx = fixture(1);
    fx.join("a", "t");
    let a = host("a");
    assert_eq!(fx.registry.remove(&a), Err(Busy::NotReleasing));
    fx.registry.drain(&a).unwrap();
    assert_eq!(fx.registry.remove(&a), Err(Busy::NotReleasing));
    fx.registry.release_host(&a).unwrap();
    assert!(
        fx.hello(&hello("b"), &cred("t")).is_err(),
        "the pool of one is full"
    );
    assert_eq!(fx.registry.remove(&a), Ok(()));
    assert!(fx.registry.hosts().is_empty());
    assert!(fx.registry.bindings().is_empty());
    assert_eq!(fx.registry.remove(&a), Ok(()));
    assert_eq!(fx.registry.remove(&host("ghost")), Ok(()));
    fx.join("b", "t");
    let b = host("b");
    fx.registry.drain(&b).unwrap();
    fx.registry.release_host(&b).unwrap();
    fx.registry.remove(&b).unwrap();
    fx.join("a", "t");
    assert_eq!(mark(&fx, "a"), None);
}

/// Asserts that a mark outlives the host's session: a draining host that goes lost and
/// says hello again is still draining, and so is a releasing one. It catches a hello that
/// rebuilds the record without the mark, which would put a host the scaler is giving back
/// into service.
#[test]
fn a_mark_survives_a_new_hello() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    fx.registry.drain(&host("a")).unwrap();
    fx.registry.drain(&host("b")).unwrap();
    fx.registry.release_host(&host("b")).unwrap();
    fx.pass_grace();
    assert_eq!(view(&fx, "a").state, HostState::Lost);
    fx.join("a", "t");
    fx.join("b", "t");
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    assert_eq!(mark(&fx, "b"), Some(HostMark::Releasing));
}

/// Asserts that a retired host that says hello again is still retired and not downgraded to a
/// plain drain: after a silence past the grace and a new hello it shows draining, and a lease
/// seated on it still queues runs (a plainly drained host refuses them). It catches a hello
/// that keeps the mark's name and loses what it allows.
#[test]
fn a_new_hello_keeps_a_retired_host_retired() {
    let fx = fixture(1);
    fx.join("a", "t");
    let a = host("a");
    fx.registry.retire(&a).unwrap();
    fx.pass_grace();
    assert_eq!(view(&fx, "a").state, HostState::Lost);
    fx.join("a", "t");
    assert_eq!(mark(&fx, "a"), Some(HostMark::Draining));
    fx.provider.seat(&a, lease(1), None).unwrap();
    assert!(fx.registry.queue_run(&a, &lease(1), job("r1")).is_ok());
}

/// Asserts the two guards that matter if a booking ever lands on a marked host. The only
/// way to put one there is `seat`, the call that puts back a booking the server lost with
/// its process (the marks are in memory, so a restart clears them and the real order is the
/// other way round). With a booking seated on a draining host and on a releasing host, a run
/// is queued on neither (`no-capacity`, the same refusal as a host out of the serving
/// phase), and the same call is queued once the drain is undone; `remove` keeps the
/// releasing host (`Busy::Lease`) until its lease is stopped. It catches a run start that
/// reads the lease table and not the mark, and a remove that drops a booking with its host.
#[test]
fn a_booking_seated_on_a_marked_host_starts_no_run_and_blocks_remove() {
    let fx = fixture(2);
    fx.join("a", "t");
    fx.join("b", "t");
    let (a, b) = (host("a"), host("b"));
    fx.registry.drain(&a).unwrap();
    fx.registry.drain(&b).unwrap();
    fx.registry.release_host(&b).unwrap();
    fx.provider.seat(&a, lease(1), None).unwrap();
    fx.provider.seat(&b, lease(2), None).unwrap();
    assert_eq!(fx.registry.host_of(&lease(2)), Some(b.clone()));

    for (name, held) in [(&a, lease(1)), (&b, lease(2))] {
        let refused = fx.registry.queue_run(name, &held, job("r1"));
        assert_eq!(refused.unwrap_err(), RunRefusal::NoCapacity, "{name}");
    }
    fx.registry.undrain(&a).unwrap();
    assert!(fx.registry.queue_run(&a, &lease(1), job("r1")).is_ok());

    assert_eq!(fx.registry.remove(&b), Err(Busy::Lease));
    assert_eq!(fx.registry.host_of(&lease(2)), Some(b.clone()));
    let mut provider = fx.registry.provider();
    provider.stop(&lease(2));
    assert_eq!(fx.registry.remove(&b), Ok(()));
    assert_eq!(fx.registry.hosts().len(), 1);
}
