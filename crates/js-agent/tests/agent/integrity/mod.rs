//! The post-job integrity checks: each check from a clean baseline with one change made in
//! the in-memory host, the inputs that cannot be read, the bounds, and the real Linux
//! reader over recorded `/proc` text. Nothing here reads the machine's own `/proc`.

mod checks;
mod lifecycle;
mod mapped;
mod noise;
mod proc;
mod rules;

use std::path::PathBuf;

use js_agent::cli_runtime::JobUsers;
use js_agent::integrity::{FakeHost, Integrity, Mount, Process};
use js_agent::lifecycle::{CheckFinding, CheckHook, ContainerId, Finished};

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
    checks.run(&Finished { run_id, live: &[] })
}

/// The check after `run_id`, with the containers `live` still on the host.
pub fn beside(checks: &mut Integrity, run_id: &str, live: &[&str]) -> Result<(), CheckFinding> {
    let live: Vec<ContainerId> = live
        .iter()
        .map(|id| ContainerId((*id).to_owned()))
        .collect();
    checks.run(&Finished {
        run_id,
        live: &live,
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
