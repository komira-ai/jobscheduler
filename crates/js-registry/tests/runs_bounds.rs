//! What bounds a run in time and what a tainted host is sent: the lease time a start
//! carries at each send, a run whose time is up, the rules of releasing, and the taint a
//! host reports.

mod support;

use std::time::Duration;

use js_agent_wire::{
    Ack, Command, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, Limits, Outcome,
    RunResult, Taint, refusal,
};
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{Binding, MIN_LOST_RETENTION_MS, RunRefusal, RunState, RunView};
use support::*;

/// A job of one core with a lease bound of `valid` milliseconds (0 for none) and a wall
/// limit of `wall`.
fn job(run: &str, valid: u64, wall: u64) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: 1000,
            memory_bytes: 1 << 20,
            pids: None,
            wall_ms: wall,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        files: Vec::new(),
        valid_for_ms: valid,
    }
}

fn held(run: &str, state: HeldState) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state,
    }
}

fn leased() -> (Fixture, u64) {
    let fx = fixture(4);
    let epoch = fx.join("a", "t");
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
    (fx, epoch)
}

fn queue(fx: &Fixture, spec: JobSpec) -> Result<RunView, RunRefusal> {
    fx.registry.queue_run(&host("a"), &lease(1), spec)
}

fn beat(fx: &Fixture, epoch: u64, listed: &[HeldRun]) -> HeartbeatReply {
    let mut message = fx.heartbeat("a", epoch);
    message.held = listed.to_vec();
    fx.registry.heartbeat(&message, &cred("t")).unwrap()
}

fn report(reason: &str) -> Taint {
    Taint {
        reason: reason.into(),
        run_id: None,
        check: None,
    }
}

/// A heartbeat carrying a taint report, answered.
fn beat_tainted(fx: &Fixture, epoch: u64, listed: &[HeldRun]) -> HeartbeatReply {
    let mut message = fx.heartbeat("a", epoch);
    message.held = listed.to_vec();
    message.taint = Some(report("residue"));
    fx.registry.heartbeat(&message, &cred("t")).unwrap()
}

fn words(reply: &HeartbeatReply) -> Vec<String> {
    let word = |command: &Command| match command {
        Command::Start(spec) => format!("start {}", spec.run_id),
        Command::Stop { run_id } => format!("stop {run_id}"),
        Command::Cancel { run_id } => format!("cancel {run_id}"),
        Command::Unknown => "unknown".into(),
    };
    reply.commands.iter().map(word).collect()
}

fn state_of(fx: &Fixture, run: &str) -> RunState {
    let runs = fx.registry.runs_of(&lease(1));
    runs.into_iter().find(|v| v.run_id == run).unwrap().state
}

fn starts(reply: &HeartbeatReply) -> Vec<JobSpec> {
    let pick = |command: &Command| match command {
        Command::Start(spec) => Some((**spec).clone()),
        _ => None,
    };
    reply.commands.iter().filter_map(pick).collect()
}

fn at(fx: &Fixture, millis: u64) {
    fx.clock.advance(Duration::from_millis(millis));
}

/// Asserts that every start is made from the lease time left when it is sent: queued with
/// 10 s left, the start carries 10 s of validity and a wall limit cut to 10 s; sent again
/// 4 s later it carries 6 s of each; and a wall limit asked below the time left is kept.
/// It catches validity and wall time armed once at queue time, which would let a start
/// that reaches the agent late run past the end of its lease.
#[test]
fn a_start_is_cut_to_the_lease_time_left_at_each_send() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 10_000, 60_000)).unwrap();
    queue(&fx, job("r2", 10_000, 2_000)).unwrap();
    let first = starts(&beat(&fx, epoch, &[]));
    assert_eq!(first.len(), 2);
    assert_eq!(
        (first[0].valid_for_ms, first[0].limits.wall_ms),
        (10_000, 10_000)
    );
    assert_eq!(
        (first[1].valid_for_ms, first[1].limits.wall_ms),
        (10_000, 2_000)
    );
    at(&fx, 4_000);
    let later = starts(&beat(&fx, epoch, &[]));
    assert_eq!(
        (later[0].valid_for_ms, later[0].limits.wall_ms),
        (6_000, 6_000)
    );
    assert_eq!(
        (later[1].valid_for_ms, later[1].limits.wall_ms),
        (6_000, 2_000)
    );
}

/// Asserts that a run with no bound is sent as it was queued, however late. It catches a
/// recomputation that turns "no bound" into a start with no time.
#[test]
fn a_run_without_a_bound_is_sent_as_queued() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    at(&fx, 4_000);
    let sent = starts(&beat(&fx, epoch, &[]));
    assert_eq!((sent[0].valid_for_ms, sent[0].limits.wall_ms), (0, 60_000));
}

/// Asserts that no start goes out once the lease time is up: the reply at that instant
/// carries a stop for the run (so the agent retires the id) and no start, the run reads
/// stopping, the next reply that does not list it gives it up as never started, and its
/// limits are back. It catches a start delivered after the lease it ran under, and a
/// queued run the agent skipped holding its budget for as long as the lease is renewed.
#[test]
fn a_run_past_its_bound_is_stopped_then_given_up() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 10_000, 60_000)).unwrap();
    assert_eq!(words(&beat(&fx, epoch, &[])), ["start r1"]);
    at(&fx, 9_999);
    assert_eq!(words(&beat(&fx, epoch, &[])), ["start r1"]);
    at(&fx, 1);
    let reply = beat(&fx, epoch, &[]);
    assert_eq!(words(&reply), ["stop r1"]);
    assert_eq!(state_of(&fx, "r1"), RunState::Stopping);
    let reply = beat(&fx, epoch, &[]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::NeverStarted);
    let mut big = job("r2", 0, 60_000);
    big.limits.cpu_millis = 4000;
    assert!(queue(&fx, big).is_ok());
}

/// Asserts that the bound gives up only a run the host has not listed: a run the host
/// lists before its time is up is left to run. It catches a lapse that stops work the
/// agent has already started.
#[test]
fn a_listed_run_is_not_stopped_at_its_bound() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 10_000, 60_000)).unwrap();
    beat(&fx, epoch, &[]);
    beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    at(&fx, 10_000);
    let reply = beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::Running);
}

/// Asserts that a queued run the host never lists keeps the host releasing after the
/// lease ends: the host is not quiet while a run is live on the server's books although
/// the host lists nothing, so a new lease is refused. It catches a quiet check that reads
/// the host's list alone, which would hand the machine over under a start still in
/// flight.
#[test]
fn a_queued_run_never_listed_keeps_the_host_releasing() {
    let (fx, _epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    fx.provider.clone().stop(&lease(1));
    assert!(fx.registry.hosts()[0].releasing);
    let refused = fx.provider.clone().start(spec(2));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
}

/// Asserts that a host that says hello after it was lost, and lists nothing, gives up the
/// run queued for it, and that no start is sent for it afterwards. It catches a hello that
/// leaves a queued run to be started on a host that restarted and forgot it.
#[test]
fn a_hello_after_a_loss_gives_up_the_queued_run() {
    let (fx, _epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    fx.pass_grace();
    let epoch = fx.hello(&hello("a"), &cred("t")).unwrap().epoch;
    assert_eq!(state_of(&fx, "r1"), RunState::NeverStarted);
    assert!(beat(&fx, epoch, &[]).commands.is_empty());
}

/// Asserts that a host holding a lease is not releasing although runs are live on it. It
/// catches a releasing flag that is true when either the booking or the quiet check
/// says so, which would show a leased host as releasing.
#[test]
fn a_booked_host_with_live_runs_is_not_releasing() {
    let (fx, epoch) = leased();
    assert!(!fx.registry.hosts()[0].releasing);
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    let listing = &fx.registry.hosts()[0];
    assert!(listing.lease.is_some());
    assert!(!listing.releasing);
}

/// Asserts that a run asked to stop that has a result held reads `exited`, and one asked
/// to stop with no result yet reads `stopping`. It catches a state that says the run is
/// still being stopped after its container has exited and its result is in.
#[test]
fn a_stopped_run_with_a_result_reads_exited() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let exited = [held("r1", HeldState::ExitedUnacked)];
    beat(&fx, epoch, &exited);
    assert_eq!(state_of(&fx, "r1"), RunState::Stopping);
    let mut message = fx.heartbeat("a", epoch);
    message.held = exited.to_vec();
    message.results = vec![RunResult {
        run_id: "r1".into(),
        lease_id: lease(1).to_string(),
        outcome: Outcome::Killed,
        exit_code: None,
        detail: None,
    }];
    let reply = fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(reply.result_acks[0].ack, Ack::Accepted);
    assert_eq!(state_of(&fx, "r1"), RunState::Exited);
}

/// Asserts what a taint report does: it is acknowledged in the reply that carries it, the
/// host lists it, a run queued before is not started, a new run is refused
/// `host-tainted`, and a host that did not report one is told `taint_ack` false. It
/// catches a report that is read and ignored, and an acknowledgement for no report.
#[test]
fn a_taint_report_is_acknowledged_and_stops_new_work() {
    let (fx, epoch) = leased();
    let clean = beat(&fx, epoch, &[]);
    assert!(!clean.taint_ack);
    assert!(fx.registry.hosts()[0].taint.is_none());
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    let reply = beat_tainted(&fx, epoch, &[]);
    assert!(reply.taint_ack);
    assert!(starts(&reply).is_empty());
    assert_eq!(fx.registry.hosts()[0].taint, Some(report("residue")));
    let refused = queue(&fx, job("r2", 0, 60_000)).unwrap_err();
    assert_eq!(refused, RunRefusal::HostTainted);
    assert_eq!(refused.code(), refusal::HOST_TAINTED);
    let again = beat(&fx, epoch, &[]);
    assert!(starts(&again).is_empty());
    assert!(!again.taint_ack);
    assert_eq!(state_of(&fx, "r1"), RunState::Queued);
}

/// Asserts that the first report is the one kept: a later report with another reason is
/// acknowledged and changes nothing. It catches a host that rewrites the record of why it
/// was tainted.
#[test]
fn the_first_taint_is_kept() {
    let (fx, epoch) = leased();
    beat_tainted(&fx, epoch, &[]);
    let mut message = fx.heartbeat("a", epoch);
    message.taint = Some(report("other"));
    let reply = fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert!(reply.taint_ack);
    assert_eq!(fx.registry.hosts()[0].taint, Some(report("residue")));
}

/// Asserts that a tainted host's live runs finish: the run it holds is still stopped when
/// asked, its result is accepted, and it ends and returns its limits as on any host. It
/// catches a taint that strands the work already on the host.
#[test]
fn a_tainted_hosts_live_runs_finish_and_are_acknowledged() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    beat_tainted(&fx, epoch, &[held("r1", HeldState::Running)]);
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let reply = beat(&fx, epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(words(&reply), ["stop r1"]);
    let mut message = fx.heartbeat("a", epoch);
    message.held = vec![held("r1", HeldState::ExitedUnacked)];
    message.results = vec![RunResult {
        run_id: "r1".into(),
        lease_id: lease(1).to_string(),
        outcome: Outcome::Killed,
        exit_code: None,
        detail: None,
    }];
    let reply = fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert_eq!(reply.result_acks[0].ack, Ack::Accepted);
    beat(&fx, epoch, &[]);
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
}

/// Asserts that a start refused as `host-tainted` taints the host without any report: the
/// result is accepted like any other, the host lists a taint naming the run, a run queued
/// meanwhile is not started, and a new run is refused. A `failed_to_start` with another
/// detail, and a result with that detail but another outcome, taint nothing. It catches a
/// host whose agent cannot write its marker being sent work again.
#[test]
fn a_start_refused_as_host_tainted_taints_the_host() {
    let (fx, epoch) = leased();
    queue(&fx, job("r1", 0, 60_000)).unwrap();
    queue(&fx, job("r2", 0, 60_000)).unwrap();
    beat(&fx, epoch, &[]);
    let result = |run: &str, detail: &str, outcome| RunResult {
        run_id: run.into(),
        lease_id: lease(1).to_string(),
        outcome,
        exit_code: None,
        detail: Some(detail.into()),
    };
    let both = [
        held("r1", HeldState::ExitedUnacked),
        held("r2", HeldState::ExitedUnacked),
    ];
    for other in [
        result("r1", "no-capacity", Outcome::FailedToStart),
        result("r1", refusal::HOST_TAINTED, Outcome::Killed),
    ] {
        let mut message = fx.heartbeat("a", epoch);
        message.held = both.to_vec();
        message.results = vec![other];
        fx.registry.heartbeat(&message, &cred("t")).unwrap();
        assert!(fx.registry.hosts()[0].taint.is_none());
    }
    let mut message = fx.heartbeat("a", epoch);
    message.held = both.to_vec();
    message.results = vec![result("r2", refusal::HOST_TAINTED, Outcome::FailedToStart)];
    let reply = fx.registry.heartbeat(&message, &cred("t")).unwrap();
    assert!(!reply.taint_ack);
    assert_eq!(reply.result_acks[0].ack, Ack::Accepted);
    let taint = fx.registry.hosts()[0].taint.clone().unwrap();
    assert_eq!(taint.reason, refusal::HOST_TAINTED);
    assert_eq!(taint.run_id.as_deref(), Some("r2"));
    let refused = queue(&fx, job("r3", 0, 60_000)).unwrap_err();
    assert_eq!(refused, RunRefusal::HostTainted);
}

/// Asserts that a tainted host takes no new lease while a clean one beside it does: the
/// lease already on the tainted host stays held on it, and once that lease is stopped a
/// request that only the tainted host could serve gets no-capacity, while the clean host
/// is leased; both hosts still count toward the pool's limit. It catches a tainted host
/// handed to the next client.
#[test]
fn a_tainted_host_is_not_leased_and_a_clean_one_is() {
    let fx = fixture(2);
    let epoch = fx.join("a", "t");
    fx.join("b", "t");
    assert_eq!(started_on(&fx.provider.clone().start(spec(1))), Some("a"));
    beat_tainted(&fx, epoch, &[]);
    assert_eq!(fx.registry.hosts()[0].lease, Some(lease(1)));
    assert_eq!(started_on(&fx.provider.clone().start(spec(2))), Some("b"));
    assert_eq!(
        fx.provider.clone().start(spec(3)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    fx.provider.clone().stop(&lease(1));
    assert_eq!(
        fx.provider.clone().start(spec(3)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
    fx.provider.clone().stop(&lease(2));
    assert_eq!(started_on(&fx.provider.clone().start(spec(4))), Some("b"));
    assert_eq!(fx.registry.hosts().len(), 2);
    assert_eq!(
        fx.provider.clone().start(spec(5)),
        StartAck::Refused(RefusalCode::NoCapacity)
    );
}

/// Asserts that a taint is part of what is persisted and comes back: the bindings carry
/// it, a registry started over them puts a tainted host back awaiting (without a lease),
/// a binding without a taint puts nothing back, a host already known is left as it is,
/// the host keeps the taint through its hello, and it is not forgotten however long it is
/// silent. It catches a restart that makes a tainted host clean and a silent tainted host
/// that is forgotten into a clean one.
#[test]
fn a_taint_survives_what_is_persisted() {
    let config = config(2)
        .with_lost_retention(MIN_LOST_RETENTION_MS)
        .unwrap();
    let before = fixture_of(config.clone());
    before.join("a", "t");
    before.join("b", "t");
    let mut message = before.heartbeat("a", 1);
    message.taint = Some(report("residue"));
    before.registry.heartbeat(&message, &cred("t")).unwrap();
    let saved = before.registry.bindings();
    assert_eq!(saved[&host("a")].taint(), Some(&report("residue")));
    assert!(saved[&host("b")].taint().is_none());

    let after = fixture_of(config);
    after
        .registry
        .restore_tainted(&host("b"), saved[&host("b")].clone());
    assert!(after.registry.hosts().is_empty());
    after
        .registry
        .restore_tainted(&host("a"), saved[&host("a")].clone());
    let listed = after.registry.hosts();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].taint, Some(report("residue")));
    assert!(listed[0].lease.is_none());
    let other = Binding::new(cred("zzz"), None).with_taint(Some(report("x")));
    after.registry.restore_tainted(&host("a"), other);
    assert_eq!(after.registry.hosts()[0].taint, Some(report("residue")));

    after.clock.advance(Duration::from_millis(
        js_agent_wire::DEFAULT_GRACE_MS + MIN_LOST_RETENTION_MS + 1,
    ));
    assert_eq!(after.registry.hosts().len(), 1);
    let welcome = after.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(welcome.epoch, 1);
    assert_eq!(after.registry.hosts()[0].taint, Some(report("residue")));
    assert_eq!(
        after.registry.bindings()[&host("a")].taint(),
        Some(&report("residue"))
    );
}
