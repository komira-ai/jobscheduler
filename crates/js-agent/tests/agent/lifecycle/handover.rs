//! When the jobs let the agent's process end and leave them to the next one
//! ([`Lifecycle::quiet_for_handover`]): every job running with no stop asked of it, or done
//! with its checks; never with a start, a stop or a finished job's cleanup under way.

use js_agent::lifecycle::Lifecycle;

use super::deferring::Deferring;
use super::{Harness, job};

fn quiet(life: &Lifecycle) -> bool {
    life.quiet_for_handover()
}

/// Asserts each point of a job's life: quiet with nothing held and with the job running; not
/// while its create is under way (it would be lost: nothing of it is recorded as running);
/// quiet once it runs; not while the container's removal of its finished job is under way
/// (its checks are owed then); quiet again once the checks have run and only its result is
/// held. Catches a handover allowed with a start half made or a check owed.
#[test]
fn a_start_or_a_cleanup_under_way_keeps_the_jobs_from_being_handed_over() {
    let (mut h, gate) = Deferring::harness("handover-quiet");
    assert!(quiet(&h.life), "nothing held");
    gate.hold("r1");
    assert!(h.start(job("r1")).is_empty());
    assert!(!quiet(&h.life), "the create is under way");
    gate.release("r1");
    h.life.step();
    assert_eq!(h.fake.created().len(), 1);
    assert!(quiet(&h.life), "it runs");

    gate.hold_removal("c1");
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(
        !quiet(&h.life),
        "its removal is under way and its checks owed"
    );
    gate.release_removal("c1");
    h.life.step();
    assert_eq!(h.life.results().len(), 1);
    assert!(h.life.held().iter().all(|run| run.run_id == "r1"));
    assert!(quiet(&h.life), "only its result is held");
}

/// Asserts that a stop asked of a running job keeps the jobs from being handed over until
/// the job has ended and its checks have run, and that a cleanup stuck on a removal that
/// keeps failing keeps them too. Catches a handover that leaves a stop half done, which the
/// next process does not know of, or a cleanup with its checks owed, which the next process
/// must then check at its start.
#[test]
fn a_stop_asked_or_a_cleanup_that_fails_keeps_the_jobs_from_being_handed_over() {
    let mut h = Harness::new("handover-stop");
    h.start(job("r1"));
    h.start(job("r2"));
    assert!(quiet(&h.life));
    h.stop("r1");
    assert!(!quiet(&h.life), "a stop is asked of r1");
    h.life.step();
    assert!(quiet(&h.life), "r1 stopped and checked, r2 runs");

    h.fake.fail("remove", u32::MAX);
    h.fake.exit("r2", 0);
    h.life.step();
    h.life.step();
    assert!(!quiet(&h.life), "r2's cleanup cannot go on");
}
