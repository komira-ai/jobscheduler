//! What a job can do to its lease and to the other jobs on the node: run it out of memory,
//! leave processes behind when it is killed, use up the processes it may make.

use std::path::PathBuf;

use js_contain::cgroup::OomOutcome;
use js_contain::container::ContainerLimits;

use crate::cell::{Cell, Lease, Outcome, Shape};

/// Catches an out-of-memory kill that is not reported as the lease's own: a job that
/// allocates until the lease's `memory.max` is killed by the kernel (the run ends when the
/// kill does; nothing is timed), it does not exit 0, and the lease cgroup's counters say
/// `OwnLimit` with the lease's limit, the answer a caller retries with more memory on.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn an_out_of_memory_kill_is_reported_as_the_leases_own_limit() {
    let cell = Cell::new("oom");
    let lease = cell.lease(
        "oom",
        // One line that never ends, held in memory by `tail`.
        &["/bin/tail", "/dev/zero"],
        Shape {
            memory: 192 << 20,
            ..Shape::default()
        },
    );
    let running = cell.start(&lease).await;
    let run = cell.finish(&lease, running).await;
    assert!(!run.succeeded(), "{}", run.said);
    let outcome = lease
        .cgroup
        .oom_outcome(Some(lease.cap), None)
        .unwrap_or_else(|e| panic!("read the lease's memory.events: {e}"));
    match outcome {
        Some(OomOutcome::OwnLimit { limit, .. }) => assert_eq!(limit, lease.cap, "{}", run.said),
        other => panic!("not the lease's own limit: {other:?}\n{}", run.said),
    }
}

/// A lease running a job that says it is up (by opening a pipe the test reads) and then
/// waits for a pipe nobody opens, so it runs until it is ended from outside.
async fn running_until_ended(cell: &Cell, name: &str) -> (Lease, js_contain::running::Running) {
    let sync = cell.sync_dir(&format!("{name}-sync"), &["ready", "hold"]);
    let ready: PathBuf = sync.source.join("ready");
    let lease = cell.lease(
        name,
        &["/bin/sh", "-c", "echo ready > /sync/ready; cat /sync/hold"],
        Shape {
            mounts: vec![sync],
            ..Shape::default()
        },
    );
    let mut running = cell.start(&lease).await;
    // Opening the pipe for reading blocks until the job opens it for writing: the job is
    // up when this returns. A container that ends first is a failure, not a wait.
    let opened = tokio::task::spawn_blocking(move || std::fs::read_to_string(ready));
    tokio::select! {
        said = opened => {
            let said = said.expect("the reader task").expect("read the pipe");
            assert_eq!(said.trim(), "ready");
        }
        () = running.wait() => {
            let run = cell.finish(&lease, running).await;
            panic!("the container ended before it was up: {}", run.said);
        }
    }
    (lease, running)
}

/// What is left of `lease` after it was ended and removed: nothing may be. The kernel says
/// its cgroup holds no process (`populated 0` in `cgroup.events` counts the cgroups below
/// it), the container's cgroup is gone, and the lease's own can be removed.
fn assert_nothing_left(lease: &Lease, run: &Outcome) {
    assert!(!run.succeeded(), "{}", run.said);
    let events = std::fs::read_to_string(lease.cgroup.dir().join("cgroup.events"))
        .unwrap_or_else(|e| panic!("read cgroup.events: {e}"));
    assert!(
        events.lines().any(|l| l == "populated 0"),
        "a process is left in the lease: {events}\n{}",
        run.said
    );
    let ctr = lease.cgroup.dir().join("ctr");
    assert!(!ctr.exists(), "{} is left: {}", ctr.display(), run.said);
    lease
        .cgroup
        .remove()
        .unwrap_or_else(|e| panic!("remove the lease cgroup: {e}\n{}", run.said));
    assert!(
        !lease.cgroup.dir().exists(),
        "{} is left: {}",
        lease.cgroup.dir().display(),
        run.said
    );
}

/// Catches a stop that leaves the container: `crun kill` with SIGTERM ends a job that
/// waits for nothing, the run ends, and no process, container cgroup or lease cgroup is
/// left.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_terminated_container_leaves_no_cgroup() {
    let cell = Cell::new("term");
    let (lease, running) = running_until_ended(&cell, "term").await;
    cell.oci
        .terminate(&lease.spec.name)
        .await
        .unwrap_or_else(|e| panic!("terminate: {e}"));
    let run = cell.finish(&lease, running).await;
    assert_nothing_left(&lease, &run);
}

/// Catches a lease kill (`cgroup.kill`, what the driver does at a wall limit or a swap
/// kill) that leaves a process or a cgroup behind, for a job blocked on a pipe.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_killed_lease_leaves_no_process_and_no_cgroup() {
    let cell = Cell::new("kill");
    let (lease, running) = running_until_ended(&cell, "kill").await;
    lease
        .cgroup
        .kill()
        .unwrap_or_else(|e| panic!("kill the lease cgroup: {e}"));
    let run = cell.finish(&lease, running).await;
    assert_nothing_left(&lease, &run);
}

/// The two jobs of the process-limit tests, run side by side:
///
/// - `a` makes processes, each one blocked on a pipe nobody writes, until its process limit
///   refuses another, then reads its own limit and count (`pids.max`, `pids.current`),
///   tells `b` through a pipe and waits for `b` to say it is done;
/// - `b`, which runs as the same ids but in its own lease, waits for `a`'s word, makes
///   twenty processes, and says so (`b-forked=20`), then releases `a`.
///
/// The jobs are ordered by the opens of those pipes. If `a` ends before `b` has done its
/// part it failed, and the test says so instead of waiting for `b`.
async fn exhaust_one_and_fork_in_the_other(
    cell: &Cell,
    a_limits: ContainerLimits,
) -> (Outcome, Outcome) {
    const A: &str = "read max < /sys/fs/cgroup/pids.max
sh -c 'i=0; while [ \"$i\" -lt 400 ]; do cat /sync/hold & i=$((i+1)); done'
read cur < /sys/fs/cgroup/pids.current
echo \"a-pids=$cur/$max\"
echo saturated > /sync/saturated
read go < /sync/release
echo a-released";
    const B: &str = "read said < /sync/saturated
echo \"a-said=$said\"
i=0
while [ \"$i\" -lt 20 ]; do /bin/true & i=$((i+1)); done
wait
echo \"b-forked=$i\"
echo go > /sync/release";
    let sync = cell.sync_dir("exhaust-sync", &["hold", "saturated", "release"]);
    let a = cell.lease(
        "exhaust-a",
        &["/bin/sh", "-c", A],
        Shape {
            limits: a_limits,
            mounts: vec![sync.clone()],
            ..Shape::default()
        },
    );
    let b = cell.lease(
        "exhaust-b",
        &["/bin/sh", "-c", B],
        Shape {
            mounts: vec![sync],
            ..Shape::default()
        },
    );
    let (running_a, running_b) = (cell.start(&a).await, cell.start(&b).await);
    let finish_a = cell.finish(&a, running_a);
    let finish_b = cell.finish(&b, running_b);
    tokio::pin!(finish_a, finish_b);
    tokio::select! {
        run_a = &mut finish_a => {
            assert!(run_a.succeeded(), "a ended before b was done: {}", run_a.said);
            (run_a, finish_b.await)
        }
        run_b = &mut finish_b => {
            assert!(run_b.succeeded(), "b ended badly: {}", run_b.said);
            (finish_a.await, run_b)
        }
    }
}

/// What the two jobs of [`exhaust_one_and_fork_in_the_other`] must show: `a` was refused a
/// process (the shell says it could not fork), and `b`, which runs as the same ids, made
/// its processes all the same.
fn assert_isolated(a: &Outcome, b: &Outcome) {
    assert!(a.succeeded() && b.succeeded(), "{}\n{}", a.said, b.said);
    assert!(
        a.stderr.contains("fork"),
        "a was never refused a process: {}",
        a.said
    );
    assert!(a.stdout.contains("a-released"), "{}", a.said);
    assert!(b.stdout.lines().any(|l| l == "b-forked=20"), "{}", b.said);
    assert!(
        b.stdout.lines().any(|l| l.starts_with("a-said=saturated")),
        "{}",
        b.said
    );
}

/// Catches a process limit that is shared between leases (G9 of the plan: every job runs
/// as the same ids): job `a`, whose lease allows 32 processes, uses them all and is refused
/// the next fork ("can't fork"), and while it holds them, job `b` in another lease makes
/// twenty processes. `a`'s `pids.current` read from inside is at its `pids.max`, 32 (the
/// forker that was refused has gone, so it can be one less).
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_job_at_its_pids_limit_does_not_stop_another_job_from_forking() {
    let cell = Cell::new("pids");
    let limits = ContainerLimits {
        pids: 32,
        ..ContainerLimits::DEFAULT
    };
    let (a, b) = exhaust_one_and_fork_in_the_other(&cell, limits).await;
    assert_isolated(&a, &b);
    let pids = a
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("a-pids="))
        .and_then(|l| l.split_once('/'))
        .map(|(cur, max)| (cur.parse::<u64>(), max.parse::<u64>()));
    match pids {
        Some((Ok(cur), Ok(max))) => {
            assert_eq!(max, 32, "{}", a.said);
            assert!(cur >= max - 1, "{cur} of {max}: {}", a.said);
        }
        other => panic!("no process count from a: {other:?}\n{}", a.said),
    }
}

/// Catches `RLIMIT_NPROC` counted across jobs: the same two jobs, with `a` bounded by
/// `RLIMIT_NPROC` 256 (its lease allows 8192 processes), `a` is refused the next fork and
/// `b`, whose limit is the default, makes twenty. `a` reads its own `pids.max` from inside,
/// so the refusal is the rlimit's and not the cgroup's.
///
/// The limit is 256 and not lower because a container given 32 was refused its first
/// `exec` ("Resource temporarily unavailable") in a run where the other tests' containers
/// were starting as the same job user: a low `RLIMIT_NPROC` can fail a job before it
/// starts, whichever jobs are running beside it. That is why the lease's `pids.max` is the
/// bound a job meets and `NPROC_HIGH` the default.
#[tokio::test]
#[ignore = "real crun, root and a job user: run through cargo xtask crun-tests --job-user"]
async fn a_job_at_its_nproc_limit_does_not_stop_another_job_from_forking() {
    let cell = Cell::new("nproc");
    let limits = ContainerLimits {
        nproc: 256,
        ..ContainerLimits::DEFAULT
    };
    let (a, b) = exhaust_one_and_fork_in_the_other(&cell, limits).await;
    assert_isolated(&a, &b);
    let max = a
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("a-pids="))
        .and_then(|l| l.split_once('/'))
        .map(|(_, max)| max.to_owned());
    assert_eq!(max.as_deref(), Some("8192"), "{}", a.said);
}
