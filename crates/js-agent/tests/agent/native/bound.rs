//! What bounds a native run through the helper: the soft stop (`SIGTERM`, then a kill after
//! the grace) and the deadline the helper holds itself, so it holds while no agent runs; and
//! both against a helper from before them, which the runtime falls back from.

use std::time::Duration;

use js_agent::lifecycle::{Exit, Runtime, Status, label};

use super::fake_helper::FakeHelper;
use super::{POLLS, Rig, native};

/// Waits until the fake helper has done `signal` to a run.
fn until_signalled(fake: &FakeHelper, signal: &str) {
    for _ in 0..POLLS {
        if fake.signals().iter().any(|done| done == signal) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{signal} never came: {:?}", fake.signals());
}

/// How many requests the fake refused as ones it cannot read (it plays an old helper).
fn unreadable(fake: &FakeHelper) -> usize {
    fake.verbs()
        .iter()
        .filter(|verb| *verb == "unreadable")
        .count()
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Starts `r1` with a wall limit of a minute in its labels.
fn start_walled(rig: &mut Rig) -> js_agent::lifecycle::RunHandle {
    let mut spec = rig.spec("r1", native("/bin/true", &[]));
    spec.labels
        .insert(label::WALL_MS.to_owned(), "60000".to_owned());
    rig.rt.create_and_start(&spec).unwrap()
}

/// Asserts the soft stop: the helper sees the `SIGTERM` first, and the kill only once its
/// clock has passed the grace the stop named, not a millisecond before; the run ends by the
/// kill. It catches a stop that kills at once (a job never gets to flush) and one that never
/// kills a job that ignores the `SIGTERM`.
#[test]
fn a_stop_terms_first_and_kills_only_after_the_grace() {
    let mut rig = Rig::new("bound-grace");
    rig.fake.ignore_term();
    let id = rig.start("r1", "/bin/true", &[]);
    let (rt, fake) = (&mut rig.rt, &rig.fake);
    std::thread::scope(|scope| {
        let stopping = scope.spawn(|| rt.stop(&id, Duration::from_secs(5)));
        until_signalled(fake, "term 7.0");
        fake.advance(4_999);
        assert_eq!(fake.signals(), ["term 7.0"], "killed inside the grace");
        fake.advance(1);
        stopping.join().unwrap().unwrap();
    });
    assert_eq!(rig.fake.signals(), ["term 7.0", "kill 7.0"]);
    assert_eq!(rig.settle(&id), exited(137));
}

/// A runtime over the same fake, to hold the rig's place while its own is rebuilt.
fn dummy(fake: &FakeHelper) -> js_agent::native::NativeRuntime {
    js_agent::native::NativeRuntime::open(fake.client(), super::config(fake), super::TERM + 50)
        .unwrap()
}

/// Asserts that a job that exits on its `SIGTERM` is never killed: the stop returns once it
/// has ended, and the run ends by the `SIGTERM`. It catches a stop that kills every job
/// whatever it does with the grace.
#[test]
fn a_job_that_exits_on_sigterm_is_never_killed() {
    let mut rig = Rig::new("bound-soft");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.rt.stop(&id, Duration::from_secs(3600)).unwrap();
    assert_eq!(rig.fake.signals(), ["term 7.0"]);
    assert_eq!(rig.settle(&id), exited(143));
    let kills = rig.fake.verbs().into_iter().filter(|v| v == "kill-uid 7.0");
    assert_eq!(kills.count(), 0);
}

/// Asserts the fallback from a helper that has no `stop`: the stop is refused as a request
/// it cannot read, and the runtime kills the user at once instead, now and on every later
/// stop without asking for `stop` again. It catches a new agent that can no longer stop a
/// run on a helper that was not updated with it.
#[test]
fn a_helper_without_stop_is_killed_through_instead() {
    let mut rig = Rig::new("bound-old-stop");
    let id = rig.start("r1", "/bin/true", &[]);
    rig.fake.be_old();
    rig.rt.stop(&id, Duration::from_secs(5)).unwrap();
    assert_eq!(rig.fake.verbs().last().unwrap(), "kill-uid 7.0");
    assert_eq!(rig.settle(&id), exited(137));
    rig.rt.stop(&id, Duration::from_secs(5)).unwrap();
    let verbs = rig.fake.verbs();
    assert_eq!(verbs[verbs.len() - 2..], ["kill-uid 7.0", "kill-uid 7.0"]);
    assert_eq!(
        unreadable(&rig.fake),
        1,
        "stop is not asked of the old helper again"
    );
    assert!(rig.fake.signals().is_empty());
}

/// Asserts the deadline the helper holds: a start whose labels carry a wall limit of a
/// minute, on a runtime whose stop grace is five seconds, asks the helper for a deadline of
/// 65 seconds; with the agent object dropped, the helper ends the run once its clock passes
/// that, not a millisecond before, and the journal records the deadline. It catches a native
/// run that goes on with no bound while no agent runs.
#[test]
fn the_helper_ends_a_run_at_its_deadline_with_no_agent() {
    let mut rig = Rig::new("bound-deadline");
    rig.rt =
        std::mem::replace(&mut rig.rt, dummy(&rig.fake)).with_stop_grace(Duration::from_secs(5));
    start_walled(&mut rig);
    assert_eq!(rig.fake.run_of("7.0").millis, Some(65_000));
    let journal = js_agent::native::Journal::new(&rig.dir.path("native-leases"));
    assert_eq!(journal.runs().unwrap()["7.0"].deadline_ms, Some(65_000));
    let super::Rig {
        fake,
        rt,
        dir: _dir,
    } = rig;
    drop(rt);
    fake.advance(64_999);
    assert!(fake.signals().is_empty(), "ended before its deadline");
    fake.advance(1);
    for _ in 0..POLLS {
        if !fake.signals().is_empty() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(fake.signals(), ["deadline 7.0"]);
}

/// Asserts the fallback from a helper that holds no deadline: the `run` with the deadline is
/// refused as a request it cannot read, the runtime starts the run again without it (once:
/// the helper saw one `run`), the journal records no deadline, the helper's clock passing
/// the wall limit ends nothing, and the agent's own stop still ends the run. It catches a
/// new agent that cannot start a run on a helper that was not updated with it.
#[test]
fn a_helper_without_deadlines_runs_the_job_and_the_agent_bounds_it() {
    let mut rig = Rig::new("bound-old-deadline");
    rig.fake.be_old();
    let id = start_walled(&mut rig);
    let runs: Vec<_> = rig
        .fake
        .seen()
        .into_iter()
        .filter(|one| one.verb == "run")
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].millis, None);
    assert_eq!(unreadable(&rig.fake), 1);
    let journal = js_agent::native::Journal::new(&rig.dir.path("native-leases"));
    assert_eq!(journal.runs().unwrap()["7.0"].deadline_ms, None);
    rig.fake.advance(3_600_000);
    assert_eq!(rig.rt.poll(&id).unwrap(), Status::Running);
    let mut second = rig.spec("r2", native("/bin/true", &[]));
    second
        .labels
        .insert(label::WALL_MS.to_owned(), "60000".to_owned());
    rig.rt.create_and_start(&second).unwrap();
    let runs = rig
        .fake
        .seen()
        .into_iter()
        .filter(|one| one.verb == "run")
        .count();
    assert_eq!(runs, 2);
    assert_eq!(
        unreadable(&rig.fake),
        1,
        "the deadline is not offered again"
    );
    rig.rt.stop(&id, Duration::ZERO).unwrap();
    assert_eq!(rig.settle(&id), exited(137));
}

/// Asserts that on a helper that holds no deadline the agent's own wall limit still ends a
/// native job: it is stopped (through `kill-uid`, the old helper having no `stop`) and
/// reported timed out. It catches a job that runs for ever because the helper ignored the
/// deadline the agent meant it to hold.
#[test]
fn the_agents_wall_limit_holds_on_a_helper_without_deadlines() {
    let mut h = super::lifecycle::Native::new("bound-old-wall");
    h.fake.be_old();
    assert_eq!(
        h.start(super::lifecycle::job("r1", "/bin/sleep")),
        ["started r1"]
    );
    h.clock.advance(60_000);
    h.until(|life| !life.results().is_empty());
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, js_agent_wire::Outcome::TimedOut);
    let verbs = h.fake.verbs();
    assert!(verbs.contains(&"kill-uid 7.0".to_owned()), "{verbs:?}");
}
