//! The whole-host family: the server's whole-host path for jobs (a job that asks for a
//! whole host, its wait in the queue, its placement on a quiet host, the start, the run and
//! the host's return to service) over many hosts, under every invariant of
//! `Invariant::ALL`.
//!
//! The scenario states only what the server does today and the design also promises: it
//! pins neither the order in which the queue serves (design section 8, row 11) nor
//! anything section 8 marks Open. It is in `cell/whole_host.rs`, shared with the replay
//! tests that pin its trace hashes.
//!
//! Sizes, in agent requests a seed: the fifty jobs, about 4,800 heartbeats
//! (`cell/whole_host.rs`), over [`SEEDS`] seeds.

#[path = "cell/whole_host.rs"]
mod whole_host;

/// How many seeds the scenario runs.
const SEEDS: u64 = 16;

/// Proves the whole-host path over twenty hosts: fifty whole-host jobs are each placed once
/// and released, and every host is serving at the end (design section 5.4 as built, through
/// the queue of section 8, rows 11 and 12, pinning neither; the scenario's own doc says
/// what turns it red).
#[test]
fn fifty_whole_host_requests_over_twenty_hosts_are_each_held_once_and_released() {
    whole_host::fifty_whole_host_requests().run_seeds(SEEDS);
}
