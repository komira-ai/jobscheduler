//! Host jobs (`isolation = host`) over a real `crun`, as root: a host job shares the host's
//! namespaces, is accounted and limited in its lease's cgroup, and is ended whole by a stop
//! and by a removal (a cancel); a contained job beside it is still contained; and a runtime
//! that does not launch host jobs refuses one.
//!
//! A host job's root is the host's `/`, so its program is the host's `/bin/sh`, and it writes
//! what it sees to the run's directory of the test (`sync`) at its host path, then opens the
//! ready pipe there: the test reads the files once the pipe says the job is up. Nothing waits
//! for a clock.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use js_agent::crun::CrunRuntime;
use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent_wire::Isolation;

use crate::runtime_suite::{LONG, Program, Subject, spec_of};
use crate::subject::{CrunSubject, crun_subject};

/// The namespaces a host job shares with the host.
const SHARED: [&str; 7] = ["pid", "net", "mnt", "ipc", "uts", "user", "cgroup"];

/// The host job `run_id` that runs `script` (the host's `/bin/sh`, in the run's scratch
/// directory), with the subject's image and no mount.
fn host_spec(s: &mut CrunSubject, run_id: &str, script: &str) -> RunSpec {
    let mut spec = spec_of(s, run_id, Program::Hold);
    spec.isolation = Isolation::Host;
    spec.mounts = Vec::new();
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
    };
    spec
}

/// Starts `spec` on `runtime` and waits for the job to open the ready pipe of `sync`.
fn start_and_wait(runtime: &mut CrunRuntime, spec: &RunSpec, sync: &Path) -> RunHandle {
    let id = runtime.create_and_start(spec).expect("create and start");
    let said = fs::read_to_string(sync.join("ready")).expect("read the pipe");
    assert_eq!(said.trim(), "up");
    id
}

/// What the file `name` the job wrote in `sync` says, trimmed.
fn wrote(sync: &Path, name: &str) -> String {
    let path = sync.join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.trim().to_owned()
}

/// The interfaces `/proc/net/dev` lists, read in this test's network namespace.
fn interfaces(dev: &str) -> BTreeSet<String> {
    dev.lines()
        .skip(2)
        .filter_map(|line| line.split(':').next())
        .map(|name| name.trim().to_owned())
        .collect()
}

/// The namespace link of `what` of this test process (root, in the host's namespaces).
fn own_namespace(what: &str) -> String {
    let link = fs::read_link(format!("/proc/self/ns/{what}")).expect("a namespace link");
    link.display().to_string()
}

/// Proves a host job is in every namespace of the host: its pid, network, mount, IPC, UTS,
/// user and cgroup namespace links are this test's own, the pid it reports is a pid of the
/// host whose process is in the host's pid namespace, and it lists the host's network
/// interfaces. Catches a host job given namespaces of its own (it could not change the
/// machine), such as a contained job's network with loopback only.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_host_job_shares_the_hosts_namespaces_and_network() {
    let mut s = crun_subject("host_job_namespaces");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-ns");
    let sync = s.sync_dir("h1");
    let links: String = SHARED
        .iter()
        .map(|ns| format!("readlink /proc/self/ns/{ns} > {}/{ns}; ", sync.display()))
        .collect();
    let script = format!(
        "{links}cat /proc/net/dev > {dir}/netdev; echo $$ > {dir}/pid; \
         echo up > {dir}/ready; exec sleep 100000",
        dir = sync.display()
    );
    let spec = host_spec(&mut s, "h1", &script);
    let id = start_and_wait(&mut runtime, &spec, &sync);
    for ns in SHARED {
        assert_eq!(wrote(&sync, ns), own_namespace(ns), "the {ns} namespace");
    }
    let pid = wrote(&sync, "pid");
    let theirs = fs::read_link(format!("/proc/{pid}/ns/pid")).expect("the job's pid on the host");
    assert_eq!(theirs.display().to_string(), own_namespace("pid"));
    let host_dev = fs::read_to_string("/proc/net/dev").expect("the host's interfaces");
    assert_eq!(interfaces(&wrote(&sync, "netdev")), interfaces(&host_dev));
    assert!(
        interfaces(&host_dev).len() > 1,
        "more than loopback: {host_dev}"
    );
    assert_eq!(runtime.stop(&id, Duration::ZERO), Ok(()));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a host job is accounted and limited in its lease's cgroup: its process is in the
/// container's cgroup `ctr` directly below the lease's (as its `/proc/<pid>/cgroup` says and
/// the cgroup's `cgroup.procs` lists), and the lease's `memory.max` is the run's booking.
/// Catches a host job that runs in the agent's cgroup or the host's, where the booking does
/// not limit it and the lease's kill does not reach it.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_host_job_is_accounted_in_its_leases_cgroup() {
    let mut s = crun_subject("host_job_cgroup");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-cg");
    let sync = s.sync_dir("h2");
    let script = format!(
        "echo $$ > {dir}/pid; echo up > {dir}/ready; exec sleep 100000",
        dir = sync.display()
    );
    let spec = host_spec(&mut s, "h2", &script);
    let id = start_and_wait(&mut runtime, &spec, &sync);
    let ctr = runtime.cgroup(&id).expect("the container's cgroup");
    let lease = ctr.parent().expect("the lease's cgroup").to_owned();
    assert_eq!(lease, s.lease_cgroup(&id.0));
    let pid = wrote(&sync, "pid");
    let procs = fs::read_to_string(ctr.join("cgroup.procs")).expect("its processes");
    assert!(
        procs.lines().any(|line| line == pid),
        "{pid} not in {procs}"
    );
    let own = fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("the job's cgroup");
    assert!(own.trim().ends_with(&format!("/{}/ctr", id.0)), "{own}");
    let memory = fs::read_to_string(lease.join("memory.max")).expect("memory.max");
    assert_eq!(memory.trim(), spec.memory_bytes.to_string());
    assert_eq!(runtime.stop(&id, Duration::ZERO), Ok(()));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a stop and a cancel end a host job whole, the child it started in the background
/// included, although the job shares the host's pid namespace: a job that handles SIGTERM
/// ends with its own code when stopped; one that ignores it is killed through the lease's
/// `cgroup.kill` when the grace is spent (137); and one removed while it runs (a cancel) is
/// killed first. After each removal the lease's cgroup is gone (it could not be removed with
/// a process in it) and the background child is no longer in it. Catches a host job, or a
/// process it left, that outlives its stop or its cancel.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_stop_and_a_cancel_end_a_host_job_whole() {
    let mut s = crun_subject("host_job_stop");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-stop");
    let cases: [(&str, &str, Option<Duration>); 3] = [
        ("t1", "trap 'exit 7' TERM;", Some(LONG)),
        ("k1", "trap '' TERM;", Some(Duration::ZERO)),
        ("c1", "trap '' TERM;", None),
    ];
    for (run, trap, grace) in cases {
        let sync = s.sync_dir(run);
        let script = format!(
            "{trap} sleep 100000 & echo $! > {dir}/child; echo up > {dir}/ready; wait",
            dir = sync.display()
        );
        let spec = host_spec(&mut s, run, &script);
        let id = start_and_wait(&mut runtime, &spec, &sync);
        let child = wrote(&sync, "child");
        let lease = s.lease_cgroup(&id.0);
        if let Some(grace) = grace {
            assert_eq!(runtime.stop(&id, grace), Ok(()), "{run}");
            runtime.wait_ended(&id);
            let code = if grace == LONG { 7 } else { 137 };
            let want = Status::Exited(Exit {
                code,
                oom_killed: false,
            });
            assert_eq!(runtime.poll(&id), Ok(want), "{run}");
        }
        assert_eq!(runtime.remove(&id), Ok(()), "{run}");
        assert!(!lease.exists(), "{run}: the lease's cgroup is left");
        let left = fs::read_to_string(format!("/proc/{child}/cgroup")).unwrap_or_default();
        assert!(
            !left.contains(&id.0),
            "{run}: the child is still in the lease: {left}"
        );
    }
}

/// Proves a contained job on a runtime that launches host jobs is still contained: its pid
/// and network namespaces are not the host's. Catches the host path leaking into every job
/// once an agent allows host jobs.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_contained_job_beside_host_jobs_keeps_its_own_namespaces() {
    let mut s = crun_subject("host_job_contained");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-con");
    let sync = s.sync_dir("c2");
    let mut spec = spec_of(&mut s, "c2", Program::Hold);
    let inside = "/mnt/sync/c2";
    let script = format!(
        "readlink /proc/self/ns/pid > {inside}/pid; readlink /proc/self/ns/net > {inside}/net; \
         echo up > {inside}/ready; exec sleep 100000"
    );
    spec.body = Body::Container {
        image: s.image(),
        args: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
    };
    let id = start_and_wait(&mut runtime, &spec, &sync);
    for ns in ["pid", "net"] {
        assert_ne!(wrote(&sync, ns), own_namespace(ns), "the {ns} namespace");
    }
    assert_eq!(runtime.stop(&id, Duration::ZERO), Ok(()));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a runtime opened without host jobs (an agent started without `--allow-host-jobs`)
/// refuses a host start before anything is made: the refusal names the flag, nothing is
/// listed, and no lease cgroup of any name is made. Catches a host job launched on an agent that never
/// agreed to one.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn a_runtime_without_host_jobs_refuses_a_host_job() {
    let mut s = crun_subject("host_job_refused");
    s.prepared();
    let spec = host_spec(&mut s, "r1", "exit 0");
    let runtime = s.runtime();
    let refused = runtime.borrow_mut().create_and_start(&spec);
    let why = refused.expect_err("refused").to_string();
    assert_eq!(
        why,
        "a host job is refused: this agent was started without --allow-host-jobs"
    );
    assert!(
        runtime
            .borrow_mut()
            .list_labelled()
            .expect("list")
            .is_empty()
    );
    // No lease cgroup of any name: the leases' cgroup holds the helpers' alone.
    let helpers = s.lease_cgroup("helpers");
    let actions = helpers.parent().expect("the leases' cgroup");
    let entries = fs::read_dir(actions).expect("the leases' cgroup");
    let cgroups: Vec<String> = entries
        .map(|entry| entry.expect("an entry"))
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(cgroups, ["helpers"]);
}
