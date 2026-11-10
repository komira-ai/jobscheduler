//! The whole-host prepare family: the ways the whole-host path of jobs meets a host that
//! does not become ready, over the server's whole path (its job service, the queue, the
//! placement, the prepare deadline and its sweep in the tick, the back-off of a failed
//! host, and agents that prepare, refuse, stay silent or restart), under every invariant
//! of `Invariant::ALL`. Each runs over [`SEEDS`] seeds.
//!
//! Each states only what the server does today and the design also promises
//! (`crates/js-lease-server/src/job_whole.rs`, design section 8, rows 15, 26 and 39): a
//! silent host fails the prepare at the server's deadline and the job goes to another host,
//! a job fails to start after `max_replace` failed prepares, a refusal is answered as its
//! reason says, a ready report racing the deadline's sweep never double-books, an agent's
//! hello restarts the prepare clock, and a cancel while the host prepares is cancelled
//! once the agent leaves `preparing`. The deadline is the server's own, 600 s (the pool
//! file no longer sets it), so these scenarios run for virtual time of that order.
//!
//! The scenarios and their sizes are in `cell/whole_host_prepare.rs`.

#[path = "cell/whole_host_prepare.rs"]
mod whole_host_prepare;

use js_agent_wire::RefusalReason;
use js_sim_cell::actions::REFUSAL_REASONS;
use js_sim_cell::{Expect, ScenarioFailure};

/// Every scenario of the family runs over this many seeds.
const SEEDS: u64 = 16;

/// The scenarios of fifty hosts run over this many.
const SCALE_SEEDS: u64 = 2;

/// Each step of the race's sweep runs over this many seeds: which side of the deadline the
/// ready report lands on is set by the step's preparation time, and the sweep's thirteen
/// steps are the race's spread.
const RACE_SEEDS: u64 = 4;

/// A host whose agent never answers its prepare fails it at the server's deadline, 600 s
/// from the placement, and the job is placed again on the other host, which the failed host
/// is barred from for the back-off; agents made ready then take the next try, and the job
/// succeeds on its second allocation (design section 8, row 15). Catches a job left on the
/// silent host past its deadline, a job ended at the first failed prepare, and a retry on
/// the host that just failed.
#[test]
fn a_silent_host_fails_the_prepare_and_the_job_goes_to_another_host() {
    whole_host_prepare::a_silent_host(whole_host_prepare::SILENT_HOSTS).run_seeds(SEEDS);
}

/// The silent host among fifty idle ones, every agent silent: the job's first try ends at
/// the deadline, it goes to a host the failed one is barred from, and the ready agents take
/// the next try, as with two hosts, over two seeds (the world is twenty-five times the size).
/// Catches a retry that depends on the pool being small, and a failed host offered again
/// within the bar among many.
#[test]
fn a_silent_host_among_fifty_fails_the_prepare_and_the_job_goes_to_another_host() {
    whole_host_prepare::a_silent_host(whole_host_prepare::SCALE_HOSTS).run_seeds(SCALE_SEEDS);
}

/// A job whose three tries all end at the deadline (silent hosts) fails to start with
/// `exclusive-not-ready`, the client hears it fail, and no allocation is left booked.
/// Catches a fourth try, a job ended early, and an allocation or host left preparing.
#[test]
fn a_job_is_terminal_after_max_replace_silent_prepares() {
    let hosts = whole_host_prepare::LOST_HOSTS;
    whole_host_prepare::a_job_is_terminal_after_max_replace_silent_prepares(hosts).run_seeds(SEEDS);
}

/// The same among fifty silent hosts, over two seeds: three tries on three hosts, then
/// `exclusive-not-ready`, with no allocation left booked on any host. Catches a count of
/// failed prepares kept per host rather than per job, which a large pool would never reach.
#[test]
fn a_job_is_terminal_after_max_replace_silent_prepares_among_fifty_hosts() {
    let hosts = whole_host_prepare::SCALE_HOSTS;
    whole_host_prepare::a_job_is_terminal_after_max_replace_silent_prepares(hosts)
        .run_seeds(SCALE_SEEDS);
}

/// A silent host with no other host to go to is placed the job again only after the
/// back-off: each of the job's three allocations is created at least the deadline plus the
/// back-off after the one before, and the job then fails to start with
/// `exclusive-not-ready` (design section 8, row 15). Catches a failed host offered to the
/// job again at once, and a job never placed again.
#[test]
fn a_silent_host_is_left_out_of_the_retry_for_reserve_backoff() {
    whole_host_prepare::a_silent_host_is_left_out_of_the_retry_for_the_backoff().run_seeds(SEEDS);
}

/// A host that refuses its prepare, for each reason the wire has, is not given the job
/// again within the bar and the job is placed on another host; a `host-unsafe` refusal taints
/// the host, which stays out of service, while the other reasons leave every host serving.
/// Catches a refusal that ends the job, a retry on the refusing host, a host not tainted
/// after an unsafe refusal and one tainted after another reason.
#[test]
fn a_refused_prepare_goes_elsewhere_for_each_refusal_reason() {
    for (_, scenario) in whole_host_prepare::every_refusal() {
        scenario.run_seeds(SEEDS);
    }
}

/// Every variant of `RefusalReason`, in the order the wire lists them, built by a match
/// with an arm per variant and no wildcard: a variant the wire gains does not compile
/// here until it is chained in, and is then missing from `REFUSAL_REASONS` until it is
/// added there.
fn every_refusal_reason() -> Vec<RefusalReason> {
    let after = |reason: Option<RefusalReason>| match reason {
        None => Some(RefusalReason::NotQuiet),
        Some(RefusalReason::NotQuiet) => Some(RefusalReason::NoiseOverBound),
        Some(RefusalReason::NoiseOverBound) => Some(RefusalReason::SetupFailed),
        Some(RefusalReason::SetupFailed) => Some(RefusalReason::HostUnsafe),
        Some(RefusalReason::HostUnsafe) => Some(RefusalReason::Unsupported),
        Some(RefusalReason::Unsupported) => None,
    };
    std::iter::successors(after(None), |reason| after(Some(*reason))).collect()
}

/// `REFUSAL_REASONS`, which the refusal scenario runs over, holds every variant of
/// `RefusalReason`, each once, with its own wire code. Catches a reason the wire gains that
/// the scenario does not cover, and a list that holds one reason twice.
#[test]
fn the_refusal_scenario_covers_every_reason_the_wire_has() {
    assert_eq!(REFUSAL_REASONS.to_vec(), every_refusal_reason());
    let codes: std::collections::BTreeSet<String> = REFUSAL_REASONS
        .iter()
        .map(|reason| String::from(*reason))
        .collect();
    assert_eq!(codes.len(), REFUSAL_REASONS.len());
}

/// A host whose ready report comes within seconds of the prepare deadline's sweep, on
/// either side of it, never double-books: whichever comes first, both jobs succeed, and no
/// job goes back to the host that failed within the bar (design section 8, row 26).
/// Catches a late report that starts a job already placed elsewhere, a job lost in the
/// race, and a host booked to two jobs.
#[test]
fn a_ready_report_racing_the_prepare_deadline_never_double_books() {
    for scenario in whole_host_prepare::race_sweep() {
        scenario.run_seeds(RACE_SEEDS);
    }
}

/// The sweep of the race straddles it: the shortest preparation of the sweep is ready
/// before the deadline (no job placed again, in any seed) and the longest after it (a job
/// placed again). Catches a sweep that has moved to one side of the deadline, which would
/// run the race one way only.
#[test]
fn the_race_sweep_runs_the_race_both_ways() {
    let again = |step: i64, wanted: usize| {
        whole_host_prepare::a_race(step)
            .expect(Expect::jobs_placed_again(wanted))
            .try_run_seeds(SEEDS)
    };
    let (first, last) = whole_host_prepare::RACE_SWEEP.into_inner();
    assert!(again(first, 0).is_ok());
    match again(last, 0) {
        Err(ScenarioFailure::Unmet { detail, .. }) => {
            assert_eq!(detail, "jobs-placed-again: wanted 0, got 1");
        }
        other => panic!("not an unmet expectation: {other:?}"),
    }
}

/// An agent started again with empty state while its host is committed to a job, three
/// times, saying hello each time, does not double-book the host: the job on it is told of
/// again and starts when the host is ready, the others are untouched, and all succeed
/// (design section 8, row 26). Catches a hello that fails or loses the job, and a host
/// booked to a second job while the first prepares.
#[test]
fn an_agent_that_restarts_while_the_host_is_committed_does_not_double_book() {
    whole_host_prepare::restarts_while_committed().run_seeds(SEEDS);
}

/// An agent's hello restarts the prepare clock of the job placed on its host: an agent
/// started again at 300 s, 300 s before the deadline, whose preparation then takes 400 s,
/// reports ready after the placement's deadline and before the new one, and the job
/// succeeds on its one allocation. Catches a hello that leaves the deadline where the
/// placement set it (the job would fail its prepare and be placed a second time).
#[test]
fn an_agent_restart_moves_the_prepare_deadline_of_its_job() {
    whole_host_prepare::restart_moves_the_deadline().run_seeds(SEEDS);
}

/// A whole-host job cancelled while its host prepares is cancelled before the agent would
/// have reported ready, its allocation freed, with no container launched and the host
/// serving again (`job_whole.rs`, rule 1; design section 8, row 39, preparing). Catches a
/// cancel that lets the job start, one held until the host is ready, and an allocation left
/// booked.
#[test]
fn a_job_cancelled_while_its_host_prepares_is_cancelled_and_starts_nothing() {
    whole_host_prepare::a_cancel_while_the_host_prepares().run_seeds(SEEDS);
}

/// A whole-host job cancelled while its host is silent is cancelled a hundred seconds
/// before the deadline would have failed its prepare, not at the deadline, and is not tried
/// a second time. Catches a cancel held until the deadline, and a cancelled job put back in
/// the queue.
#[test]
fn a_job_cancelled_while_its_host_is_silent_is_cancelled_before_the_deadline() {
    whole_host_prepare::a_cancel_while_the_host_is_silent().run_seeds(SEEDS);
}
