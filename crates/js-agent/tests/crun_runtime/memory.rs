//! The memory booking of a job is no limit: over a real `crun`, the cgroups the agent makes for a
//! job carry no `memory.max` and no `memory.swap.max`, a job that uses more than it booked on a
//! host with room runs to its end and reports what it used, and a job that grows without bound
//! is stopped only by the limit above all jobs, which kills it whole and leaves its neighbour
//! running.
//!
//! The runner is not a node, so each test holds its jobs together to a limit of its own
//! ([`CrunSubject::hold_jobs_to`]): it stands for the host's memory (with the kernel's own
//! out-of-memory handling) or for one limit on the group of all jobs. Without it a job that
//! grows without bound would take the runner. Nothing is timed: a job that must be up opens a
//! pipe the test reads, and one that must have ended is waited for by the runtime.

use std::fs;
use std::path::Path;

use js_agent::lifecycle::{Body, Exit, RunHandle, Status};

use crate::runtime_suite::{Program, Subject, begin, clean_up, poll, spec_of};
use crate::subject::{CrunSubject, crun_subject};

/// What the jobs here book: far less than they use.
const BOOKED: u64 = 64 << 20;
/// What the job over its booking holds at once: four times its booking.
const HELD: u64 = 256 << 20;
/// The memory of the jobs together, when there is room: four times what [`HELD`] needs.
const ROOM: u64 = 1 << 30;
/// The memory of the jobs together, for the job that grows without bound: the neighbour's
/// booking (256 MiB, of which it uses a few) and the runaway's (64 MiB) fit in it, as a
/// placement packs a host, and the runaway goes past them.
const HOST: u64 = 384 << 20;

/// Starts the run `run_id` of `/bin/sh -c script`, booked [`BOOKED`].
fn start(s: &mut CrunSubject, run_id: &str, script: &str) -> RunHandle {
    s.prepared();
    let mut spec = spec_of(s, run_id, Program::Exit(0));
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
    };
    spec.memory_bytes = BOOKED;
    let started = s.runtime().borrow_mut().create_and_start(&spec);
    started.expect("create and start")
}

/// The trimmed text of the cgroup file `file` in `dir`.
fn read(dir: &Path, file: &str) -> String {
    let text = fs::read_to_string(dir.join(file));
    let text = text.unwrap_or_else(|e| panic!("read {}: {e}", dir.join(file).display()));
    text.trim().to_owned()
}

/// Proves what the agent writes on a job's cgroups, read from the kernel's own files while the
/// job runs: the lease's and the container's `memory.max` and `memory.swap.max` read `max`
/// (none was written), and both keep `memory.oom.group` 1, so a job the kernel kills dies
/// whole. Catches a `memory.max` of the booking on either cgroup (a job that needs more than it
/// booked would stall in reclaim at it, and a tool that sizes itself from it would size itself
/// to the booking), and a `memory.swap.max` of 0 (the host's swap could not absorb what a job
/// uses over its booking).
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_jobs_cgroups_carry_no_memory_or_swap_limit_from_its_booking() {
    let mut s = crun_subject("no_memory_limit");
    let id = begin(&mut s, "r1", Program::Hold);
    let ctr = s.runtime().borrow_mut().cgroup(&id).expect("a cgroup");
    let lease = ctr.parent().expect("the lease's cgroup").to_owned();
    for dir in [&lease, &ctr] {
        let files = ["memory.max", "memory.swap.max", "memory.oom.group"].map(|f| read(dir, f));
        assert_eq!(files, ["max", "max", "1"], "{}", dir.display());
    }
    clean_up(&s);
}

/// Proves a job that uses four times its booking on a host with room runs to its end: it
/// exits 0, unflagged, and its usage reports a peak of at least what it held (256 MiB, booked
/// 64 MiB), never one clipped at the booking. Catches a job killed or stalled at its booking,
/// and a peak held at it, which would teach the sizing a job smaller than it is.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_job_over_its_booking_on_a_host_with_room_runs_to_its_end_and_reports_its_peak() {
    let mut s = crun_subject("over_its_booking");
    s.hold_jobs_to(ROOM, 0);
    // One line of `HELD` zeros, which `tail` holds in memory until its input ends.
    let id = start(
        &mut s,
        "r1",
        &format!("head -c {HELD} /dev/zero | tail > /dev/null"),
    );
    s.settled(&id);
    let ended = Exit {
        code: 0,
        oom_killed: false,
    };
    assert_eq!(poll(&s, &id), Status::Exited(ended));
    let usage = s.runtime().borrow_mut().usage(&id).expect("its usage");
    assert!(
        usage.peak_memory_bytes >= HELD,
        "the peak is what the job held, not its booking: {usage:?}"
    );
    clean_up(&s);
}

/// A job that grows without bound beside a quiet neighbour, under a limit of [`HOST`] on the
/// jobs together with `swap` bytes of swap: it is killed whole by that limit, exits 137 with
/// the memory flag clear (the kill is the host's, not its booking's, so the server runs it
/// again with the same booking), its peak is past its booking, and the neighbour still runs.
fn a_runaway_is_killed_whole_and_its_neighbour_runs_on(name: &str, swap: u64) {
    let mut s = crun_subject(name);
    s.hold_jobs_to(HOST, swap);
    let neighbour = begin(&mut s, "n1", Program::Hold);
    // One line that never ends, held in memory by `tail`.
    let runaway = start(&mut s, "r1", "exec tail /dev/zero");
    s.settled(&runaway);
    let killed = Exit {
        code: 137,
        oom_killed: false,
    };
    assert_eq!(poll(&s, &runaway), Status::Exited(killed));
    let usage = s.runtime().borrow_mut().usage(&runaway).expect("its usage");
    assert!(
        usage.peak_memory_bytes > 4 * BOOKED,
        "it grew past its booking until the limit above it: {usage:?}"
    );
    assert_eq!(
        poll(&s, &neighbour),
        Status::Running,
        "the neighbour runs on"
    );
    clean_up(&s);
}

/// Proves what a job that grows without bound does to a host, with no swap: the kernel kills
/// it, whole, at the limit above all jobs, and the job beside it runs on. Catches a job killed
/// at its own booking (flagged, so the server would book it twice as much and run it again), a
/// kill that takes the neighbour, and one that leaves part of the runaway running.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_job_that_grows_without_bound_is_killed_whole_and_its_neighbour_runs_on() {
    a_runaway_is_killed_whole_and_its_neighbour_runs_on("runaway_no_swap", 0);
}

/// Proves the same with 64 MiB of swap for the jobs together: swap absorbs what it can first,
/// then the kernel kills the runaway whole, and the neighbour runs on. On a runner without a
/// swap device the swap limit allows nothing and the end is the same, which is the point: the
/// outcome does not depend on whether the host swaps.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_job_that_grows_without_bound_with_swap_is_killed_whole_and_its_neighbour_runs_on() {
    a_runaway_is_killed_whole_and_its_neighbour_runs_on("runaway_swap", 64 << 20);
}
