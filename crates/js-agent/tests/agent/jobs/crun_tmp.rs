//! A crun agent's jobs' temporary directories, through the whole agent (the server, the
//! lifecycle and the crun runtime over the node's stand-in `crun`), with the machine's
//! in-memory quota: without `--scratch-quota`, each job's `/tmp` and `/var/tmp` is a
//! directory beside the run's in the scratch root, bounded in a project of its own and gone
//! with the run; and an agent whose scratch root cannot enforce a project quota starts and
//! runs no job. What the kernel refuses is `tests/crun_runtime`'s.

use std::fs;
use std::path::PathBuf;

use js_agent::lifecycle::TMP_INODES;
use js_agent::quota::{Call, FakeQuota, ProjectId, ProjectIds};

use super::crun_e2e::{NODE_IN_USE, Rig, crun_flags, node};
use super::crun_machine::CrunMachine;
use crate::server::POOL_RUNS_QUICK_TOML;
use crate::support::code;

/// The bounds the machine's quota was given, as `(directory, project, bytes, inodes)`.
fn bounds(rig: &Rig) -> Vec<(PathBuf, ProjectId, u64, u64)> {
    let calls = rig.machine.fake.quota.calls();
    calls
        .into_iter()
        .filter_map(|call| match call {
            Call::Apply {
                dir,
                project,
                block_limit_bytes,
                inode_limit,
            } => Some((dir, project, block_limit_bytes, inode_limit)),
            _ => None,
        })
        .collect()
}

/// Asserts the whole path of a job's temporary directory on an agent without
/// `--scratch-quota`: the run's job gets `.tmp-<lease>` in the scratch root, bound in the
/// slot's temporary project to 1 GiB and the temporary inode limit before its container
/// runs, with its `tmp` and `var-tmp` in it; nothing else is bounded (the run's scratch has
/// no quota without the flag); and once the job has ended and been cleaned up the directory
/// is gone. It catches a crun agent that leaves `/tmp` unbounded (or in memory) when the
/// operator did not ask for `--scratch-quota`, a default other than 1 GiB, and a `/tmp` left
/// on the node's disk after its job.
#[test]
fn a_crun_agent_bounds_each_jobs_tmp_directory_in_a_project_of_its_own() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let rig = Rig::new("jobs_crun_tmp", &[]);
    let run = rig.queued();
    rig.until_up(&run);
    let bound = bounds(&rig);
    let [(dir, project, bytes, inodes)] = bound.as_slice() else {
        panic!("not one bound, the job's temporary directory's: {bound:?}");
    };
    let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(leaf.starts_with(".tmp-") && leaf.ends_with(&run), "{leaf}");
    assert_eq!(
        (*project, *bytes, *inodes),
        (
            ProjectIds::default().of_tmp_slot(0).unwrap(),
            1 << 30,
            TMP_INODES
        )
    );
    for inside in ["tmp", "var-tmp"] {
        assert!(dir.join(inside).is_dir(), "{inside} is made in it");
    }
    fs::write(node().dir.path().join("go"), "go").expect("let the container end");
    rig.until(&run, |seen| seen.1.is_some());
    for _ in 0..100 {
        if !dir.exists() {
            break;
        }
        rig.beat();
    }
    assert!(!dir.exists(), "the temporary directory outlived its job");
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
}

/// Asserts what a crun agent does on a scratch root that cannot enforce a project quota (an
/// XFS mounted without `prjquota`): it starts and registers (a node is not lost for it), says
/// at start, with the probe's reason, that it starts no job until the quota works, and runs no
/// job: the stand-in `crun` is never asked to run one and no directory is bounded. It catches
/// an agent that runs a job with an unbounded `/tmp` on such a host, and one that exits and
/// leaves the node out of the pool.
#[test]
fn a_crun_agent_whose_scratch_root_cannot_hold_a_quota_runs_no_job() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let reason = "xfs mounted without prjquota";
    let machine = CrunMachine::with_quota(FakeQuota::unsupported(reason));
    let flags = crun_flags("h1", &[]);
    let rig = Rig::on_flags("jobs_crun_tmp_none", flags, POOL_RUNS_QUICK_TOML, machine);
    let run = rig.queued();
    for _ in 0..10 {
        rig.beat();
    }
    let (state, _) = rig.view(&run);
    assert_ne!(state, "JOB_STATE_RUNNING", "the job ran");
    let ready = node().dir.path().join("ready");
    assert!(!ready.exists(), "the stand-in ran a container");
    assert!(bounds(&rig).is_empty(), "something was bounded");
    let ran = rig.agent.stop();
    assert_eq!(ran.status(), code(0), "{}", ran.err);
    let warning = format!(
        "js-agent: warning: the scratch root cannot enforce a project quota, which bounds each \
         job's /tmp and /var/tmp ({reason}): no job starts here until it can"
    );
    assert!(ran.err.lines().any(|line| line == warning), "{}", ran.err);
}
