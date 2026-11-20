//! The post-job integrity checks: each check from a clean baseline with one change made in
//! the in-memory host, the inputs that cannot be read, the bounds, and the real Linux
//! reader over recorded `/proc` text. A change that is the host's own is a note and the
//! check passes; only a job's process left running is a finding. Nothing here reads the
//! machine's own `/proc`.

mod checks;
mod crun;
mod lifecycle;
mod mapped;
mod modules;
mod noise;
mod overlap;
mod proc;
mod restored;
mod rules;
mod settle;

use std::path::PathBuf;

use js_agent::cli_runtime::JobUsers;
use js_agent::integrity::{FakeHost, Integrity, Mount, Process};
use js_agent::lifecycle::{CheckFinding, CheckHook, CheckNote, Finished, RunHandle};

/// The user the containers run as in these tests.
pub const JOB_UID: u32 = 65534;

/// The users of the containers in these tests: the one host uid, no mapping.
pub const JOB_USERS: JobUsers = JobUsers::Uid(JOB_UID);

/// The checks over `host`, with `watch` watched and the baseline taken now.
pub fn started(host: &FakeHost, watch: Vec<PathBuf>) -> Integrity {
    Integrity::start(Box::new(host.clone()), JOB_USERS, watch).unwrap()
}

/// The check after the run `run_id`, with no other container on the host.
pub fn after(checks: &mut Integrity, run_id: &str) -> Result<(), CheckFinding> {
    checks.run(&Finished {
        run_id,
        live: &[],
        slot: None,
        live_slots: &[],
        held_mounts: &[],
    })
}

/// The check after the run `run_id` that held `slot`, with the jobs of `live_slots` still
/// running (their containers are not named, so only their slots tell them from strays).
pub fn after_slot(
    checks: &mut Integrity,
    run_id: &str,
    slot: u32,
    live_slots: &[u32],
) -> Result<(), CheckFinding> {
    checks.run(&Finished {
        run_id,
        live: &[],
        slot: Some(slot),
        live_slots,
        held_mounts: &[],
    })
}

/// The check after `run_id`, with the containers `live` still on the host.
pub fn beside(checks: &mut Integrity, run_id: &str, live: &[&str]) -> Result<(), CheckFinding> {
    let live: Vec<RunHandle> = live.iter().map(|id| RunHandle((*id).to_owned())).collect();
    checks.run(&Finished {
        run_id,
        live: &live,
        slot: None,
        live_slots: &[],
        held_mounts: &[],
    })
}

/// A process.
pub fn process(pid: u32, ppid: u32, uid: u32, start: u64, cgroup: &str) -> Process {
    Process {
        pid,
        ppid,
        uid,
        start,
        cgroup: cgroup.to_owned(),
        comm: "stray".to_owned(),
        state: 'S',
    }
}

/// A process with the command name `comm` in the state `state`.
pub fn process_named(base: Process, comm: &str, state: char) -> Process {
    Process {
        comm: comm.to_owned(),
        state,
        ..base
    }
}

/// A mount of a file system of type `ext4` from `source`.
pub fn mount(point: &str, source: &str) -> Mount {
    Mount {
        root: "/".to_owned(),
        point: point.to_owned(),
        fstype: "ext4".to_owned(),
        source: source.to_owned(),
    }
}

/// What a finding says: (is it a residue, code, detail).
pub fn said(found: &CheckFinding) -> (bool, &'static str, String) {
    (
        matches!(found, CheckFinding::Residue { .. }),
        found.code(),
        found.detail().to_owned(),
    )
}

/// Asserts that the check after the run `run_id` passes (only a leftover process of a job
/// taints), and returns the notes it made.
pub fn noted_after(checks: &mut Integrity, run_id: &str) -> Vec<CheckNote> {
    assert_eq!(after(checks, run_id), Ok(()), "the check passes");
    checks.notes()
}

/// Asserts that the check after `run_id`, with the containers `live` still on the host,
/// passes, and returns the notes it made.
pub fn noted_beside(checks: &mut Integrity, run_id: &str, live: &[&str]) -> Vec<CheckNote> {
    assert_eq!(beside(checks, run_id, live), Ok(()), "the check passes");
    checks.notes()
}

/// Whether the check after `run_id` passes and notes nothing: the host's change, if any, is
/// not one the checks see.
pub fn quiet(checks: &mut Integrity, run_id: &str) -> bool {
    after(checks, run_id).is_ok() && checks.notes().is_empty()
}

/// Whether the check after `run_id`, with the containers `live` still on the host, passes
/// and notes nothing.
pub fn quiet_beside(checks: &mut Integrity, run_id: &str, live: &[&str]) -> bool {
    beside(checks, run_id, live).is_ok() && checks.notes().is_empty()
}

/// The codes of `notes`, in order.
pub fn codes(notes: &[CheckNote]) -> Vec<&'static str> {
    notes.iter().map(|note| note.code).collect()
}
