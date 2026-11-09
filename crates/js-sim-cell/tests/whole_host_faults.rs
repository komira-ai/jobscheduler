//! The whole-host fault family: the ways the whole-host path of jobs meets work in the way
//! and failure, over the server's whole path (its job service, the queue, the placement and
//! the marks of a host being drained, and agents that run the jobs): a drain behind busy
//! hosts, a cancelled job, and a placed job whose agent restarts. Each runs over [`SEEDS`]
//! seeds under every invariant of `Invariant::ALL`.
//!
//! Each states only what the server does today and the design also promises: none pins
//! the order the queue serves (design section 8, row 11) or anything section 8 marks Open.
//! What is a whole host comes from what the client asked for, never from the server's
//! listing.
//!
//! The scenarios and their sizes are in `cell/whole_host_faults.rs` and, for the cancelled
//! job, which the replay tests also pin, `cell/whole_host_cancel.rs`.

#[path = "cell/whole_host_cancel.rs"]
mod whole_host_cancel;
#[path = "cell/whole_host_faults.rs"]
mod whole_host_faults;

/// Every scenario of the family runs over this many seeds.
const SEEDS: u64 = 16;

/// Every host busy with a 300 s run when a whole-host job arrives: it waits, is placed only
/// after the runs on its host ended, and no run is cut (design section 6.4, "drain never
/// cuts work"; no row of section 7 or 8 names it).
#[test]
fn drain_waits_for_a_busy_host_and_cuts_no_run() {
    whole_host_faults::a_drain().run_seeds(SEEDS);
}

/// A whole-host job cancelled while queued loses its place at once, and the host serves
/// again (design section 8, row 39, queued).
#[test]
fn a_cancelled_whole_host_request_drops_its_place_and_the_host_serves_again() {
    whole_host_cancel::a_cancelled_request().run_seeds(SEEDS);
}

/// A placed whole-host job whose host's agent is started again: the host is not booked to
/// a second job (design section 8, row 25).
#[test]
fn a_whole_host_job_whose_agent_restarts_after_it_is_placed() {
    whole_host_faults::held_job_restart().run_seeds(SEEDS);
}
