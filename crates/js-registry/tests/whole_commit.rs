//! The commit gate of a whole-node lease on a registered host: which host it commits to,
//! when the lease becomes visible, what is withheld until the agent reports ready, and
//! when the host books again. The registry is driven directly, on a manual clock.

mod support;

use js_agent_wire::{
    Command, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, Limits, Phase, Taint,
};
use js_pool_static::{EXAMPLE_CONFIG, StaticPool};
use js_provider::{Observed, Provider, RefusalCode, StartAck, StartSpec};
use js_registry::{PoolConfig, RegisteredProvider, RunRefusal, RunState};
use js_types::{Admission, ClassId, ProviderId, Request, ResourceClass, Resources};
use support::*;

/// The whole-node class of the pool under test.
fn whole() -> ResourceClass {
    ResourceClass::whole_node()
}

/// A pool whose class is `Whole`.
fn whole_fixture() -> Fixture {
    let name = ProviderId::new("builders").unwrap();
    fixture_of(PoolConfig::new(name, whole(), pool_shape(), 4))
}

/// A request for the whole-node class.
fn whole_request() -> Request {
    Request {
        class: whole().id().clone(),
        ..request()
    }
}

fn whole_spec(sequence: u64) -> StartSpec {
    StartSpec::new(lease(sequence), whole_request(), &[whole()]).unwrap()
}

/// A provider that serves a `BookAll` class (`share`) next to the `Whole` one.
fn mixed(fx: &Fixture) -> RegisteredProvider {
    let share = ResourceClass::new(ClassId::new("share").unwrap(), Admission::BookAll, 0);
    fx.registry.provider().with_classes(vec![share, whole()])
}

fn share_spec(sequence: u64) -> StartSpec {
    let request = Request {
        class: ClassId::new("share").unwrap(),
        ..request()
    };
    let known = [ResourceClass::new(
        ClassId::new("share").unwrap(),
        Admission::BookAll,
        0,
    )];
    StartSpec::new(lease(sequence), request, &known).unwrap()
}

fn no_capacity() -> StartAck {
    StartAck::Refused(RefusalCode::NoCapacity)
}

/// A heartbeat of `host` in `epoch`, changed by `edit`, answered.
fn beat_with(
    fx: &Fixture,
    host: &str,
    epoch: u64,
    edit: impl FnOnce(&mut js_agent_wire::Heartbeat),
) -> HeartbeatReply {
    let mut message = fx.heartbeat(host, epoch);
    edit(&mut message);
    fx.registry.heartbeat(&message, &cred("t")).unwrap()
}

/// The agent reports `lease` ready.
fn report_ready(fx: &Fixture, epoch: u64, lease_id: &str) -> HeartbeatReply {
    beat_with(fx, "a", epoch, |b| {
        b.exclusive_ready = Some(lease_id.to_string());
    })
}

fn job(run: &str) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            disk_bytes: None,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: 0,
        bundle: None,
        native: None,
    }
}

fn running(run: &str) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state: HeldState::Running,
    }
}

fn starts(reply: &HeartbeatReply) -> usize {
    let starts = reply.commands.iter();
    starts
        .filter(|command| matches!(command, Command::Start(_)))
        .count()
}

/// A pool with host `a` joined, lease 1 committed to it. Returns the fixture, the
/// provider and the epoch.
fn committed() -> (Fixture, RegisteredProvider, u64) {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = fx.provider.clone();
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    (fx, provider, epoch)
}

/// As `committed`, then the agent reports the host ready.
fn ready() -> (Fixture, RegisteredProvider, u64) {
    let (fx, provider, epoch) = committed();
    report_ready(&fx, epoch, &lease(1).to_string());
    (fx, provider, epoch)
}

/// Asserts the commit: a `Whole` start on a quiet, serving, live host answers `Unknown`
/// (the table keeps the lease pending), books the host, and every heartbeat reply to
/// that host names the lease in `exclusive`; a heartbeat in phase `preparing` is
/// accepted and keeps the host out of reach; and the lease reads `Unknown`, never
/// `Present`, before the report (the token is minted only on `Present`). A repeated
/// start answers `Unknown` too, books nothing more. It catches a commit that answers
/// `Started` (the token before the host is ready) and an `observe` that confirms early.
#[test]
fn a_whole_start_commits_and_stays_unconfirmed_until_the_report() {
    let (fx, mut provider, epoch) = committed();
    assert_eq!(lease(1).to_string(), "1-1");
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(1)));
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    let view = &fx.registry.hosts()[0];
    assert_eq!(view.lease, Some(lease(1)));
    let reply = fx.registry.heartbeat(&fx.heartbeat("a", epoch), &cred("t"));
    assert_eq!(reply.unwrap().exclusive.as_deref(), Some("1-1"));
    let preparing = beat_with(&fx, "a", epoch, |b| b.phase = Phase::Preparing);
    assert_eq!(preparing.exclusive.as_deref(), Some("1-1"));
    assert_eq!(fx.registry.hosts()[0].phase, Phase::Preparing);
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
}

/// Asserts the ready report: it flips the lease to `Present` and a repeated start to
/// `Started`; a report for another lease id changes nothing, and neither does one from a
/// host that reports a taint in the same heartbeat (a ready host is clean). The reply
/// keeps naming the lease after the flip, so an agent that restarts is told again. It
/// catches a report taken for any lease and a token minted on a tainted host.
#[test]
fn only_the_named_lease_reported_ready_by_a_clean_host_is_confirmed() {
    let (fx, mut provider, epoch) = committed();
    report_ready(&fx, epoch, &lease(9).to_string());
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    let tainted = beat_with(&fx, "a", epoch, |b| {
        b.exclusive_ready = Some(lease(1).to_string());
        b.taint = Some(Taint {
            reason: "check".into(),
            run_id: None,
            check: None,
        });
    });
    assert!(tainted.taint_ack);
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert!(fx.registry.hosts()[0].taint.is_some());
    // A clean host, on a fresh pool.
    let (fx, mut provider, epoch) = committed();
    let reply = report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(reply.exclusive.as_deref(), Some("1-1"));
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    let again = StartAck::Started { host: host("a") };
    assert_eq!(provider.start(whole_spec(1)), again);
    let later = fx.registry.heartbeat(&fx.heartbeat("a", epoch), &cred("t"));
    assert_eq!(later.unwrap().exclusive.as_deref(), Some("1-1"));
}

/// Asserts that a host tainted in an earlier heartbeat does not take the ready report for
/// its committed lease in a later one, and that the lease stays unobserved. It catches a
/// ready report taken without the check that the host is clean.
#[test]
fn a_host_tainted_earlier_does_not_become_ready() {
    let (fx, mut provider, epoch) = committed();
    let tainted = beat_with(&fx, "a", epoch, |b| {
        b.taint = Some(Taint {
            reason: "check".into(),
            run_id: None,
            check: None,
        });
    });
    assert!(tainted.taint_ack);
    report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
}

/// Asserts the run gate on the queue: before the report a run is refused `NotReady`
/// (code `not-ready`), not `NoCapacity` even when the host announced `preparing`; after
/// it the run is queued and the next reply starts it. It catches a run queued on a host
/// that has not finished preparing.
#[test]
fn no_run_is_queued_before_ready() {
    let (fx, _provider, epoch) = committed();
    let queue = |run: &str| fx.registry.queue_run(&host("a"), &lease(1), job(run));
    assert_eq!(queue("r1").unwrap_err(), RunRefusal::NotReady);
    assert_eq!(RunRefusal::NotReady.code(), "not-ready");
    beat_with(&fx, "a", epoch, |b| b.phase = Phase::Preparing);
    assert_eq!(queue("r1").unwrap_err(), RunRefusal::NotReady);
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
    report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(queue("r1").unwrap().state, RunState::Queued);
    let reply = beat_with(&fx, "a", epoch, |_| {});
    assert_eq!(starts(&reply), 1);
}

/// Asserts the restore of a whole-node booking after a restart: a lease seated on a host
/// that is not known yet (an awaiting host) and one seated on a joined host are both
/// committed, never free. They read `Unknown` and are named in `exclusive` once the host
/// speaks, no run is queued before the agent reports ready for exactly that lease, and the
/// report makes the lease `Present`. It catches a restored booking that reads `Present`
/// with no ready report, or a restored host that other leases or runs could use.
#[test]
fn a_restored_whole_booking_is_committed_until_the_agent_reports_ready() {
    let fx = whole_fixture();
    let mut provider = fx.provider.clone();
    provider.seat(&host("a"), lease(1), None).unwrap();
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    let epoch = fx.join("a", "t");
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    let reply = fx.registry.heartbeat(&fx.heartbeat("a", epoch), &cred("t"));
    assert_eq!(reply.unwrap().exclusive.as_deref(), Some("1-1"));
    let queue = |run: &str| fx.registry.queue_run(&host("a"), &lease(1), job(run));
    assert_eq!(queue("r1").unwrap_err(), RunRefusal::NotReady);
    report_ready(&fx, epoch, &lease(1).to_string());
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    assert_eq!(queue("r1").unwrap().state, RunState::Queued);

    // A host that already joined takes the restored lease the same way.
    let fx = whole_fixture();
    let mut provider = fx.provider.clone();
    let epoch = fx.join("b", "t");
    provider.seat(&host("b"), lease(2), None).unwrap();
    assert_eq!(provider.observe(&lease(2)), Observed::Unknown);
    let reply = fx.registry.heartbeat(&fx.heartbeat("b", epoch), &cred("t"));
    assert_eq!(reply.unwrap().exclusive.as_deref(), Some("1-2"));

    // A pool that books the whole host without the commit gate restores its leases as
    // they were: present, with nothing to prepare.
    let fx = fixture(2);
    let mut provider = fx.provider.clone();
    let epoch = fx.join("c", "t");
    provider.seat(&host("c"), lease(3), None).unwrap();
    let seen = Observed::Present { host: host("c") };
    assert_eq!(provider.observe(&lease(3)), seen);
    let reply = fx.registry.heartbeat(&fx.heartbeat("c", epoch), &cred("t"));
    assert_eq!(reply.unwrap().exclusive, None);
}

/// Asserts that a whole-node booking put back on a host that lists held runs is ready,
/// not committed: the lease reads `Present` and its consumer's run is queued (not
/// refused `not-ready`), because only a ready lease could have runs on the host. A
/// host that lists none is committed (the tests above and below). It catches a restart
/// that refuses the consumer of a ready lease until its runs end.
#[test]
fn a_restored_whole_booking_with_live_runs_is_ready() {
    let fx = whole_fixture();
    let mut provider = fx.provider.clone();
    let epoch = fx.join("a", "t");
    beat_with(&fx, "a", epoch, |b| b.held = vec![running("r0")]);
    provider.seat(&host("a"), lease(1), None).unwrap();
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    let reply = beat_with(&fx, "a", epoch, |b| b.held = vec![running("r0")]);
    assert_eq!(reply.exclusive.as_deref(), Some("1-1"));
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert_eq!(queued.unwrap().state, RunState::Queued);

    // The same restore on a host that lists no run is committed.
    let fx = whole_fixture();
    let mut provider = fx.provider.clone();
    let epoch = fx.join("a", "t");
    beat_with(&fx, "a", epoch, |_| {});
    provider.seat(&host("a"), lease(1), None).unwrap();
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    let refused = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert_eq!(refused.unwrap_err(), RunRefusal::NotReady);
}

/// Asserts that a ready report moves only a committed host: a report from a host that
/// holds no lease (naming another host's lease), a report for a lease that already
/// reads ready (the same or another id), and a report for a lease that ended each
/// change nothing, and each heartbeat is answered, not refused. It catches a report
/// taken for a lease the reporting host does not hold.
#[test]
fn a_ready_report_moves_only_a_committed_host() {
    let (fx, mut provider, epoch) = committed();
    let other = fx.join("b", "t");
    let reply = beat_with(&fx, "b", other, |b| {
        b.exclusive_ready = Some(lease(1).to_string());
    });
    assert_eq!(reply.exclusive, None);
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert!(!provider.booked().contains_key(&host("b")));

    // Reported again, and for another lease, after it is ready: still ready.
    report_ready(&fx, epoch, &lease(1).to_string());
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    report_ready(&fx, epoch, &lease(1).to_string());
    let reply = report_ready(&fx, epoch, &lease(9).to_string());
    assert_eq!(reply.exclusive.as_deref(), Some("1-1"));
    assert_eq!(provider.observe(&lease(1)), seen);

    // Reported for a lease that ended: the host is restoring, never ready again.
    provider.stop(&lease(1));
    let reply = report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(reply.exclusive, None);
    assert_eq!(provider.observe(&lease(1)), Observed::Absent);
    let reply = report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(reply.exclusive, None);
    assert!(provider.booked().is_empty());
}

/// Asserts that stopping a lease the registry never booked (a whole-node request that
/// was refused, then released) reads `Absent` and leaves the hosts as they were.
#[test]
fn stopping_an_unbooked_lease_changes_no_host() {
    let (fx, mut provider, _epoch) = committed();
    let stopped = provider.stop(&lease(7));
    assert!(matches!(stopped, js_provider::StopAck::Stopped { .. }));
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(1)));
    assert!(!fx.registry.hosts()[0].releasing);
}

/// Asserts that a host that says hello again lost its session and its preparation: a
/// ready lease reads `Unknown` again (it keeps its booking) until the new session
/// reports ready, and a run queued while it was ready is given up as never started by
/// the hello, not started on the new session. It catches a ready state that survives a
/// restart of the agent.
#[test]
fn a_new_session_must_report_ready_again() {
    let (fx, mut provider, _epoch) = ready();
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert!(queued.is_ok());
    fx.pass_grace();
    let epoch = fx.hello(&hello("a"), &cred("t")).unwrap().epoch;
    assert_eq!(provider.observe(&lease(1)), Observed::Unknown);
    assert_eq!(provider.booked().get(&host("a")), Some(&lease(1)));
    let runs = fx.registry.runs_of(&lease(1));
    assert_eq!(runs[0].state, RunState::NeverStarted);
    let refused = fx.registry.queue_run(&host("a"), &lease(1), job("r2"));
    assert_eq!(refused.unwrap_err(), RunRefusal::NotReady);
    report_ready(&fx, epoch, &lease(1).to_string());
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
}

/// Asserts exclusivity (W1): while a whole-node lease holds a host, in either state, no
/// other lease books it, whole or shared, and a second whole start finds no capacity.
/// It catches a second booking of a committed host.
#[test]
fn a_committed_host_is_booked_by_nobody_else() {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = mixed(&fx);
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
    assert_eq!(provider.start(whole_spec(2)), no_capacity());
    assert_eq!(provider.start(share_spec(3)), no_capacity());
    report_ready(&fx, epoch, &lease(1).to_string());
    assert_eq!(provider.start(whole_spec(2)), no_capacity());
    assert_eq!(provider.start(share_spec(3)), no_capacity());
    assert_eq!(provider.booked().len(), 1);
}

/// Asserts the quiet rule: a host that lists a run it holds is not committed to, even
/// serving, live and clean (the answer is `NoCapacity` and nothing is booked); the same
/// host commits once a heartbeat lists none. It catches a commit on a host that is still
/// busy with the last lease's work.
#[test]
fn a_host_that_is_not_quiet_is_not_committed_to() {
    let fx = whole_fixture();
    let epoch = fx.join("a", "t");
    let mut provider = fx.provider.clone();
    beat_with(&fx, "a", epoch, |b| b.held = vec![running("old")]);
    assert_eq!(provider.start(whole_spec(1)), no_capacity());
    assert!(provider.booked().is_empty());
    beat_with(&fx, "a", epoch, |_| {});
    assert_eq!(provider.start(whole_spec(1)), StartAck::Unknown);
}

/// Asserts the end of a lease (W9): a stop frees the booking and reads `Absent`, and the
/// host books again only after a heartbeat that follows a reply without `exclusive` and
/// shows it serving with nothing held. A heartbeat that crossed the stop (serving,
/// empty), one in phase `restoring`, and one serving with a run held each leave it
/// unbookable (shown as `releasing`); the next serving, empty one frees it. The same
/// holds when the lease ended before the report. It catches a host booked while the
/// agent still restores, and one booked on the stale heartbeat that preceded the stop.
#[test]
fn a_host_books_again_only_after_it_is_serving_and_empty_following_the_stop() {
    for reported in [true, false] {
        let (fx, mut provider, epoch) = committed();
        if reported {
            report_ready(&fx, epoch, &lease(1).to_string());
        }
        let stopped = provider.stop(&lease(1));
        assert!(matches!(stopped, js_provider::StopAck::Stopped { .. }));
        assert_eq!(provider.observe(&lease(1)), Observed::Absent);
        assert!(fx.registry.hosts()[0].releasing);
        assert_eq!(provider.start(whole_spec(2)), no_capacity());
        let crossed = beat_with(&fx, "a", epoch, |_| {});
        assert_eq!(crossed.exclusive, None);
        assert_eq!(provider.start(whole_spec(2)), no_capacity());
        beat_with(&fx, "a", epoch, |b| b.phase = Phase::Restoring);
        assert_eq!(provider.start(whole_spec(2)), no_capacity());
        beat_with(&fx, "a", epoch, |b| b.held = vec![running("r1")]);
        assert_eq!(provider.start(whole_spec(2)), no_capacity());
        assert!(fx.registry.hosts()[0].releasing);
        beat_with(&fx, "a", epoch, |_| {});
        assert!(!fx.registry.hosts()[0].releasing);
        assert_eq!(provider.start(whole_spec(2)), StartAck::Unknown);
    }
}

/// Asserts that a host that says hello during the restore starts the wait again (the
/// new session has not been told), and that a tainted host never books again, however
/// quiet it is. It catches a restore cleared by a hello and a taint forgotten by it.
#[test]
fn a_hello_restarts_the_restore_and_a_taint_ends_booking() {
    let (fx, mut provider, epoch) = ready();
    provider.stop(&lease(1));
    beat_with(&fx, "a", epoch, |_| {});
    fx.pass_grace();
    let epoch = fx.hello(&hello("a"), &cred("t")).unwrap().epoch;
    beat_with(&fx, "a", epoch, |_| {});
    assert_eq!(provider.start(whole_spec(2)), no_capacity());
    beat_with(&fx, "a", epoch, |b| {
        b.taint = Some(Taint {
            reason: "check".into(),
            run_id: None,
            check: None,
        });
    });
    for _ in 0..2 {
        beat_with(&fx, "a", epoch, |_| {});
        // The only host is tainted, so no host could ever be given whole.
        let refused = StartAck::Refused(RefusalCode::NoCapableHost);
        assert_eq!(provider.start(whole_spec(2)), refused);
    }
}

/// Asserts the regression: a `BookAll` lease on a registered host is granted at once
/// (`Started`, `Present`), its reply names no `exclusive`, its runs are queued without a
/// report, and the host books again at the first serving heartbeat after the stop, with
/// no restore to wait for. It catches a gate that leaks into the shared class.
#[test]
fn a_bookall_lease_is_unchanged() {
    let fx = fixture(4);
    let epoch = fx.join("a", "t");
    let mut provider = fx.provider.clone();
    assert_eq!(started_on(&provider.start(spec(1))), Some("a"));
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert!(queued.is_ok());
    let reply = beat_with(&fx, "a", epoch, |_| {});
    assert_eq!((starts(&reply), reply.exclusive.as_deref()), (1, None));
    provider.stop(&lease(1));
    beat_with(&fx, "a", epoch, |_| {});
    beat_with(&fx, "a", epoch, |_| {});
    assert_eq!(started_on(&provider.start(spec(2))), Some("a"));
}

/// Documents the static pool: it has no agent to wait for, so a `Whole` lease (the
/// `whole-node` class) is held at once and reads `Present` on its first look.
#[test]
fn a_static_pool_holds_a_whole_lease_at_once() {
    let id = ProviderId::new("pool").unwrap();
    let class = ResourceClass::whole_node();
    let pool = StaticPool::from_toml(id, EXAMPLE_CONFIG).unwrap();
    let mut pool = pool.with_classes(vec![class.clone()]);
    assert_eq!(class.admission(), &Admission::Whole);
    let request = Request {
        class: class.id().clone(),
        resources: Resources::new(1000, 1, 1, Some("nvidia-a100".into())).unwrap(),
        capabilities: Default::default(),
        ..request()
    };
    let spec = StartSpec::new(lease(1), request, &[class]).unwrap();
    let StartAck::Started { host } = pool.start(spec) else {
        panic!("a static pool grants a whole lease at once");
    };
    assert_eq!(pool.observe(&lease(1)), Observed::Present { host });
}

/// Asserts the regression the gate must not cause: `gpu-host` has not opted into the quiet
/// gate (it is admitted `BookAll`), so on a registered host its start is `Started` at once
/// and reads `Present`, no heartbeat reply names an `exclusive`, a run is queued without
/// a report and started by the next reply. It fails if the gate is applied to `gpu-host`, which would
/// leave every real gpu-host lease pending for want of an agent that reports ready.
#[test]
fn a_gpu_host_lease_is_started_at_once_without_the_gate() {
    let class = ResourceClass::gpu_host();
    let mut hello = hello("a");
    hello.shape.gpus = 1;
    hello.shape.gpu_class = Some("a100".into());
    let floor = Resources::new(4000, 8 << 30, 1, Some("a100".into())).unwrap();
    let shape = js_types::Shape::new(floor, [("os".to_string(), "macos".to_string())].into());
    let name = ProviderId::new("builders").unwrap();
    let fx = fixture_of(PoolConfig::new(name, class.clone(), shape, 4));
    let epoch = fx.hello(&hello, &cred("t")).unwrap().epoch;
    let request = Request {
        class: class.id().clone(),
        resources: Resources::new(1000, 1, 1, Some("a100".into())).unwrap(),
        ..request()
    };
    let spec = StartSpec::new(lease(1), request, &[class]).unwrap();
    let mut provider = fx.provider.clone();
    assert_eq!(started_on(&provider.start(spec)), Some("a"));
    let seen = Observed::Present { host: host("a") };
    assert_eq!(provider.observe(&lease(1)), seen);
    let reply = beat_with(&fx, "a", epoch, |_| {});
    assert_eq!(reply.exclusive, None);
    let queued = fx.registry.queue_run(&host("a"), &lease(1), job("r1"));
    assert_eq!(queued.unwrap().state, RunState::Queued);
    let reply = beat_with(&fx, "a", epoch, |_| {});
    assert_eq!((starts(&reply), reply.exclusive), (1, None));
}
