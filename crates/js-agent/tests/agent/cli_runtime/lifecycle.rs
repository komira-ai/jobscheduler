//! The lifecycle's rules, driven over `CliRuntime` and the scripted command line instead
//! of the in-memory fake: the same jobs, the same results, the same order of cleanup.

use js_agent::lifecycle::{Config, Event, Lifecycle, NoChecks, label};
use js_agent_wire::{Command, Outcome};

use super::scripted::Scripted;
use super::{HOST, docker, made_at, open_at, thirty_seconds_on};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{GRACE, cleanup, config_in, job, names, reply};
use crate::support::Scratch;

/// A lifecycle over a runtime over `docker`, on `os`.
struct World {
    life: Lifecycle,
    docker: Scripted,
    clock: ManualClock,
    config: Config,
    _dir: Scratch,
}

impl World {
    fn new(name: &str, os: &str) -> Self {
        let dir = Scratch::new(&format!("cli-life-{name}"));
        let config = config_in(&dir, false);
        let (docker, clock) = (docker(), ManualClock::default());
        let (life, _) = Lifecycle::adopt(
            config.clone(),
            Box::new(open_at(&docker, os, made_at)),
            Box::new(NoChecks),
            Box::new(clock.clone()),
        )
        .unwrap();
        Self {
            life,
            docker,
            clock,
            config,
            _dir: dir,
        }
    }

    fn start(&mut self, run_id: &str) -> Vec<Event> {
        let spec = job(run_id);
        self.life
            .apply(&reply(vec![Command::Start(Box::new(spec))]))
    }

    fn outcomes(&self) -> Vec<Outcome> {
        self.life.results().iter().map(|r| r.outcome).collect()
    }
}

/// Asserts a job's whole life over the command line: the container is created with the
/// run's labels and the agent's, the exit is read from `inspect`, the result is ready
/// before the cleanup, and the cleanup removes the container and gives the slot back.
/// It catches a backend that breaks the order the lifecycle relies on.
#[test]
fn a_job_runs_to_its_result_and_is_cleaned_up() {
    let mut w = World::new("whole", "linux");
    assert_eq!(names(&w.start("r1")), ["started r1"]);
    assert_eq!(w.life.free().cpu_millis, 3000);
    let args = super::created(&w.docker, "r1");
    assert!(args.contains(&format!("{}=r1", label::RUN)));
    assert!(args.contains(&format!("{}={HOST}", label::AGENT)));
    assert!(w.life.step().is_empty(), "it runs");
    w.docker.exit("r1", 0);
    let events = names(&w.life.step());
    assert_eq!(events[0], "result r1");
    assert_eq!(events[1..], cleanup("r1"));
    assert_eq!(w.outcomes(), [Outcome::Succeeded]);
    assert!(w.docker.ids().is_empty(), "the container was removed");
    assert_eq!(w.life.free().cpu_millis, 4000);

    w.start("r2");
    w.docker.exit("r2", 7);
    w.start("r3");
    w.docker.exit_oom("r3");
    w.life.step();
    assert_eq!(
        w.outcomes(),
        [Outcome::Succeeded, Outcome::Failed, Outcome::OutOfMemory]
    );
}

/// Asserts that a stop and a wall-time limit go to the runtime as `stop` with the
/// configured grace, and end as `killed` and `timed_out`. It catches a limit that is not
/// enforced when the runtime is the command line.
#[test]
fn a_stop_and_the_wall_limit_stop_the_container() {
    let mut w = World::new("stop", "linux");
    w.start("r1");
    w.start("r2");
    w.life.apply(&reply(vec![Command::Stop {
        run_id: "r1".to_owned(),
    }]));
    w.life.step();
    w.clock.advance(60_000);
    w.life.step();
    assert_eq!(w.outcomes(), [Outcome::Killed, Outcome::TimedOut]);
    let stops = w.docker.calls_of("stop");
    assert_eq!(stops.len(), 2);
    assert_eq!(
        stops[0][..3],
        ["stop", "--time", "10"],
        "the grace is {GRACE:?}"
    );
    assert!(w.docker.ids().is_empty());
    assert_eq!(w.life.free().cpu_millis, 4000);
}

/// Asserts that a container removed from outside ends its job as `failed` and frees the
/// slot (the runtime says it is gone, which is not an error). It catches a job wedged
/// in `running` for ever.
#[test]
fn a_container_removed_from_outside_ends_its_job() {
    let mut w = World::new("vanished", "linux");
    w.start("r1");
    w.docker.vanish("r1");
    let events = names(&w.life.step());
    assert_eq!(events[0], "result r1");
    assert_eq!(w.outcomes(), [Outcome::Failed]);
    assert_eq!(w.life.free().cpu_millis, 4000);
}

/// Asserts that a `create` that fails ends the start as `failed_to_start` with the
/// runtime's detail, with nothing left in the runtime and the slot free, and that a
/// `start` that fails does the same. It catches a leaked container or a booked slot.
#[test]
fn a_failed_create_or_start_leaves_nothing() {
    let mut w = World::new("create-fails", "linux");
    w.docker.fail("create", 1);
    w.start("r1");
    w.docker.fail("start", 1);
    w.start("r2");
    assert_eq!(
        w.outcomes(),
        [Outcome::FailedToStart, Outcome::FailedToStart]
    );
    let details: Vec<Option<String>> = w.life.results().into_iter().map(|r| r.detail).collect();
    let failed = Some("runtime-failed".to_owned());
    assert_eq!(details, [failed.clone(), failed]);
    assert!(w.docker.ids().is_empty());
    w.life.step();
    assert_eq!(w.life.free().cpu_millis, 4000, "freed by the cleanup");
}

/// Asserts that a GPU job on a Mac is refused as `failed_to_start` with the code
/// `gpu-unsupported` and that nothing reaches the runtime but the version call, while a
/// job without a GPU runs there, and the same GPU job runs on Linux. It catches a GPU
/// limit that is silently dropped on a host whose containers cannot reach the GPU.
#[test]
fn gpu_limit_on_macos_is_failed_to_start_with_a_code() {
    let mut mac = World::new("mac-gpu", "macos");
    let mut spec = job("g1");
    spec.gpu_indices = vec![0];
    let events = mac
        .life
        .apply(&reply(vec![Command::Start(Box::new(spec.clone()))]));
    assert_eq!(names(&events), ["refused g1 gpu-unsupported"]);
    let result = &mac.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("gpu-unsupported"));
    for sub in ["create", "start"] {
        assert!(mac.docker.calls_of(sub).is_empty(), "{sub} was called");
    }
    assert_eq!(mac.life.free().cpu_millis, 4000);
    assert_eq!(names(&mac.start("c1")), ["started c1"]);

    let mut linux = World::new("linux-gpu", "linux");
    let events = linux
        .life
        .apply(&reply(vec![Command::Start(Box::new(spec))]));
    assert_eq!(names(&events), ["started g1"]);
}

/// Asserts the restart: a new process adopts the containers carrying the run label and
/// this agent's label, lists them first, and counts the time they ran before the restart
/// against the wall limit (the container was made 30 seconds before the new runtime
/// started, so 30 seconds of the minute remain); another agent's container on the same
/// runtime is not adopted. It catches a restart that gives every job a fresh limit and
/// one that adopts a neighbour's job.
#[test]
fn a_restart_adopts_its_own_containers_with_their_age() {
    let mut w = World::new("restart", "linux");
    w.start("r1");
    w.docker.leak(&[
        (label::RUN, "theirs"),
        (label::LEASE, "lease-x"),
        (label::AGENT, "h-other"),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::CREATED_MS, "1000000"),
    ]);
    let (mut life, events) = Lifecycle::adopt(
        w.config.clone(),
        Box::new(open_at(&w.docker, "linux", thirty_seconds_on)),
        Box::new(NoChecks),
        Box::new(w.clock.clone()),
    )
    .unwrap();
    assert_eq!(names(&events), ["adopted r1"]);
    assert_eq!(life.free().cpu_millis, 3000);
    w.clock.advance(29_999);
    assert!(life.step().is_empty());
    w.clock.advance(1);
    life.step();
    let results = life.results();
    assert_eq!(results[0].outcome, Outcome::TimedOut);
    assert_eq!(results[0].run_id, "r1");
    assert_eq!(results.len(), 1);
}

/// Asserts that a `create` whose outcome is unknown leaves nothing a restarted agent
/// could take for a running job. The call is killed after the daemon made the container
/// and the removal by name fails too, so the container stays, labelled as the agent's: the
/// start is reported `failed_to_start`, and the next process, which lists the containers
/// first, finds it never started, removes it and adopts nothing. It catches the orphan
/// that held its booking until the wall time and was then reported `timed_out`.
#[test]
fn a_restart_does_not_adopt_what_a_killed_create_left() {
    let mut w = World::new("orphan", "linux");
    w.docker.ambiguous_create(Err(()));
    w.docker.fail("rm", 1);
    w.start("r1");
    assert_eq!(w.outcomes(), [Outcome::FailedToStart]);
    assert_eq!(w.docker.ids().len(), 1, "the cleanup failed too");
    let (life, events) = Lifecycle::adopt(
        w.config.clone(),
        Box::new(open_at(&w.docker, "linux", thirty_seconds_on)),
        Box::new(NoChecks),
        Box::new(w.clock.clone()),
    )
    .unwrap();
    assert_eq!(names(&events), ["finding - unadoptable-container"]);
    assert!(w.docker.ids().is_empty(), "the orphan was removed");
    assert_eq!(life.free().cpu_millis, 4000, "nothing is booked for it");

    let mut clean = World::new("orphan-clean", "linux");
    clean.docker.ambiguous_create(Err(()));
    clean.start("r2");
    assert_eq!(clean.outcomes(), [Outcome::FailedToStart]);
    assert!(clean.docker.ids().is_empty(), "removed at once, by name");
}

/// Asserts that an orphan the engine cannot remove (a container created and never
/// started, labelled as the agent's) does not stop a restarted agent from starting: the
/// listing succeeds, the running job is adopted, the orphan is evicted, reported as kept
/// and booked (its labels say what), and a later step that can remove it frees the
/// booking. It catches one undeletable orphan that makes every restart fail to adopt.
#[test]
fn an_orphan_that_cannot_be_removed_is_booked_and_retried() {
    let mut w = World::new("orphan-stuck", "linux");
    w.start("r1");
    let orphan = w.docker.leak_created(&[
        (label::RUN, "o1"),
        (label::LEASE, "lease-o"),
        (label::AGENT, HOST),
        (label::CPU_MILLIS, "1000"),
        (label::MEMORY_BYTES, "1073741824"),
        (label::GPUS, ""),
        (label::WALL_MS, "60000"),
        (label::ISOLATION, "contained"),
        (label::CREATED_MS, "1000000"),
    ]);
    w.docker.fail("rm", 1000);
    let (mut life, events) = Lifecycle::adopt(
        w.config.clone(),
        Box::new(open_at(&w.docker, "linux", thirty_seconds_on)),
        Box::new(NoChecks),
        Box::new(w.clock.clone()),
    )
    .unwrap();
    assert_eq!(
        names(&events),
        [
            "adopted r1",
            "tainted - cleanup-failed",
            "finding - unadoptable-container-kept"
        ]
    );
    assert!(w.docker.ids().contains(&orphan));
    assert_eq!(life.free().cpu_millis, 2000, "the orphan is booked");
    assert_eq!(names(&life.step()), Vec::<String>::new(), "still stuck");

    w.docker.fail("rm", 0);
    assert_eq!(names(&life.step()), ["finding - unadoptable-container"]);
    assert!(!w.docker.ids().contains(&orphan));
    assert_eq!(life.free().cpu_millis, 3000, "the booking was freed");
}

/// Asserts that a container the engine calls `dead` (it failed to remove it) ends its job
/// as failed whatever exit code it kept, a code of 0 included, and never as succeeded.
/// It catches work reported done that may not have finished.
#[test]
fn a_dead_container_never_reports_success() {
    let mut w = World::new("dead", "linux");
    w.start("r1");
    w.docker.say("inspect", "dead 0 false");
    w.life.step();
    assert_eq!(w.outcomes(), [Outcome::Failed]);
    assert_eq!(w.life.results()[0].exit_code, None);

    let mut coded = World::new("dead-coded", "linux");
    coded.start("r2");
    coded.docker.say("inspect", "dead 2 false");
    coded.life.step();
    let results = coded.life.results();
    assert_eq!(results[0].outcome, Outcome::Failed);
    assert_eq!(results[0].exit_code, Some(2));
}
