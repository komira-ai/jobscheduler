//! Container runs on registered hosts: how a run is admitted, what a heartbeat reply
//! carries for it, what a host's held list and results do to it, and when its limits
//! return to the lease.

mod support;

use js_agent_wire::{
    Ack, Command, HeartbeatReply, HeldRun, HeldState, Isolation, JobSpec, Limits, Mount, Outcome,
    Phase, RunResult, limits, refusal,
};
use js_provider::{Provider, RefusalCode, StartAck};
use js_registry::{MAX_FINISHED_RUNS, MAX_LIVE_RUNS, PoolConfig, RunRefusal, RunState, RunView};
use js_types::sched::WorkKey;
use js_types::{Admission, ClassId, ProviderId, ResourceClass, Resources, Shape};
use support::*;

/// A job of `cpu` thousandths of a core and 1 MiB, no GPU.
fn job(run: &str, cpu: u64) -> JobSpec {
    JobSpec {
        run_id: run.into(),
        lease_id: String::new(),
        image: "busybox:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        args: Vec::new(),
        limits: Limits {
            cpu_millis: cpu,
            memory_bytes: 1 << 20,
            pids: None,
            wall_ms: 60_000,
        },
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        isolation: Isolation::Contained,
        labels: Default::default(),
        heartbeat_seq: 0,
        valid_for_ms: 0,
    }
}

fn held(run: &str, state: HeldState) -> HeldRun {
    HeldRun {
        run_id: run.into(),
        state,
    }
}

/// A heartbeat of `host` in `epoch` listing `listed`, answered.
fn beat(fx: &Fixture, host: &str, epoch: u64, listed: &[HeldRun]) -> HeartbeatReply {
    let mut message = fx.heartbeat(host, epoch);
    message.held = listed.to_vec();
    fx.registry.heartbeat(&message, &cred("t")).unwrap()
}

/// As `beat`, carrying `results`.
fn beat_results(
    fx: &Fixture,
    host: &str,
    epoch: u64,
    listed: &[HeldRun],
    results: Vec<RunResult>,
) -> HeartbeatReply {
    let mut message = fx.heartbeat(host, epoch);
    message.held = listed.to_vec();
    message.results = results;
    fx.registry.heartbeat(&message, &cred("t")).unwrap()
}

fn result(run: &str, lease: &str) -> RunResult {
    RunResult {
        run_id: run.into(),
        lease_id: lease.into(),
        outcome: Outcome::Succeeded,
        exit_code: Some(0),
        detail: None,
    }
}

/// The kinds and ids of a reply's commands, as `start r1`, `stop r1`, `cancel r1`.
fn words(reply: &HeartbeatReply) -> Vec<String> {
    reply
        .commands
        .iter()
        .map(|command| match command {
            Command::Start(spec) => format!("start {}", spec.run_id),
            Command::Stop { run_id } => format!("stop {run_id}"),
            Command::Cancel { run_id } => format!("cancel {run_id}"),
            Command::Unknown => "unknown".into(),
        })
        .collect()
}

fn acks(reply: &HeartbeatReply) -> Vec<(String, Ack)> {
    let acks = reply.result_acks.iter();
    acks.map(|ack| (ack.run_id.clone(), ack.ack)).collect()
}

/// A fixture with host `a` joined and leased: lease 1 holds it. Returns the epoch.
fn leased() -> (Fixture, u64) {
    let fx = fixture(4);
    let epoch = fx.join("a", "t");
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
    (fx, epoch)
}

fn queue(fx: &Fixture, run: &str, cpu: u64) -> Result<RunView, RunRefusal> {
    fx.registry.queue_run(&host("a"), &lease(1), job(run, cpu))
}

fn state_of(fx: &Fixture, run: &str) -> RunState {
    let runs = fx.registry.runs_of(&lease(1));
    runs.into_iter()
        .find(|view| view.run_id == run)
        .unwrap()
        .state
}

fn stop_lease(fx: &Fixture, sequence: u64) {
    let mut provider = fx.provider.clone();
    provider.stop(&lease(sequence));
}

/// Asserts the life of a run through the replies: its start is carried on every reply
/// until the host lists it, and not after; the state follows what the host lists
/// (starting, running, exited, and a state this version does not know reads as running);
/// and the run is ended when the host stops listing it. It catches a start sent once only
/// (a reply lost on the way would lose the run) and a start repeated for a run the host
/// already holds.
#[test]
fn a_start_is_carried_until_the_host_lists_the_run() {
    let (fx, epoch) = leased();
    let queued = queue(&fx, "r1", 1000).unwrap();
    assert_eq!(queued.state, RunState::Queued);
    assert_eq!(queued.lease, lease(1));
    assert_eq!(queued.host, host("a"));
    for _ in 0..3 {
        let reply = beat(&fx, "a", epoch, &[]);
        assert_eq!(words(&reply), ["start r1"]);
        assert_eq!(state_of(&fx, "r1"), RunState::Queued);
    }
    let reply = beat(&fx, "a", epoch, &[held("r1", HeldState::Starting)]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::Starting);
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(state_of(&fx, "r1"), RunState::Running);
    beat(&fx, "a", epoch, &[held("r1", HeldState::ExitedUnacked)]);
    assert_eq!(state_of(&fx, "r1"), RunState::Exited);
    beat(&fx, "a", epoch, &[held("r1", HeldState::Unknown)]);
    assert_eq!(state_of(&fx, "r1"), RunState::Running);
    let reply = beat(&fx, "a", epoch, &[]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
}

/// Asserts what the start carries: the lease the registry knows it under, the heartbeats
/// the host had taken when it was queued, and the fields the caller set. It catches a
/// start that names the caller's lease text instead of the booking's, and one with no
/// sequence for the agent to order by.
#[test]
fn the_start_names_the_lease_and_the_heartbeat_it_was_queued_at() {
    let (fx, epoch) = leased();
    beat(&fx, "a", epoch, &[]);
    beat(&fx, "a", epoch, &[]);
    let mut wanted = job("r1", 1000);
    wanted.lease_id = "something-else".into();
    wanted.valid_for_ms = 5_000;
    fx.registry
        .queue_run(&host("a"), &lease(1), wanted)
        .unwrap();
    let reply = beat(&fx, "a", epoch, &[]);
    let Command::Start(sent) = &reply.commands[0] else {
        panic!("{:?}", reply.commands);
    };
    assert_eq!(sent.lease_id, "1-1");
    assert_eq!(sent.heartbeat_seq, 2);
    assert_eq!(sent.valid_for_ms, 5_000);
    assert!(reply.validate().refused.is_empty());
}

/// Asserts that a stop is a retained intent: it is on every reply while the host still
/// lists the run, the run reads as stopping, and once the host stops listing it the stop
/// is gone and the run has ended. It catches a stop sent once, which a lost reply would
/// turn into a run that never stops.
#[test]
fn stop_is_resent_until_the_run_leaves_the_held_list() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    let asked = fx.registry.stop_run(&lease(1), "r1").unwrap();
    assert_eq!(asked.state, RunState::Stopping);
    for _ in 0..3 {
        let reply = beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
        assert_eq!(words(&reply), ["stop r1"]);
    }
    let reply = beat(&fx, "a", epoch, &[]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
}

/// Asserts that a stop for a run the host never listed ends it as never started once the
/// stop has gone out, and that no start is sent beside the stop. It catches a queued run
/// that is stopped and then waits forever for a host that has no record of it, holding
/// its limits.
#[test]
fn a_run_stopped_before_it_is_listed_is_given_up_after_the_stop() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let reply = beat(&fx, "a", epoch, &[]);
    assert_eq!(words(&reply), ["stop r1"]);
    assert_eq!(state_of(&fx, "r1"), RunState::Stopping);
    let reply = beat(&fx, "a", epoch, &[]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::NeverStarted);
    assert!(queue(&fx, "r2", 4000).is_ok());
}

/// Asserts that a queued run is not started on a host that was lost before it listed the
/// run: when the host is heard again, the run is never started, no start is sent, and its
/// limits are free. It catches a start delivered to a host that went away and came back
/// after the work no longer mattered.
#[test]
fn a_queued_run_is_never_started_on_a_host_that_was_lost() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 4000).unwrap();
    fx.pass_grace();
    let reply = beat(&fx, "a", epoch, &[]);
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::NeverStarted);
    assert!(queue(&fx, "r2", 4000).is_ok());
}

/// Asserts that a run the host lists is not given up when the host was lost in between
/// (its reply reached it): the run is running when the host is heard again, in a
/// heartbeat and in a hello of a new session alike; and that a hello which no longer
/// lists a run that was held ends it. It catches a loss that forgets work still running.
#[test]
fn a_listed_run_survives_the_loss_of_its_host() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    queue(&fx, "r2", 1000).unwrap();
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    fx.pass_grace();
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(state_of(&fx, "r1"), RunState::Running);
    assert_eq!(state_of(&fx, "r2"), RunState::NeverStarted);

    fx.pass_grace();
    let mut again = hello("a");
    again.held = vec![held("r1", HeldState::Running)];
    let welcome = fx.hello(&again, &cred("t")).unwrap();
    assert_eq!(state_of(&fx, "r1"), RunState::Running);
    fx.pass_grace();
    fx.hello(&hello("a"), &cred("t")).unwrap();
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
    assert_eq!(welcome.epoch, 2);
}

/// Asserts that a job whose image is not pinned by a sha256 digest is refused before any
/// state is read, with `image-digest-required`, and queues nothing; a digest alone and a
/// tag with a digest are queued. It catches a start reaching a host that pulls what it
/// lacks with a tag that can move.
#[test]
fn an_unpinned_image_is_refused_and_a_pinned_one_queued() {
    let (fx, _epoch) = leased();
    let ask = |spec: JobSpec| fx.registry.queue_run(&host("a"), &lease(1), spec);
    let hex = "0123456789abcdef".repeat(4);
    for image in [
        "busybox:1".to_string(),
        "busybox".to_string(),
        format!("busybox@sha256:{}", &hex[..40]),
        format!("busybox@sha512:{hex}{hex}"),
    ] {
        let mut bad = job("r1", 1);
        bad.image = image.clone();
        let err = ask(bad).unwrap_err();
        assert_eq!(err.code(), refusal::IMAGE_DIGEST_REQUIRED, "{image}");
    }
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
    let mut digest = job("r1", 1);
    digest.image = format!("busybox@sha256:{hex}");
    ask(digest).unwrap();
    let mut both = job("r2", 1);
    both.image = format!("busybox:1@sha256:{hex}");
    ask(both).unwrap();
}

/// Asserts every refusal of the admission and its code, in the order the registry checks
/// them, and that a refusal queues nothing. It catches a refusal with the wrong code or
/// status class, and a check that moved behind one that depends on state.
#[test]
fn every_refusal_has_its_code() {
    let (fx, epoch) = leased();
    let ask = |spec: JobSpec| fx.registry.queue_run(&host("a"), &lease(1), spec);
    let code = |err: RunRefusal| err.code();

    let stranger = fx
        .registry
        .queue_run(&host("static-1"), &lease(1), job("r1", 1));
    assert_eq!(stranger.unwrap_err(), RunRefusal::HostNotRegistered);

    let mut bad = job("r1", 1000);
    bad.image = "oci:thing".into();
    let invalid = ask(bad).unwrap_err();
    assert_eq!(code(invalid), refusal::INVALID_IMAGE);
    let mut bad = job("r1", 1000);
    bad.mounts = vec![Mount {
        root_name: "cache".into(),
        rel: "../up".into(),
        ro: true,
    }];
    assert_eq!(code(ask(bad).unwrap_err()), refusal::INVALID_MOUNT);
    let mut bad = job("r1", 1000);
    bad.limits.wall_ms = 0;
    assert_eq!(code(ask(bad).unwrap_err()), refusal::WALL_TIME_NOT_POSITIVE);

    let mut host_job = job("r1", 1000);
    host_job.isolation = Isolation::Host;
    let refused = ask(host_job).unwrap_err();
    assert_eq!(refused, RunRefusal::HostJobsNotAllowed);
    assert_eq!(code(refused), refusal::HOST_JOBS_NOT_ALLOWED);

    let over = ask(job("r1", 4001)).unwrap_err();
    assert_eq!(over, RunRefusal::LimitsExceedShape);
    assert_eq!(code(over), refusal::LIMITS_EXCEED_SHAPE);
    let mut over = job("r1", 1000);
    over.limits.memory_bytes = (8 << 30) + 1;
    assert_eq!(ask(over).unwrap_err(), RunRefusal::LimitsExceedShape);
    let mut gpu = job("r1", 1000);
    gpu.gpu_indices = vec![0];
    assert_eq!(ask(gpu).unwrap_err(), RunRefusal::LimitsExceedShape);

    let unheld = fx
        .registry
        .queue_run(&host("a"), &lease(9), job("r1", 1000));
    let unheld = unheld.unwrap_err();
    assert_eq!(unheld, RunRefusal::NotHeld);
    assert_eq!(code(unheld), refusal::NOT_HELD);

    queue(&fx, "r1", 3000).unwrap();
    let full = ask(job("r2", 1001)).unwrap_err();
    assert_eq!(full, RunRefusal::NoCapacity);
    assert_eq!(code(full), refusal::NO_CAPACITY);
    let mut heavy = job("r2", 10);
    heavy.limits.memory_bytes = 8 << 30;
    assert_eq!(ask(heavy).unwrap_err(), RunRefusal::NoCapacity);
    let twice = ask(job("r1", 10)).unwrap_err();
    assert_eq!(twice, RunRefusal::RunExists);
    assert_eq!(code(twice), "run-exists");
    assert_eq!(code(RunRefusal::UnknownRun), "unknown-run");
    assert_eq!(fx.registry.runs_of(&lease(1)).len(), 1);

    fx.pass_grace();
    let lost = ask(job("r3", 10)).unwrap_err();
    assert_eq!(lost, RunRefusal::HostLost);
    assert_eq!(code(lost), refusal::HOST_LOST);
    beat(&fx, "a", epoch, &[]);
    let mut draining = fx.heartbeat("a", epoch);
    draining.phase = Phase::Draining;
    fx.registry.heartbeat(&draining, &cred("t")).unwrap();
    assert_eq!(ask(job("r3", 10)).unwrap_err(), RunRefusal::NoCapacity);
}

/// Asserts that a host which has not said hello since a restart (awaiting) takes no run,
/// as a lost one does. It catches a run queued for a host nobody has heard from.
#[test]
fn an_awaiting_host_takes_no_run() {
    let fx = fixture(2);
    fx.provider.seat(&host("a"), lease(1), None).unwrap();
    let asked = fx.registry.queue_run(&host("a"), &lease(1), job("r1", 10));
    assert_eq!(asked.unwrap_err(), RunRefusal::HostLost);
}

/// A pool of two GPUs.
fn gpu_fixture() -> Fixture {
    let name = ProviderId::new("builders").unwrap();
    let class = ResourceClass::new(ClassId::new("mac-host").unwrap(), Admission::BookAll, 0);
    let resources = Resources::new(4000, 8 << 30, 2, Some("nvidia-a100".into())).unwrap();
    let shape = Shape::new(resources, map(&[("os", "macos")]));
    fixture_of(PoolConfig::new(name, class, shape, 2))
}

/// Asserts the GPU rules: an index beyond the booked GPUs is over the shape, an index a
/// live run holds is no capacity, and the index is free again once that run is gone. It
/// catches two jobs given the same device.
#[test]
fn a_gpu_index_belongs_to_one_live_run() {
    let fx = gpu_fixture();
    let mut offer = hello("a");
    offer.shape.gpus = 2;
    offer.shape.gpu_class = Some("nvidia-a100".into());
    let epoch = fx.hello(&offer, &cred("t")).unwrap().epoch;
    fx.provider.clone().start(spec(1));
    let on = |run: &str, indices: &[u32]| {
        let mut spec = job(run, 100);
        spec.gpu_indices = indices.to_vec();
        fx.registry.queue_run(&host("a"), &lease(1), spec)
    };
    assert_eq!(on("r0", &[2]).unwrap_err(), RunRefusal::LimitsExceedShape);
    assert!(on("r1", &[1]).is_ok());
    assert_eq!(on("r2", &[0, 1]).unwrap_err(), RunRefusal::NoCapacity);
    assert!(on("r2", &[0]).is_ok());
    assert_eq!(on("r3", &[0]).unwrap_err(), RunRefusal::NoCapacity);
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(on("r3", &[1]).unwrap_err(), RunRefusal::NoCapacity);
    beat(&fx, "a", epoch, &[]);
    assert!(on("r3", &[1]).is_ok());
}

/// Asserts the budget rule: a run's limits return to the lease only when the host stops
/// listing it. While it is listed (running, or exited and not yet acknowledged) a run
/// that would fit once it was gone is refused; the moment the list no longer has it, the
/// same run is admitted. It catches a budget that is returned when the result arrives or
/// the stop is asked, which would let a second job start while the first still holds the
/// machine.
#[test]
fn the_budget_returns_only_after_the_run_leaves_the_held_list() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 2000).unwrap();
    queue(&fx, "r2", 2000).unwrap();
    assert_eq!(queue(&fx, "r3", 2000).unwrap_err(), RunRefusal::NoCapacity);
    let both = [
        held("r1", HeldState::Running),
        held("r2", HeldState::Running),
    ];
    beat(&fx, "a", epoch, &both);
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let exited = [
        held("r1", HeldState::ExitedUnacked),
        held("r2", HeldState::Running),
    ];
    let reply = beat_results(&fx, "a", epoch, &exited, vec![result("r1", "1-1")]);
    assert_eq!(acks(&reply), [("r1".to_string(), Ack::Accepted)]);
    assert_eq!(queue(&fx, "r3", 2000).unwrap_err(), RunRefusal::NoCapacity);
    let after = [held("r2", HeldState::Running)];
    beat(&fx, "a", epoch, &after);
    assert!(queue(&fx, "r3", 2000).is_ok());
}

/// Asserts each acknowledgement of a result: accepted the first time, duplicate when it is
/// resent, mismatch for a run of another host or for a run named under another lease,
/// stale when the lease no longer holds the host, and for a run the registry has no record
/// of: stale when the lease holds this host, mismatch when it holds another, unknown lease
/// when no host holds it. It catches a verdict that falls into the wrong class, which
/// decides whether an agent keeps or drops a result.
#[test]
fn a_result_gets_the_verdict_for_what_it_names() {
    let fx = fixture(4);
    let a = fx.join("a", "t");
    let b = fx.join("b", "t");
    fx.provider.clone().start(spec(1));
    fx.provider.clone().start(spec(2));
    queue(&fx, "r1", 1000).unwrap();
    queue(&fx, "r2", 1000).unwrap();
    queue(&fx, "r3", 1000).unwrap();
    let listed = [
        held("r1", HeldState::Running),
        held("r2", HeldState::Running),
        held("r3", HeldState::Running),
    ];
    let first = beat_results(&fx, "a", a, &listed, vec![result("r1", "1-1")]);
    assert_eq!(acks(&first), [("r1".to_string(), Ack::Accepted)]);
    let views = fx.registry.runs_of(&lease(1));
    assert_eq!(views[0].result, Some(result("r1", "1-1")));

    let again = beat_results(&fx, "a", a, &listed, vec![result("r1", "1-1")]);
    assert_eq!(acks(&again), [("r1".to_string(), Ack::Duplicate)]);

    // Another host names the right lease and a run with no result yet: only the host
    // check tells it apart from the owner.
    let theirs = beat_results(&fx, "b", b, &[], vec![result("r2", "1-1")]);
    assert_eq!(acks(&theirs), [("r2".to_string(), Ack::Mismatch)]);
    let under_theirs = beat_results(&fx, "b", b, &[], vec![result("r1", "1-2")]);
    assert_eq!(acks(&under_theirs), [("r1".to_string(), Ack::Mismatch)]);
    let elsewhere = beat_results(&fx, "a", a, &listed, vec![result("r2", "7-7")]);
    assert_eq!(acks(&elsewhere), [("r2".to_string(), Ack::Mismatch)]);

    let unknown = vec![
        result("x1", "1-1"),
        result("x2", "1-2"),
        result("x3", "7-7"),
    ];
    let unrecorded = beat_results(&fx, "a", a, &listed, unknown);
    let wanted = [
        ("x1".to_string(), Ack::Stale),
        ("x2".to_string(), Ack::Mismatch),
        ("x3".to_string(), Ack::UnknownLease),
    ];
    assert_eq!(acks(&unrecorded), wanted);

    stop_lease(&fx, 1);
    let moved_on = beat_results(&fx, "a", a, &listed, vec![result("r3", "1-1")]);
    assert_eq!(acks(&moved_on), [("r3".to_string(), Ack::Stale)]);
    let kept = fx.registry.runs_of(&lease(1));
    assert!(
        kept.iter()
            .all(|view| view.result.is_none() | (view.run_id == "r1"))
    );
}

/// Asserts what a stopped lease does: the booking is free at once, but the host takes no
/// new lease while one of its runs is still listed, the stop for each run goes out on
/// every reply, the host reads as releasing, and the host is bookable again after the
/// first heartbeat that lists none. It catches a lease released while its job still runs
/// that hands the machine to the next client at once.
#[test]
fn a_released_host_is_bookable_only_after_a_heartbeat_lists_no_run() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    queue(&fx, "r2", 1000).unwrap();
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    stop_lease(&fx, 1);
    let refused = fx.provider.clone().start(spec(2));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    assert!(fx.registry.hosts()[0].releasing);
    assert!(fx.registry.hosts()[0].lease.is_none());

    let reply = beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(words(&reply), ["stop r1", "stop r2"]);
    let again = beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    assert_eq!(words(&again), ["stop r1"]);
    let refused = fx.provider.clone().start(spec(2));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    let done = beat(&fx, "a", epoch, &[]);
    assert!(done.commands.is_empty());
    assert!(!fx.registry.hosts()[0].releasing);
    let taken = fx.provider.clone().start(spec(2));
    assert_eq!(started_on(&taken), Some("a"));
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
}

/// Asserts that a run the registry holds no record of is cancelled and never adopted: a
/// hello that lists one is admitted, the host takes no lease and no run is queued for it,
/// the next heartbeat reply cancels it for as long as it is listed, and the host is
/// bookable after a heartbeat that lists none. It catches an agent that restarted with
/// work the server forgot being left to run on a machine that is then leased to someone
/// else.
#[test]
fn a_run_listed_in_a_hello_that_the_registry_does_not_know_is_cancelled() {
    let fx = fixture(2);
    let mut rejoin = hello("a");
    rejoin.held = vec![held("old", HeldState::Running)];
    let epoch = fx.hello(&rejoin, &cred("t")).unwrap().epoch;
    assert!(fx.registry.hosts()[0].releasing);
    let refused = fx.provider.clone().start(spec(1));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));

    let reply = beat(&fx, "a", epoch, &[held("old", HeldState::Running)]);
    assert_eq!(words(&reply), ["cancel old"]);
    let reply = beat(&fx, "a", epoch, &[held("old", HeldState::ExitedUnacked)]);
    assert_eq!(words(&reply), ["cancel old"]);
    let reply = beat(&fx, "a", epoch, &[]);
    assert!(reply.commands.is_empty());
    assert!(started_on(&fx.provider.clone().start(spec(1))).is_some());
}

/// Asserts the cancels beyond the live runs: a run that has ended and is listed again, and
/// a run that is live on another host, are both cancelled on the host that lists them.
/// It catches an adoption through a stale record or through another host's run.
#[test]
fn an_ended_run_or_a_run_of_another_host_is_cancelled_when_listed() {
    let fx = fixture(4);
    let a = fx.join("a", "t");
    let b = fx.join("b", "t");
    fx.provider.clone().start(spec(1));
    queue(&fx, "r1", 1000).unwrap();
    beat(&fx, "a", a, &[held("r1", HeldState::Running)]);
    let reply = beat(&fx, "b", b, &[held("r1", HeldState::Running)]);
    assert_eq!(words(&reply), ["cancel r1"]);
    beat(&fx, "a", a, &[]);
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
    let reply = beat(&fx, "a", a, &[held("r1", HeldState::Running)]);
    assert_eq!(words(&reply), ["cancel r1"]);
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
}

/// Asserts that a reply stays within the wire's caps whatever is queued and listed: with
/// more unknown runs listed than a reply may carry, the commands are cut to the cap, the
/// stop comes first and the starts last, the reply validates with nothing refused, and
/// the acknowledgements are as many as the results. It catches a reply the agent would
/// refuse in part, which could cost it a stop.
#[test]
fn a_reply_stays_within_the_caps_with_many_commands() {
    let (fx, epoch) = leased();
    queue(&fx, "mine", 1000).unwrap();
    fx.registry.stop_run(&lease(1), "mine").unwrap();
    queue(&fx, "next", 1000).unwrap();
    let mut listed = vec![held("mine", HeldState::Running)];
    let strangers = limits::MAX_COMMANDS + 76;
    listed.extend((0..strangers).map(|n| held(&format!("s{n}"), HeldState::Running)));
    let reply = beat(&fx, "a", epoch, &listed);
    assert_eq!(reply.commands.len(), limits::MAX_COMMANDS);
    assert_eq!(words(&reply)[0], "stop mine");
    assert_eq!(words(&reply)[1], "cancel s0");
    assert!(!words(&reply).contains(&"start next".to_string()));
    let checked = reply.validate();
    assert!(checked.refused.is_empty());
    assert!(checked.acks.is_ok());
    let mut few = vec![held("mine", HeldState::Running)];
    few.extend((0..3).map(|n| held(&format!("s{n}"), HeldState::Running)));
    let reply = beat(&fx, "a", epoch, &few);
    assert_eq!(
        words(&reply),
        [
            "stop mine",
            "cancel s0",
            "cancel s1",
            "cancel s2",
            "start next"
        ]
    );
}

/// Asserts that the live runs of a host are capped at [`MAX_LIVE_RUNS`] so that a stop
/// for each fits one reply, and that the 257th is no capacity even though its limits fit.
/// It catches an unbounded queue of commands.
#[test]
fn live_runs_are_capped() {
    let (fx, epoch) = leased();
    for n in 0..MAX_LIVE_RUNS {
        queue(&fx, &format!("r{n}"), 1).unwrap();
    }
    assert_eq!(queue(&fx, "over", 1).unwrap_err(), RunRefusal::NoCapacity);
    for n in 0..MAX_LIVE_RUNS {
        fx.registry.stop_run(&lease(1), &format!("r{n}")).unwrap();
    }
    let reply = beat(&fx, "a", epoch, &[]);
    assert_eq!(reply.commands.len(), MAX_LIVE_RUNS);
    assert!(reply.validate().refused.is_empty());
}

/// Asserts that a start is held back while the host is not serving and sent when it is,
/// while a stop is not held back. It catches work started on a draining or quarantined
/// host.
#[test]
fn starts_wait_for_the_serving_phase() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    let mut draining = fx.heartbeat("a", epoch);
    draining.phase = Phase::Draining;
    let reply = fx.registry.heartbeat(&draining, &cred("t")).unwrap();
    assert!(reply.commands.is_empty());
    assert_eq!(state_of(&fx, "r1"), RunState::Queued);
    let reply = beat(&fx, "a", epoch, &[]);
    assert_eq!(words(&reply), ["start r1"]);
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let mut draining = fx.heartbeat("a", epoch);
    draining.phase = Phase::Draining;
    let reply = fx.registry.heartbeat(&draining, &cred("t")).unwrap();
    assert_eq!(words(&reply), ["stop r1"]);
}

/// Asserts the stop route's rules: a run of another lease or that does not exist is
/// unknown, a run that has ended is left as it is and reads as ended, and a run stopped
/// twice stays one intent. It catches a stop that reaches a run of another lease.
#[test]
fn stopping_a_run_follows_the_lease() {
    let (fx, epoch) = leased();
    queue(&fx, "r1", 1000).unwrap();
    let other = fx.registry.stop_run(&lease(2), "r1").unwrap_err();
    assert_eq!(other, RunRefusal::UnknownRun);
    let none = fx.registry.stop_run(&lease(1), "nope").unwrap_err();
    assert_eq!(none, RunRefusal::UnknownRun);
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    fx.registry.stop_run(&lease(1), "r1").unwrap();
    let twice = fx.registry.stop_run(&lease(1), "r1").unwrap();
    assert_eq!(twice.state, RunState::Stopping);
    beat(&fx, "a", epoch, &[]);
    let late = fx.registry.stop_run(&lease(1), "r1").unwrap();
    assert_eq!(late.state, RunState::Ended);
    assert_eq!(state_of(&fx, "r1"), RunState::Ended);
}

/// Asserts the retention of finished runs: they stay readable while the lease holds the
/// host, only the newest [`MAX_FINISHED_RUNS`] of them, in the order they were queued;
/// and they are dropped when the lease has been stopped. It catches finished runs that
/// pile up for the life of the process.
#[test]
fn finished_runs_are_bounded_and_dropped_with_the_lease() {
    let (fx, epoch) = leased();
    let total = MAX_FINISHED_RUNS + 5;
    for n in 0..total {
        let run = format!("r{n}");
        queue(&fx, &run, 1000).unwrap();
        beat(&fx, "a", epoch, &[held(&run, HeldState::Running)]);
        beat(&fx, "a", epoch, &[]);
    }
    let kept = fx.registry.runs_of(&lease(1));
    assert_eq!(kept.len(), MAX_FINISHED_RUNS);
    assert_eq!(kept[0].run_id, "r5");
    assert_eq!(
        kept[MAX_FINISHED_RUNS - 1].run_id,
        format!("r{}", total - 1)
    );
    assert!(kept.iter().all(|view| view.state == RunState::Ended));
    stop_lease(&fx, 1);
    beat(&fx, "a", epoch, &[]);
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
}

/// Asserts that a host forgotten after its retention takes its runs with it: a host that
/// holds no lease, lists a run, and then stays silent past the grace period and the
/// retention is dropped and so are the records of its runs. It catches records that
/// outlive their host.
#[test]
fn a_forgotten_host_forgets_its_runs() {
    let config = config(2).with_lost_retention(js_registry::MIN_LOST_RETENTION_MS);
    let fx = fixture_of(config.unwrap());
    let epoch = fx.join("a", "t");
    fx.provider.clone().start(spec(1));
    queue(&fx, "r1", 1000).unwrap();
    beat(&fx, "a", epoch, &[held("r1", HeldState::Running)]);
    stop_lease(&fx, 1);
    assert_eq!(fx.registry.runs_of(&lease(1)).len(), 1);
    let kept = js_agent_wire::DEFAULT_GRACE_MS + js_registry::MIN_LOST_RETENTION_MS + 1;
    fx.clock.advance(std::time::Duration::from_millis(kept));
    assert!(fx.registry.hosts().is_empty());
    assert!(fx.registry.runs_of(&lease(1)).is_empty());
}

/// Asserts the names a state prints as, which the consumer API shows. It catches a rename.
#[test]
fn states_have_stable_names() {
    let names = [
        (RunState::Queued, "queued"),
        (RunState::Starting, "starting"),
        (RunState::Running, "running"),
        (RunState::Exited, "exited"),
        (RunState::Stopping, "stopping"),
        (RunState::Ended, "ended"),
        (RunState::NeverStarted, "never-started"),
    ];
    for (state, name) in names {
        assert_eq!(state.as_str(), name);
    }
}

/// Asserts that a run queued with a work key answers it, by id and in its view, and that a
/// run queued without one, and a run the registry does not know, answer none; and that the
/// start the host is sent is, whole, the start of the same run queued without a key. It
/// catches a key lost between the queue and the result, a key given to the wrong run, and
/// a key that reaches the agent by any route (a label, an argument, the environment, the
/// image or a field of its own).
#[test]
fn a_work_key_stays_with_its_run_and_never_reaches_the_host() {
    let (fx, epoch) = leased();
    let key = WorkKey::new("builds", "ab12/34");
    let keyed = fx
        .registry
        .queue_keyed_run(&host("a"), &lease(1), job("r1", 500), Some(key.clone()))
        .unwrap();
    assert_eq!(keyed.work_key, Some(key.clone()));
    let plain = queue(&fx, "r2", 500).unwrap();
    assert_eq!(plain.work_key, None);
    assert_eq!(fx.registry.work_key_of("r1"), Some(key.clone()));
    assert_eq!(fx.registry.work_key_of("r2"), None);
    assert_eq!(fx.registry.work_key_of("r3"), None);
    let listed = fx.registry.runs_of(&lease(1));
    assert_eq!(listed[0].work_key, Some(key));
    assert_eq!(listed[1].work_key, None);
    let reply = beat(&fx, "a", epoch, &[]);
    assert_eq!(words(&reply), ["start r1", "start r2"]);
    // The same run id submitted without a key, on a registry of its own: what the host
    // receives for it must be equal to the start of the keyed run, whole.
    let (other, other_epoch) = leased();
    queue(&other, "r1", 500).unwrap();
    let unkeyed = beat(&other, "a", other_epoch, &[]);
    assert_eq!(words(&unkeyed), ["start r1"]);
    assert_eq!(reply.commands[0], unkeyed.commands[0]);
}
