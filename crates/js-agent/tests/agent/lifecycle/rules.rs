//! The rules of a start and a stop: what runs, what is refused and with which code, and
//! what is dropped without a word.

use std::time::Duration;

use js_agent::lifecycle::{RETIRED_CAP, label};
use js_agent_wire::limits::{MAX_COMMANDS, MAX_HELD, MAX_RESULTS};
use js_agent_wire::{Command, HeldRun, HeldState, Isolation, Outcome};
use serde_json::json;

use super::{GRACE, Harness, Options, cleanup, job, names, reply_json};

/// Asserts that a start for a run the agent holds changes nothing, whether the first
/// start was a moment ago, in the same reply, or has since finished and been
/// acknowledged: one container, one scratch directory, one booking. It catches a start
/// that runs the work twice when the server resends it.
#[test]
fn duplicate_start_idempotent() {
    let mut h = Harness::new("duplicate");
    h.start(job("r1"));
    let events = h.start(job("r1"));
    assert!(
        events.is_empty(),
        "a repeated start is not news: {events:?}"
    );
    assert_eq!(h.fake.created().len(), 1);
    assert_eq!(h.life.free().cpu_millis, 3000);
    let twice = h.apply(vec![
        Command::Start(Box::new(job("r2"))),
        Command::Start(Box::new(job("r2"))),
    ]);
    assert_eq!(names(&twice), ["invalid #1 invalid-reply", "started r2"]);
    assert_eq!(h.fake.created().len(), 2);
    h.fake.exit("r1", 0);
    h.life.step();
    let after_exit = h.start(job("r1"));
    assert!(
        after_exit.is_empty(),
        "a run that exited and is not acknowledged is held: {after_exit:?}"
    );
    assert_eq!(h.fake.created().len(), 2);
    assert_eq!(h.life.results().len(), 1);
    h.ack("r1", "ACCEPTED");
    assert_eq!(h.held(), ["r2"]);
    let late = h.start(job("r1"));
    assert_eq!(names(&late), ["skipped r1 Retired"]);
    assert_eq!(
        h.fake.created().len(),
        2,
        "a finished run does not run again"
    );
}

/// Asserts that a stop that arrives before its start, for a run the agent never held,
/// leaves the start dead: it creates nothing, makes no scratch directory and reports
/// nothing. A cancel does the same. It catches an agent that forgets a stop it could not
/// apply, and starts the run the server has given up.
#[test]
fn start_after_stop_never_runs() {
    let mut h = Harness::new("stop-first");
    assert!(h.stop("r1").is_empty());
    assert!(h.cancel("r2").is_empty());
    for run in ["r1", "r2"] {
        let events = h.start(job(run));
        assert_eq!(names(&events), [format!("skipped {run} Retired")]);
        assert!(!h.scratch_of(run).exists());
    }
    assert!(h.log.borrow().is_empty(), "no container call was made");
    assert!(h.held().is_empty());
    assert!(h.life.results().is_empty());
    let ok = h.start(job("r3"));
    assert_eq!(names(&ok), ["started r3"]);
}

/// Asserts that the agent remembers a bounded number of stopped run ids, the newest:
/// after one more than the cap, the oldest may start again and the newest may not. It
/// catches a memory that grows for as long as the agent lives.
#[test]
fn the_memory_of_stopped_runs_is_bounded() {
    let mut h = Harness::new("retired-cap");
    let ids: Vec<String> = (0..=RETIRED_CAP).map(|n| format!("x{n}")).collect();
    for chunk in ids.chunks(1000) {
        let stops = chunk
            .iter()
            .map(|run_id| Command::Stop {
                run_id: run_id.clone(),
            })
            .collect();
        h.apply(stops);
    }
    assert_eq!(
        names(&h.start(job("x1"))),
        ["skipped x1 Retired"],
        "the second oldest is still remembered, with no stop sent again"
    );
    assert_eq!(
        names(&h.start(job("x0"))),
        ["started x0"],
        "the oldest is forgotten"
    );
    let newest = ids.last().unwrap();
    assert_eq!(
        names(&h.start(job(newest))),
        [format!("skipped {newest} Retired")]
    );
}

/// Asserts that a stop repeated for the same unknown run takes one place in the memory of
/// stopped runs: after it, as many other ids as fit beside it are stopped, and the first
/// still cannot start. It catches a repeated stop that is remembered twice and pushes the
/// run out of the memory early.
#[test]
fn a_repeated_stop_takes_one_place_in_the_memory() {
    let mut h = Harness::new("retired-repeat");
    h.stop("dup");
    h.stop("dup");
    let others: Vec<String> = (0..RETIRED_CAP - 1).map(|n| format!("y{n}")).collect();
    for chunk in others.chunks(1000) {
        let stops = chunk
            .iter()
            .map(|run_id| Command::Stop {
                run_id: run_id.clone(),
            })
            .collect();
        h.apply(stops);
    }
    assert_eq!(names(&h.start(job("dup"))), ["skipped dup Retired"]);
}

/// Asserts that a start that waited longer than its `valid_for_ms` on the agent's clock
/// (here because the starts before it in the reply took long) is dropped with no result,
/// no held run and no container; that a start that waited exactly its `valid_for_ms`
/// still runs; that a start with no bound runs however long the starts before it took;
/// and that the same run may start from a later reply. It catches a stale start that
/// runs, one that is reported (the server gives its lease up as never started on its
/// own), a boundary off by one millisecond, and a zero bound read as "no time at all".
#[test]
fn stale_start_dropped_silently() {
    let mut h = Harness::new("stale");
    h.fake.create_takes(1500);
    // Each start takes 1500 ms, so the n-th start of the reply waited (n - 1) * 1500.
    let small = |run: &str, valid_for_ms: u64| {
        let mut spec = job(run);
        spec.limits.cpu_millis = 500;
        spec.valid_for_ms = valid_for_ms;
        spec
    };
    let late = small("late", 1000);
    let events = h.apply(vec![
        Command::Start(Box::new(small("slow", 0))),
        // Waited exactly as long as it allows: runs.
        Command::Start(Box::new(small("exact", 1500))),
        // No bound, after the clock has moved: runs.
        Command::Start(Box::new(small("unbounded", 0))),
        Command::Start(Box::new(late.clone())),
        Command::Start(Box::new(small("fine", 5000))),
    ]);
    assert_eq!(
        names(&events),
        [
            "started slow",
            "started exact",
            "started unbounded",
            "skipped late Stale",
            "started fine"
        ]
    );
    assert_eq!(h.held(), ["slow", "exact", "unbounded", "fine"]);
    assert!(
        h.life.results().is_empty(),
        "nothing is sent for a stale start"
    );
    assert!(!h.scratch_of("late").exists());
    h.fake.create_takes(0);
    assert_eq!(names(&h.start(late)), ["started late"]);
}

/// Asserts that a start with `isolation = host` on an agent without `--allow-host-jobs`
/// creates nothing (no container, no scratch directory, no booking) and is answered
/// with a held `failed_to_start` result carrying `host-jobs-not-allowed`; and that with
/// the consent the same start enters the state machine and reaches the runtime marked as
/// a host job. It catches an agent that runs a job outside a container on the server's
/// word alone.
#[test]
fn host_job_without_agent_consent_creates_nothing() {
    let mut h = Harness::new("host-refused");
    let mut spec = job("r1");
    spec.isolation = Isolation::Host;
    let events = h.start(spec.clone());
    assert_eq!(names(&events), ["refused r1 host-jobs-not-allowed"]);
    assert!(h.log.borrow().is_empty());
    assert!(!h.scratch_of("r1").exists());
    assert_eq!(h.life.free().cpu_millis, 4000);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("host-jobs-not-allowed"));
    assert_eq!(
        h.life.held(),
        [HeldRun {
            run_id: "r1".to_owned(),
            state: HeldState::ExitedUnacked
        }]
    );

    let again = h.start(spec.clone());
    assert!(
        again.is_empty(),
        "a refused run whose result is held is not refused twice: {again:?}"
    );
    assert_eq!(h.life.results().len(), 1);
    assert_eq!(h.life.held().len(), 1);

    let mut allowed = Harness::with(
        "host-allowed",
        Options {
            allow_host_jobs: true,
            ..Options::default()
        },
    );
    assert_eq!(
        names(&allowed.start(spec)),
        ["tainted r1 host-job", "started r1"]
    );
    assert_eq!(allowed.fake.created()[0].isolation, Isolation::Host);
}

/// Asserts that a start whose isolation this version cannot classify never reaches the
/// runtime: the reply decodes it as an unknown command, nothing is created, and a valid
/// start beside it still runs. It catches an agent that runs a job it cannot classify
/// as a contained one.
#[test]
fn unknown_isolation_refused_never_run() {
    let mut h = Harness::new("unknown-isolation");
    let mut odd = serde_json::to_value(Command::Start(Box::new(job("odd")))).unwrap();
    odd["isolation"] = json!("quantum");
    let fine = serde_json::to_value(Command::Start(Box::new(job("fine")))).unwrap();
    let events = h
        .life
        .apply(&reply_json(&json!({ "commands": [odd, fine] })));
    assert_eq!(names(&events), ["started fine"]);
    assert_eq!(h.held(), ["fine"]);
    assert!(h.scratch_of("odd").metadata().is_err());
}

/// Asserts that a command that fails validation is reported with its place and code and
/// is not acted on, and that a stop beside it still takes effect. It catches an agent
/// that drops a whole reply for one bad command (a stop lost that way is never resent).
#[test]
fn an_invalid_start_costs_the_commands_beside_it_nothing() {
    let mut h = Harness::new("invalid-beside");
    h.start(job("r1"));
    let mut bad = job("bad");
    bad.image = "-v/:/host".to_owned();
    let events = h.apply(vec![
        Command::Start(Box::new(bad)),
        Command::Stop {
            run_id: "r1".to_owned(),
        },
        Command::Unknown,
    ]);
    assert_eq!(names(&events), ["invalid #0 invalid-image"]);
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts that a start whose image is a tag alone (or a digest that is not sha256 of 64
/// hex digits) is reported `image-digest-required`, creates nothing, and costs the start
/// beside it nothing; a tag with a digest starts. It catches an agent that pulls an image
/// whose content can move because an older server sent it.
#[test]
fn a_start_without_a_pinned_image_is_refused() {
    let mut h = Harness::new("unpinned");
    let mut tag = job("tag");
    tag.image = "registry.example/job:1".to_owned();
    let mut short = job("short");
    short.image = format!("registry.example/job@sha256:{}", "ab".repeat(20));
    let mut both = job("both");
    both.image = format!("registry.example/job:1@sha256:{}", "ab".repeat(32));
    let events = h.apply(vec![
        Command::Start(Box::new(tag)),
        Command::Start(Box::new(short)),
        Command::Start(Box::new(both)),
    ]);
    assert_eq!(
        names(&events),
        [
            "invalid #0 image-digest-required",
            "invalid #1 image-digest-required",
            "started both"
        ]
    );
    assert_eq!(h.fake.created().len(), 1);
}

/// Asserts the refusals of a start that does not fit, each with its code and each
/// creating nothing: more CPU than is free, more memory than is free, a GPU index a
/// running job holds, and, with the machine idle, a job larger than the machine and a GPU
/// index the machine does not offer. It catches over-commit, a GPU in two jobs, and a
/// job placed on a device that is not there.
#[test]
fn overcommit_refused() {
    let mut h = Harness::new("overcommit");
    let mut first = job("first");
    first.limits.cpu_millis = 3000;
    first.limits.memory_bytes = 6 << 30;
    first.gpu_indices = vec![1];
    h.start(first);

    let mut cpu = job("cpu");
    cpu.limits.cpu_millis = 1001;
    let mut memory = job("memory");
    memory.limits.memory_bytes = (2 << 30) + 1;
    let mut gpu = job("gpu");
    gpu.gpu_indices = vec![1];
    for spec in [cpu, memory, gpu] {
        let run = spec.run_id.clone();
        let events = names(&h.start(spec));
        assert_eq!(events, [format!("refused {run} no-capacity")]);
        assert!(!h.scratch_of(&run).exists());
    }
    let mut exact = job("exact");
    exact.limits.cpu_millis = 1000;
    exact.limits.memory_bytes = 2 << 30;
    exact.gpu_indices = vec![0];
    assert_eq!(
        names(&h.start(exact)),
        ["started exact"],
        "a job that fits runs"
    );
    assert_eq!(h.fake.created().len(), 2);

    let mut huge = job("huge");
    huge.limits.cpu_millis = 4001;
    let mut fat = job("fat");
    fat.limits.memory_bytes = (8 << 30) + 1;
    let mut ghost = job("ghost");
    ghost.gpu_indices = vec![7];
    for spec in [huge, fat, ghost] {
        let run = spec.run_id.clone();
        let events = names(&h.start(spec));
        assert_eq!(events, [format!("refused {run} limits-exceed-shape")]);
    }
    assert_eq!(h.fake.created().len(), 2);
    assert_eq!(h.life.free().gpus.len(), 0, "both GPUs are booked");
}

/// Asserts that a GPU index is in at most one job at a time across replies, and is free
/// again once the job holding it has been cleaned up. It catches a booking that is
/// forgotten (two jobs on one device) and one that is never returned.
#[test]
fn gpu_index_double_booked_refused() {
    let mut h = Harness::new("gpu-twice");
    let mut one = job("one");
    one.gpu_indices = vec![0, 1];
    let mut two = job("two");
    two.gpu_indices = vec![1];
    assert_eq!(names(&h.start(one)), ["started one"]);
    assert_eq!(names(&h.start(two)), ["refused two no-capacity"]);
    h.fake.exit("one", 0);
    h.life.step();
    assert_eq!(h.life.free().gpus.len(), 2);
    let mut three = job("three");
    three.gpu_indices = vec![1];
    assert_eq!(names(&h.start(three)), ["started three"]);
    assert_eq!(h.life.free().gpus.len(), 1);
    assert_eq!(h.fake.created()[1].gpu_indices, [1]);
}

/// Asserts that a job is killed by the agent's clock once its wall time has passed,
/// counted from the moment its container started (not from the start's arrival), with
/// the configured grace, and reported `timed_out`; and that one millisecond earlier it is
/// left alone. It catches a limit that is never enforced, one that is enforced early,
/// and one measured from the wrong moment.
#[test]
fn wall_limit_kills_and_reports_timed_out() {
    let mut h = Harness::new("wall");
    h.fake.create_takes(5000);
    let mut spec = job("r1");
    spec.limits.wall_ms = 10_000;
    h.start(spec);
    h.clock.advance(9_999);
    assert!(h.life.step().is_empty());
    assert!(h.fake.stops().is_empty());
    h.clock.advance(1);
    let events = names(&h.life.step());
    assert_eq!(events[0], "result r1");
    assert_eq!(h.fake.stops(), [("c1".to_owned(), GRACE)]);
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::TimedOut);
    assert_eq!(result.exit_code, None);
    assert_eq!(GRACE, Duration::from_secs(10));
}

/// Asserts that a job that exited by itself before the agent looked is reported as it
/// exited, even when the agent looks after its wall time. It catches a result rewritten
/// to `timed_out` because the agent was late.
#[test]
fn a_job_that_finished_before_its_limit_is_not_timed_out() {
    let mut h = Harness::new("late-look");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.clock.advance(120_000);
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::Succeeded);
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a runtime that cannot say whether a container has exited does not let
/// the job run past its wall time: the failure is a finding, the limit still stops the
/// job, and the next poll that works finishes it. It catches a limit that depends on the
/// runtime's answering.
#[test]
fn a_runtime_that_cannot_poll_does_not_extend_the_limit() {
    let mut h = Harness::new("poll-fails");
    h.start(job("r1"));
    h.fake.fail("poll", 1);
    let events = names(&h.life.step());
    assert_eq!(events, ["finding r1 poll-failed"]);
    h.fake.fail("poll", 2);
    h.clock.advance(60_000);
    let events = names(&h.life.step());
    assert!(
        events.is_empty(),
        "the finding is reported once: {events:?}"
    );
    assert_eq!(h.fake.stops().len(), 1);
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::TimedOut);
}

/// Asserts that a container the runtime no longer has ends its job: it is reported
/// `failed` with no exit status, goes through the usual cleanup, and gives its slot back;
/// a vanished container of a job being stopped is `killed` and of one past its limit
/// `timed_out`. It catches a job wedged in `running` with its slot booked for good.
#[test]
fn a_container_that_vanishes_ends_its_job() {
    let mut h = Harness::new("vanished");
    h.start(job("r1"));
    h.fake.vanish("r1");
    let events = names(&h.life.step());
    assert_eq!(events[0], "result r1");
    assert_eq!(events[1..], cleanup("r1"));
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.exit_code, None);
    assert_eq!(h.life.free().cpu_millis, 4000);
    assert!(!h.scratch_of("r1").exists());

    h.start(job("r2"));
    h.start(job("r3"));
    h.fake.vanish("r2");
    h.fake.vanish("r3");
    h.stop("r2");
    h.clock.advance(60_000);
    h.life.step();
    let outcomes: Vec<Outcome> = h.life.results().iter().map(|r| r.outcome).collect();
    assert_eq!(
        outcomes,
        [Outcome::Failed, Outcome::Killed, Outcome::TimedOut]
    );
    assert_eq!(h.life.free().cpu_millis, 4000);
}

/// Asserts that starts that are refused cannot grow what the agent holds past what a
/// heartbeat can list: after as many unacknowledged results as the wire allows, the next
/// start is skipped with no record, the held and result lists stay within the limits, and
/// an acknowledgement makes room for it. It catches a server that fills the agent's memory
/// with refused starts, and a heartbeat the server must reject.
#[test]
fn refused_starts_cannot_outgrow_the_wire_limits() {
    let mut h = Harness::new("held-cap");
    let mut spec = job("x");
    spec.isolation = Isolation::Host;
    for chunk in (0..MAX_HELD).collect::<Vec<_>>().chunks(MAX_COMMANDS) {
        let starts = chunk
            .iter()
            .map(|n| {
                let mut one = spec.clone();
                one.run_id = format!("x{n}");
                Command::Start(Box::new(one))
            })
            .collect();
        h.apply(starts);
    }
    assert_eq!(h.life.held().len(), MAX_HELD);
    assert_eq!(h.life.results().len(), MAX_RESULTS);
    spec.run_id = "extra".to_owned();
    assert_eq!(names(&h.start(spec.clone())), ["skipped extra Full"]);
    assert_eq!(h.life.held().len(), MAX_HELD);
    assert_eq!(h.life.results().len(), MAX_RESULTS);
    h.ack("x0", "ACCEPTED");
    assert_eq!(
        names(&h.start(spec)),
        ["refused extra host-jobs-not-allowed"]
    );
    assert_eq!(h.life.held().len(), MAX_HELD);
}

/// Asserts that a stop that the runtime refuses is a finding and is asked for again at
/// the next step, and that once it works the job is reported killed. It catches a stop
/// that is given up on after one failure.
#[test]
fn a_failed_stop_is_retried() {
    let mut h = Harness::new("stop-fails");
    h.start(job("r1"));
    h.fake.fail("stop", 1);
    h.stop("r1");
    assert_eq!(names(&h.life.step()), ["finding r1 stop-failed"]);
    assert!(h.life.results().is_empty());
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);
    assert_eq!(h.fake.stops().len(), 1);
}

/// Asserts that two stops for a run are one stop: the container is stopped once even
/// when it is slow to exit and the step runs again, and that a stop for a run the agent
/// never held is quiet (no event, no call, nothing held). It catches a stop that signals
/// the container on every repeat, and a stop for a stranger that leaves a trace on the
/// wire.
#[test]
fn stop_twice_is_one_stop_and_stop_unknown_run_is_quiet() {
    let mut h = Harness::new("stop-twice");
    h.fake.stop_exits(false);
    h.start(job("r1"));
    h.stop("r1");
    h.stop("r1");
    h.life.step();
    h.stop("r1");
    h.life.step();
    assert_eq!(
        h.fake.stops().len(),
        1,
        "one stop however often it is asked for"
    );
    assert!(
        h.life.results().is_empty(),
        "the container has not exited yet"
    );
    h.fake.exit("r1", 143);
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::Killed);

    let before = h.log.borrow().len();
    let events = h.stop("stranger");
    assert!(events.is_empty());
    assert_eq!(h.log.borrow().len(), before);
    assert_eq!(h.held(), ["r1"], "only the real run is held");
}

/// Asserts that a stop for a run whose container has already exited changes nothing:
/// the result stays what the exit made it. It catches a late stop that rewrites a
/// success as a kill.
#[test]
fn a_stop_after_the_exit_changes_nothing() {
    let mut h = Harness::new("stop-late");
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    h.life.step();
    h.stop("r1");
    h.life.step();
    assert_eq!(h.life.results()[0].outcome, Outcome::Succeeded);
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a stop applied after the container exited, and before the agent looked,
/// reports the job's own exit: success stays `succeeded` with status 0, a failure stays
/// `failed` with its status, and no stop is sent to a container that has finished. It
/// catches finished work reported as killed because a stop raced the poll.
#[test]
fn a_stop_that_beats_the_look_does_not_relabel_a_finished_job() {
    let mut h = Harness::new("stop-beats-look");
    h.start(job("ok"));
    h.start(job("bad"));
    h.fake.exit("ok", 0);
    h.fake.exit("bad", 3);
    h.stop("ok");
    h.stop("bad");
    h.life.step();
    let results = h.life.results();
    assert_eq!(
        (results[0].outcome, results[0].exit_code),
        (Outcome::Succeeded, Some(0))
    );
    assert_eq!(
        (results[1].outcome, results[1].exit_code),
        (Outcome::Failed, Some(3))
    );
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a job past its wall time whose stop the runtime refused, and which then
/// exits by itself, is reported as it exited: what happened is reported, not what the
/// agent meant to do. It catches a late success reported as `timed_out`.
#[test]
fn a_timeout_whose_stop_failed_reports_a_later_exit_as_it_was() {
    let mut h = Harness::new("timeout-stop-fails");
    h.start(job("r1"));
    h.fake.fail("stop", 1);
    h.clock.advance(60_000);
    assert_eq!(names(&h.life.step()), ["finding r1 stop-failed"]);
    h.fake.exit("r1", 0);
    h.life.step();
    let result = &h.life.results()[0];
    assert_eq!(
        (result.outcome, result.exit_code),
        (Outcome::Succeeded, Some(0))
    );
    assert!(h.fake.stops().is_empty());
}

/// Asserts that a cancel, even after a stop, reports nothing, and that a stop after a
/// cancel does not take the cancel back: the container is stopped and cleaned and the
/// slot freed, but no result is held. It catches a cancelled run that is reported to a
/// server that no longer holds its lease.
#[test]
fn a_cancel_reports_nothing_even_after_a_stop() {
    let mut h = Harness::new("cancel");
    h.start(job("r1"));
    h.stop("r1");
    h.cancel("r1");
    let events = names(&h.life.step());
    assert_eq!(events, cleanup("r1"));
    assert!(h.life.results().is_empty());
    assert!(h.held().is_empty(), "nothing is owed, so nothing is held");
    assert_eq!(h.life.free().cpu_millis, 4000);

    h.start(job("r2"));
    h.cancel("r2");
    h.stop("r2");
    h.life.step();
    assert!(h.life.results().is_empty(), "a stop after a cancel reports");
    assert!(h.held().is_empty());
}

/// Asserts that the container is created with the job's own labels and the agent's over
/// them, its limits, its resolved scratch directory and its GPUs. It catches a runtime
/// handed the wrong limits, and an agent label that a job could override.
#[test]
fn the_container_is_created_with_the_jobs_limits_and_the_agents_labels() {
    let mut h = Harness::new("spec");
    let mut spec = job("r1");
    spec.gpu_indices = vec![1, 0];
    spec.labels.insert("team".to_owned(), "infra".to_owned());
    h.start(spec);
    let made = &h.fake.created()[0];
    assert_eq!(
        made.image,
        "registry.example/job:1@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(made.args, ["--go"]);
    assert_eq!((made.cpu_millis, made.memory_bytes), (1000, 1 << 30));
    assert_eq!(made.pids, Some(100));
    assert_eq!(made.gpu_indices, [1, 0]);
    assert_eq!(made.scratch, h.scratch_of("r1"));
    let labels = &made.labels;
    assert_eq!(labels["team"], "infra");
    assert_eq!(labels[label::RUN], "r1");
    assert_eq!(labels[label::LEASE], "lease-r1");
    assert_eq!(labels[label::GPUS], "1,0");
    assert_eq!(labels[label::WALL_MS], "60000");
}

/// Asserts that two harnesses built under one name are two worlds: the second one's
/// set-up does not remove the first one's scratch root, and a start on the first one
/// still makes its scratch directory. It catches a test harness whose directory is
/// named by the test alone, so that two tests of one name (or one test run twice at once)
/// delete each other's roots under a running agent.
#[test]
fn harnesses_of_one_name_do_not_share_a_scratch_root() {
    let mut first = Harness::new("same-name");
    let mut second = Harness::new("same-name");
    assert_ne!(first.dir.path("scratch"), second.dir.path("scratch"));
    assert_eq!(names(&first.start(job("r1"))), ["started r1"]);
    assert!(first.scratch_of("r1").is_dir());
    assert_eq!(names(&second.start(job("r1"))), ["started r1"]);
    assert!(second.scratch_of("r1").is_dir());
    assert_ne!(first.scratch_of("r1"), second.scratch_of("r1"));
}
