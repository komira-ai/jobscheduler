//! The worker of an agent that was given node-local mount providers
//! (`docs/design/mount-providers.md`): the lifecycle it builds is given them, once it has
//! adopted what an earlier process left, and a machine that cannot make their client ends the
//! start of the worker with the reason.

use std::path::Path;

use js_agent::Worker;
use js_agent::lifecycle::{MountProviders, UserNames};

use js_agent::Delivery;
use js_agent_wire::HeldState;

use super::fake::{FakeMachine, HANG_GUARD};
use super::{STEP_NEVER, job_start, plan, start, starts};
use crate::support::Scratch;

/// Asserts that a worker given a provider asks it for its sweep as it starts, naming the
/// host id the agent registered under and keeping no run (nothing was adopted), before it has
/// been given any order, and that a worker given none makes no call to any provider. Catches
/// a worker that builds its lifecycle without the providers the flags named, so a start
/// naming one would be refused, and a sweep that never leaves the host's providers clean.
#[test]
fn a_worker_given_providers_sweeps_them_as_it_starts() {
    let dir = Scratch::new("jobs-providers-sweep");
    let machine = FakeMachine::new();
    let mut given = plan(&dir, &machine, STEP_NEVER);
    let sockets = [("tier".to_owned(), dir.path("tier.sock"))];
    given.providers = MountProviders::from_flags(
        &sockets,
        &[],
        Some(30),
        UserNames::Passwd(Path::new("/nonexistent")),
    )
    .unwrap();
    let worker = match Worker::start(given) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    assert_eq!(machine.provider_calls(), ["h1 tier Sweep { keep: [] }"]);
    drop(worker);

    let quiet = FakeMachine::new();
    let other_dir = Scratch::new("jobs-providers-none");
    let worker = start(&other_dir, &quiet);
    assert!(quiet.provider_calls().is_empty());
    drop(worker);
}

/// Asserts that a worker whose machine cannot make the providers' client does not start, and
/// says why. Catches an agent that registers with providers it cannot call.
#[test]
fn a_machine_that_cannot_make_the_providers_ends_the_start_of_the_worker() {
    let dir = Scratch::new("jobs-providers-fail");
    let machine = FakeMachine::new().without_providers("no client is made");
    let mut given = plan(&dir, &machine, STEP_NEVER);
    let sockets = [("tier".to_owned(), dir.path("tier.sock"))];
    given.providers = MountProviders::from_flags(
        &sockets,
        &[],
        None,
        UserNames::Passwd(Path::new("/nonexistent")),
    )
    .unwrap();
    match Worker::start(given) {
        Ok(_) => panic!("the worker started"),
        Err(why) => assert_eq!(why, "--mount-provider: no client is made"),
    }
    assert!(machine.provider_calls().is_empty());
}

/// Asserts that the providers a worker is given ring that worker: a start waits for its image,
/// whose pull ends without the runtime ringing, and the providers' ring alone makes the worker
/// step and start it, though it has no step interval to fall back on. Catches providers given
/// no wake (or a wake given only after they were attached), so a start waiting on a provider's
/// answer goes on only at the next step interval.
#[test]
fn the_providers_of_a_worker_ring_it() {
    let dir = Scratch::new("jobs-providers-wake");
    let machine = FakeMachine::new();
    let mut given = plan(&dir, &machine, STEP_NEVER);
    let sockets = [("tier".to_owned(), dir.path("tier.sock"))];
    given.providers = MountProviders::from_flags(
        &sockets,
        &[],
        Some(30),
        UserNames::Passwd(Path::new("/nonexistent")),
    )
    .unwrap();
    let worker = match Worker::start(given) {
        Ok(worker) => worker,
        Err(why) => panic!("the worker did not start: {why}"),
    };
    let image = job_start("r1").image;
    machine.world.begin_pull(&image);
    assert_eq!(worker.try_deliver(starts(&["r1"])), Delivery::Delivered);
    assert!(worker.handle().settle());
    assert_eq!(worker.snapshot().jobs[0].state, HeldState::Starting);

    machine.world.end_pull(&image, false);
    machine.ring_providers();
    let running = |s: &js_agent::Snapshot| s.jobs.iter().any(|h| h.state == HeldState::Running);
    assert!(worker.handle().wait_until(&running, HANG_GUARD));
    assert_eq!(machine.world.created(), ["r1"]);
}
