//! The worker of an agent that was given node-local mount providers
//! (`docs/design/mount-providers.md`): the lifecycle it builds is given them, once it has
//! adopted what an earlier process left, and a machine that cannot make their client ends the
//! start of the worker with the reason.

use js_agent::Worker;
use js_agent::lifecycle::MountProviders;

use super::fake::FakeMachine;
use super::{STEP_NEVER, plan, start};
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
    given.providers = MountProviders::from_flags(&sockets, Some(30)).unwrap();
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
    given.providers = MountProviders::from_flags(&sockets, None).unwrap();
    match Worker::start(given) {
        Ok(_) => panic!("the worker started"),
        Err(why) => assert_eq!(why, "no client is made"),
    }
    assert!(machine.provider_calls().is_empty());
}
