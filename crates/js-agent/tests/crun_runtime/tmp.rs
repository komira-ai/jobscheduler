//! A job's temporary directory over a real `crun` and a real XFS project quota: the job's
//! `/tmp` and `/var/tmp` are on the scratch disk (an XFS, not a tmpfs in memory), share one
//! bound of 1 GiB in a project of their own, and the kernel refuses a write past it with "no
//! space left" while nothing the job wrote escapes the project; the directory goes with the
//! container.
//!
//! The volume is the loop-mounted XFS with `prjquota` of the quota tests (`JS_XFS_ROOT`, made
//! by the workflow, large enough for a job to reach the bound). The tests do what the
//! lifecycle does around a run, in its order: the run's directory, the lease's and the job's
//! temporary directory ([`Runtime::tmp_dir`]) are made empty beside each other, the temporary
//! one is put under the slot's temporary project, and only then is the container created.
//! Each test takes a slot of its own.

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::time::Duration;

use js_agent::lifecycle::{Body, Status, TMP_INODES};
use js_agent::quota::{Mode, ProjectId, ProjectIds, ScratchQuota, XfsQuota};

use crate::escape::{self, Disk};
use crate::quota::volume;
use crate::runtime_suite::{Program, Shared, Subject, spec_of};
use crate::subject::{CrunSubject, crun_subject_on};

/// The bound of a job's temporary directory: the agent's default, 1 GiB.
const TMP_LIMIT: u64 = 1 << 30;

/// The id of the run every test starts.
const RUN: &str = "t1";

/// What is cleaned up whatever a test did: the containers are stopped and removed, the
/// directories removed, and the bound taken off the project. Declared after the subject, so
/// it drops first.
struct Bound {
    quota: XfsQuota,
    runtime: Shared,
    dirs: Vec<PathBuf>,
    tmp_dir: PathBuf,
    project: ProjectId,
}

impl Drop for Bound {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.runtime.try_borrow_mut()
            && let Ok(found) = runtime.list_labelled()
        {
            for container in found {
                let _ = runtime.stop(&container.id, Duration::ZERO);
                let _ = runtime.remove(&container.id);
            }
        }
        for dir in &self.dirs {
            let _ = fs::remove_dir_all(dir);
        }
        let _ = self.quota.clear(&self.tmp_dir, self.project);
    }
}

/// What a job's script left.
struct Ran {
    exit: i32,
    stdout: String,
    stderr: String,
    /// What the temporary project holds while the container is held.
    used: u64,
    /// The bytes of the temporary directory that are not in its project.
    escaped: u64,
    /// Whether the temporary directory was there after the container was removed.
    left_after_removal: bool,
}

/// Runs `script` as a job of the slot `slot`, its temporary directory bounded to
/// [`TMP_LIMIT`] in the slot's temporary project as the lifecycle bounds it, reads what it
/// left while the container is held, then removes the container.
fn run_script(script: &str, slot: u32) -> Ran {
    let volume = volume();
    let mut subject: CrunSubject = crun_subject_on(&format!("tmp_{slot}"), &volume);
    subject.prepared();
    let mut spec = spec_of(&mut subject, RUN, Program::Exit(0));
    let Body::Container { args, .. } = &mut spec.body else {
        panic!("spec_of makes a container job");
    };
    *args = vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];

    let quota = XfsQuota::new(subject.runs(), ProjectIds::default());
    let probe = quota.probe();
    assert_eq!(probe.mode, Mode::Project, "{}", probe.reason);
    let project = ProjectIds::default()
        .of_tmp_slot(slot)
        .expect("a temporary project");
    let (lease_dir, tmp_dir) = {
        let runtime = subject.runtime();
        let runtime = runtime.borrow();
        (
            runtime.lease_dir(RUN).expect("a lease beside the runs"),
            runtime
                .tmp_dir(RUN)
                .expect("a temporary directory beside the runs"),
        )
    };
    let bound = Bound {
        quota,
        runtime: subject.runtime(),
        dirs: vec![spec.scratch.clone(), lease_dir.clone(), tmp_dir.clone()],
        tmp_dir: tmp_dir.clone(),
        project,
    };
    // The lifecycle's order: the directories empty, the temporary one bound, then the
    // container.
    fs::create_dir(&lease_dir).expect("make the lease directory");
    fs::create_dir(&tmp_dir).expect("make the job's temporary directory");
    bound
        .quota
        .apply(&tmp_dir, project, TMP_LIMIT, TMP_INODES)
        .unwrap_or_else(|e| panic!("bound {}: {e}", tmp_dir.display()));

    let id = subject
        .runtime()
        .borrow_mut()
        .create_and_start(&spec)
        .expect("create and start");
    subject.settled(&id);
    let status = subject.runtime().borrow_mut().poll(&id).expect("poll");
    let Status::Exited(exit) = status else {
        panic!("the job did not end: {status:?}");
    };
    let tail = subject
        .runtime()
        .borrow_mut()
        .log_tail(&id)
        .expect("log tail")
        .expect("the container is held");
    let used = bound.quota.usage(project).expect("usage").bytes;
    let dev = fs::metadata(&volume).expect("stat the volume").dev();
    let escaped = escape::escaped(&Disk, dev, &[&tmp_dir], project.get())
        .unwrap_or_else(|e| panic!("walk the temporary directory: {e}"));
    subject.runtime().borrow_mut().remove(&id).expect("remove");
    let ran = Ran {
        exit: exit.code,
        stdout: tail.stdout,
        stderr: tail.stderr,
        used,
        escaped,
        left_after_removal: tmp_dir.exists(),
    };
    drop(bound);
    ran
}

/// The lines of the job's output.
fn lines(ran: &Ran) -> Vec<&str> {
    ran.stdout.lines().collect()
}

/// Asserts that a job's `/tmp` and `/var/tmp` hold what fits in 1 GiB and nothing past it:
/// 900 MiB written to `/tmp` is written, then 227 MiB more to `/var/tmp` (1.1 GiB in all) is
/// refused as a full disk, what the project holds stays at most 1 GiB, nothing escaped it,
/// both places are an XFS (`statfs`), not a tmpfs, and the directory is gone once the
/// container is removed. It catches a `/tmp` left in memory, a `/var/tmp` outside the bound
/// (or with a bound of its own), a bound below 900 MiB or above 1 GiB, and a `/tmp` left on
/// the node's disk after its job.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writes_900_mib_to_tmp_and_is_refused_past_1_gib_with_var_tmp() {
    let ran = run_script(
        "dd if=/dev/zero of=/tmp/a bs=1048576 count=900 && echo first=ok; \
         dd if=/dev/zero of=/var/tmp/b bs=1048576 count=227; echo second=$?; \
         stat -f -c %T /tmp; stat -f -c %T /var/tmp; exit 0",
        60,
    );
    let out = lines(&ran);
    assert_eq!(ran.exit, 0, "{}", ran.stderr);
    assert!(
        out.contains(&"first=ok"),
        "900 MiB fit: {out:?} {}",
        ran.stderr
    );
    assert!(
        out.iter()
            .any(|line| line.starts_with("second=") && *line != "second=0"),
        "the write past 1 GiB was not refused: {out:?}"
    );
    assert!(
        ran.stderr.contains("No space left on device"),
        "not refused as a full disk: {}",
        ran.stderr
    );
    assert_eq!(
        out.iter().filter(|line| **line == "xfs").count(),
        2,
        "/tmp and /var/tmp are not both on the XFS: {out:?}"
    );
    assert!(!out.contains(&"tmpfs"), "{out:?}");
    assert!(
        ran.used > 900 << 20,
        "the bytes were not counted: {}",
        ran.used
    );
    assert!(ran.used <= TMP_LIMIT, "the bound was passed: {}", ran.used);
    assert_eq!(ran.escaped, 0, "bytes escaped the temporary project");
    assert!(
        !ran.left_after_removal,
        "the temporary directory outlived its job"
    );
}

/// Asserts that one write of 1.1 GiB to `/tmp` is refused as a full disk, the project holds at
/// most the bound and nothing escaped it: the node's disk is not filled past the job's 1 GiB.
/// It catches a `/tmp` with no bound, which fills the scratch volume.
#[test]
#[ignore = "real crun on an XFS volume: the js-agent-crun-runtime workflow"]
fn a_job_writing_1_1_gib_to_tmp_is_refused() {
    let ran = run_script(
        "dd if=/dev/zero of=/tmp/big bs=1048576 count=1127 && exit 0; exit 7",
        61,
    );
    assert_eq!(ran.exit, 7, "the write was not refused: {}", ran.stderr);
    assert!(
        ran.stderr.contains("No space left on device"),
        "not refused as a full disk: {}",
        ran.stderr
    );
    assert!(
        ran.used > TMP_LIMIT / 2,
        "the bytes were not counted: {}",
        ran.used
    );
    assert!(ran.used <= TMP_LIMIT, "the bound was passed: {}", ran.used);
    assert_eq!(ran.escaped, 0, "bytes escaped the temporary project");
}
