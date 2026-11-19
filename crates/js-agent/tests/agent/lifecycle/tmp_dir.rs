//! A job's temporary directory (the crun runtime's `/tmp` and `/var/tmp`), over the fake
//! runtime and the in-memory quota: the lifecycle makes it empty beside the run's directory,
//! puts it under the slot's temporary project (a bound of its own, apart from the run's
//! scratch), removes it with the run, clears both projects, keeps a slot whose temporary
//! project still holds usage, and starts no run while the quota cannot be used. What the
//! kernel then refuses is `tests/crun_runtime`'s.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use js_agent::lifecycle::{Event, RunQuota, TMP_INODES};
use js_agent::quota::{Call, ProjectId, ProjectIds, Usage};
use js_agent_wire::Outcome;

use super::quota::{DEFAULT_BYTES, INODES, Spy, applies, clears, project};
use super::{Harness, Options, cleanup, job, names};

/// The bytes of a job's temporary directory in these tests: 1 GiB.
const TMP_BYTES: u64 = 1 << 30;

/// The temporary project of `slot`.
fn tmp_project(slot: u32) -> ProjectId {
    ProjectIds::default().of_tmp_slot(slot).unwrap()
}

/// A crun agent without `--scratch-quota`: only the jobs' temporary directories are bounded.
fn tmp_only(spy: Arc<Spy>) -> RunQuota {
    RunQuota::tmp_only(spy).with_tmp_bytes(TMP_BYTES)
}

/// A crun agent with `--scratch-quota`: the runs' scratch and the jobs' temporary
/// directories are bounded, each in its own project.
fn both(spy: Arc<Spy>) -> RunQuota {
    RunQuota::new(spy, DEFAULT_BYTES, INODES).with_tmp_bytes(TMP_BYTES)
}

/// A crun agent whose scratch root could not enforce a project quota when it started.
fn unusable(spy: Arc<Spy>) -> RunQuota {
    tmp_only(spy).unusable("xfs mounted without prjquota")
}

/// A harness whose quota is `build` over a spy, with the fake runtime giving each job a
/// temporary directory in the scratch root.
fn harness(name: &str, build: fn(Arc<Spy>) -> RunQuota) -> (Harness, Arc<Spy>) {
    let spy = Arc::new(Spy::new());
    let options = Options {
        quota: Some(spy.clone()),
        quota_build: Some(build),
        ..Options::default()
    };
    let h = Harness::with(name, options);
    let root = h.scratch_of("any").parent().unwrap().to_owned();
    h.fake.keep_tmp_dirs_in(&root);
    (h, spy)
}

/// The job's temporary directory of `run`.
fn tmp_of(h: &Harness, run: &str) -> PathBuf {
    h.scratch_of(run).with_file_name(format!(".tmp-fake-{run}"))
}

/// The applies of `spy` as `(directory name, project, bytes, inodes)`.
fn bounds(spy: &Spy) -> Vec<(String, ProjectId, u64, u64)> {
    applies(spy)
        .into_iter()
        .map(|call| match call {
            Call::Apply {
                dir,
                project,
                block_limit_bytes,
                inode_limit,
            } => (
                dir.file_name().unwrap().to_string_lossy().into_owned(),
                project,
                block_limit_bytes,
                inode_limit,
            ),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Asserts that a job's temporary directory is made empty in the scratch root and bound in
/// the slot's temporary project, to the temporary bound and inode limit, before the container
/// is created; and that without `--scratch-quota` the run's own directory is bound by nothing.
/// It catches a `/tmp` with no bound (a job could fill the node's disk), one bound after the
/// container is made (files written first escape the count), and one put in the run's scratch
/// project (it would share, and be capped by, the run's booking).
#[test]
fn a_jobs_tmp_directory_is_bound_in_its_own_project_before_the_container_is_made() {
    let (mut h, spy) = harness("tmp-bind", tmp_only);
    assert_eq!(names(&h.start(job("r1"))), ["started r1"]);
    assert_eq!(spy.seen(), ["apply .tmp-fake-r1 empty=true ok=true"]);
    assert_eq!(
        bounds(&spy),
        [(
            ".tmp-fake-r1".to_owned(),
            tmp_project(0),
            TMP_BYTES,
            TMP_INODES
        )]
    );
    assert!(tmp_of(&h, "r1").is_dir());
    assert!(
        h.log
            .borrow()
            .iter()
            .any(|line| line == "create r1 [no quota]"),
        "the run's own directory is bound by nothing: {:?}",
        h.log.borrow()
    );
}

/// Asserts that with `--scratch-quota` too, the run's scratch and the job's temporary
/// directory each get their own project and bound: the run's at the bytes it books, the
/// temporary one at the temporary bound. It catches the two bounds merged into one project.
#[test]
fn with_a_scratch_quota_the_tmp_directory_keeps_a_project_of_its_own() {
    let (mut h, spy) = harness("tmp-both", both);
    h.start(job("r1"));
    assert_eq!(
        bounds(&spy),
        [
            ("r1".to_owned(), project(0), DEFAULT_BYTES, INODES),
            (
                ".tmp-fake-r1".to_owned(),
                tmp_project(0),
                TMP_BYTES,
                TMP_INODES
            ),
        ]
    );
    assert_ne!(project(0), tmp_project(0));
}

/// Asserts that the temporary directory goes with the run: after the run ends its cleanup
/// removes it, then clears the temporary project and the scratch project, in that order,
/// each once and only once nothing of the run is left in the scratch root. It catches a
/// `/tmp` left behind on the node's disk after its job, and a project cleared while its
/// directory still holds files.
#[test]
fn the_tmp_directory_is_removed_with_the_run_and_both_projects_are_cleared() {
    let (mut h, spy) = harness("tmp-remove", tmp_only);
    h.start(job("r1"));
    fs::write(tmp_of(&h, "r1").join("left"), b"what the job wrote").unwrap();
    h.fake.exit("r1", 0);
    assert_eq!(
        names(&h.life.step()),
        [vec!["result r1".to_owned()], cleanup("r1")].concat()
    );
    assert!(!tmp_of(&h, "r1").exists(), "the directory is removed");
    let cleared: Vec<_> = clears(&spy)
        .into_iter()
        .map(|call| match call {
            Call::Clear { project, .. } => project,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(cleared, [tmp_project(0), project(0)]);
    assert_eq!(
        spy.seen()[1..],
        ["clear .tmp-fake-r1 listed=[]", "clear r1 listed=[]"]
    );
}

/// Asserts that a slot whose temporary project still holds usage (a finished job's files
/// the file system has not freed) is parked and the next slot taken, as a slot whose scratch
/// project holds usage is. It catches a run given a slot whose `/tmp` still counts an earlier
/// job's bytes against its bound.
#[test]
fn a_slot_whose_tmp_project_holds_usage_is_parked_and_the_next_one_taken() {
    let (mut h, spy) = harness("tmp-busy", tmp_only);
    spy.inner.charge(
        tmp_project(0),
        Usage {
            bytes: 4096,
            inodes: 1,
        },
    );
    assert_eq!(
        names(&h.start(job("r1"))),
        ["finding r1 quota-slot-busy", "started r1"]
    );
    assert_eq!(
        bounds(&spy),
        [(
            ".tmp-fake-r1".to_owned(),
            tmp_project(1),
            TMP_BYTES,
            TMP_INODES
        )]
    );
}

/// Asserts that a finished run whose temporary project alone still reads usage after its tree
/// is gone (its scratch project reads zero) holds its slot (the cleanup waits, saying why) until the usage reads zero. It catches a
/// cleanup that reads the scratch project alone and frees the slot while `/tmp`'s inodes are
/// still counted.
#[test]
fn a_tmp_project_whose_usage_lags_holds_the_slot_until_it_reads_zero() {
    let (mut h, spy) = harness("tmp-lag", tmp_only);
    spy.set_lag_of(tmp_project(0), 1);
    h.start(job("r1"));
    h.fake.exit("r1", 0);
    let first = names(&h.life.step());
    assert!(
        first.contains(&"finding r1 quota-usage-held".to_owned()),
        "{first:?}"
    );
    assert!(!first.contains(&"freed r1".to_owned()), "{first:?}");
    let second = names(&h.life.step());
    assert!(second.contains(&"freed r1".to_owned()), "{second:?}");
}

/// Asserts that a temporary directory that cannot be bound refuses the run and makes no
/// container: the start fails `scratch-failed`. It catches a job started with an unbounded
/// `/tmp`.
#[test]
fn a_tmp_directory_that_cannot_be_bound_refuses_the_run() {
    let (mut h, spy) = harness("tmp-apply-fails", tmp_only);
    spy.inner.set_fail_apply(true);
    assert_eq!(
        names(&h.start(job("r1"))),
        ["finding r1 scratch-failed", "result r1"]
    );
    assert!(h.fake.created().is_empty(), "no container");
    let result = &h.life.results()[0];
    assert_eq!(result.outcome, Outcome::FailedToStart);
    assert_eq!(result.detail.as_deref(), Some("scratch-failed"));
}

/// Asserts the start-up sweep: an adopted run's temporary directory stays, a leftover one of a
/// run that no longer exists is removed with what is in it, and the temporary project it held
/// is cleared. It catches a sweep that removes a live job's `/tmp`, and one that leaves a dead
/// job's `/tmp` and its project's bytes on the disk.
#[test]
fn the_sweep_keeps_an_adopted_runs_tmp_directory_and_removes_a_leftover() {
    let (mut h, spy) = harness("tmp-sweep", tmp_only);
    h.start(job("r1"));
    let gone = tmp_of(&h, "gone");
    fs::create_dir(&gone).unwrap();
    fs::write(gone.join("left"), b"left behind").unwrap();
    spy.inner.charge(
        tmp_project(7),
        Usage {
            bytes: 4096,
            inodes: 1,
        },
    );
    assert_eq!(names(&h.restart().unwrap()), ["adopted r1"]);
    assert!(!gone.exists(), "the leftover is removed");
    assert!(tmp_of(&h, "r1").is_dir(), "the adopted run's stays");
    assert!(
        clears(&spy).iter().any(|call| matches!(
            call,
            Call::Clear { project, .. } if *project == tmp_project(7)
        )),
        "the leftover's project is cleared"
    );
}

/// Asserts that a quota found unusable when the agent started (its scratch root cannot
/// enforce a project quota) starts no run: each start is refused `quota-unavailable`, with
/// the probe's reason, before anything is made; the start reports `quota-unhealthy`; and once
/// the probe passes, a later step reports the recovery and runs start. It catches an agent
/// that runs jobs with an unbounded `/tmp` on such a host, and one that never takes jobs
/// after the mount is fixed.
#[test]
fn a_quota_unusable_at_the_start_refuses_every_start_until_it_works() {
    let (mut h, spy) = harness("tmp-unusable", unusable);
    spy.set_probe_none(Some("xfs mounted without prjquota"));
    let events = h.start(job("r1"));
    assert_eq!(names(&events), ["refused r1 quota-unavailable"]);
    assert!(h.fake.created().is_empty());
    assert!(!h.scratch_of("r1").exists() && !tmp_of(&h, "r1").exists());
    let why: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Refused { why, .. } => Some(why.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        why,
        [
            "the scratch quota is unhealthy: the scratch root cannot enforce a project quota, \
          which bounds each job's /tmp and /var/tmp: xfs mounted without prjquota"
        ]
    );
    assert_eq!(
        names(&h.restart().unwrap()),
        ["finding - quota-unhealthy"],
        "a start whose quota cannot be used says so"
    );
    spy.set_probe_none(None);
    h.clock.advance(60_000);
    assert_eq!(names(&h.life.step()), ["finding - quota-recovered"]);
    assert_eq!(names(&h.start(job("r2"))), ["started r2"]);
}

/// The flags of a crun agent's jobs, with `extra`, checked as the agent checks them; no
/// `--scratch-quota` unless `extra` names it.
fn local(name: &str, extra: &[&str]) -> Result<js_agent::lifecycle::Local, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::support::Scratch::new(name);
    let root = dir.path("scratch");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let root = root.display().to_string();
    let mut argv = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
        "--pool",
        "p",
        "--scratch-root",
        &root,
    ];
    argv.extend(extra.iter().copied());
    let cli = <js_agent::Cli as clap::Parser>::try_parse_from(argv).unwrap();
    js_agent::lifecycle::Local::from_cli(&cli, 0)
}

/// Asserts `--job-tmp-gib`: without it a job's temporary directory is bounded to 1 GiB, with
/// it to the GiB it says (the edges 1 and 65535 taken), with or without `--scratch-quota`
/// (without it nothing else is bounded and nothing is booked), and only for a runtime that
/// gives each job a temporary directory; 0 and 65536 are refused, naming the flag. It catches
/// a flag that is checked and dropped, a default other than 1 GiB, a tmp bound that needs
/// `--scratch-quota` (the farm runs without it), and a bound of 0, which is no limit.
#[test]
fn the_tmp_flag_sets_the_bound_of_each_jobs_tmp_directory() {
    let fake = || Arc::new(js_agent::quota::FakeQuota::new());
    for (extra, gib) in [
        (vec![], 1),
        (vec!["--job-tmp-gib", "1"], 1),
        (vec!["--job-tmp-gib", "3"], 3),
        (vec!["--job-tmp-gib", "65535"], 65535),
    ] {
        let quota = local("tmp-flag", &extra).unwrap().run_quota(fake(), true);
        assert_eq!(quota.tmp_bytes(), gib * TMP_BYTES, "{extra:?}");
        assert!(quota.bounds_tmp() && !quota.bounds_scratch(), "{extra:?}");
        assert_eq!(quota.default_disk_bytes(), 0, "{extra:?}");
    }
    let both = local("tmp-flag-both", &["--scratch-quota", "--job-tmp-gib", "2"]).unwrap();
    let quota = both.run_quota(fake(), true);
    assert!(quota.bounds_tmp() && quota.bounds_scratch());
    assert_eq!(quota.tmp_bytes(), 2 * TMP_BYTES);
    let elsewhere = local("tmp-flag-other", &[])
        .unwrap()
        .run_quota(fake(), false);
    assert!(
        !elsewhere.bounds_tmp(),
        "a runtime with no temporary directory"
    );
    for (given, said) in [
        ("0", "--job-tmp-gib: 0 is not from 1 to 65535"),
        ("65536", "--job-tmp-gib: 65536 is not from 1 to 65535"),
    ] {
        let refused = local("tmp-flag-bad", &["--job-tmp-gib", given]).expect_err(given);
        assert_eq!(refused, said);
    }
}

/// Asserts that a quota that does not bound the jobs' temporary directories (a runtime given
/// none by the agent's flags) leaves the one the runtime names unbound: only the run's
/// scratch is bounded, and the directory is still made and removed with the run. It catches
/// a temporary project used when the agent did not ask for one.
#[test]
fn a_quota_that_bounds_no_tmp_directory_leaves_it_unbound() {
    let (mut h, spy) = harness("tmp-unbounded", |spy| {
        RunQuota::new(spy, DEFAULT_BYTES, INODES).without_tmp()
    });
    h.start(job("r1"));
    assert_eq!(
        bounds(&spy),
        [("r1".to_owned(), project(0), DEFAULT_BYTES, INODES)]
    );
    assert!(tmp_of(&h, "r1").is_dir());
    h.fake.exit("r1", 0);
    h.life.step();
    assert!(!tmp_of(&h, "r1").exists());
    assert_eq!(
        spy.seen().last().map(String::as_str),
        Some("clear r1 listed=[]")
    );
}
