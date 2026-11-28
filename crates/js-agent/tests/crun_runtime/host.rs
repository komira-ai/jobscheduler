//! Host jobs (`isolation = host`) over a real `crun`, as root: a host job shares the host's
//! namespaces, is accounted and limited in its lease's cgroup, and is ended whole by a stop
//! and by a removal (a cancel); a contained job beside it is still contained; and a runtime
//! that does not launch host jobs refuses one. Host jobs start while another host start is
//! at `crun`'s step that replaces the host's `/dev/ptmx`.
//!
//! A host job's root is the host's `/`, so its program is the host's `/bin/sh`, and it writes
//! what it sees to the run's directory of the test (`sync`) at its host path, then opens the
//! ready pipe there: the test reads the files once the pipe says the job is up. Nothing waits
//! for a clock: the wait for the pipe also ends when the run does (its exit record appears),
//! and a job that ends before it is up fails the test with how it ended.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;

use js_agent::crun::CrunRuntime;
use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent_wire::Isolation;
use js_contain::oci::exit_record_path;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{OFlags, inotify};

use crate::runtime_suite::{Program, Subject, spec_of};
use crate::subject::{CrunSubject, crun_subject};

/// The grace of the stop whose SIGTERM the job handles: far more than a job that ends on it
/// takes, and a bound for one that does not, which the stop then kills (exit 137, not 7)
/// instead of waiting for ever.
const TERM_GRACE: Duration = Duration::from_secs(60);

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

/// Starts `spec` on `runtime` and waits until the job says `up` in the ready pipe of `sync`,
/// or its run ends first, which fails the test with how it ended and what it wrote to stderr.
fn start_and_wait(
    s: &CrunSubject,
    runtime: &mut CrunRuntime,
    spec: &RunSpec,
    sync: &Path,
) -> RunHandle {
    let id = runtime.create_and_start(spec).expect("create and start");
    let Some(said) = up_or_ended(&sync.join("ready"), &s.lease_dir(&id.0)) else {
        let status = runtime.poll(&id);
        let tail = runtime.log_tail(&id).ok().flatten().unwrap_or_default();
        panic!(
            "the host job ended before it was up: {status:?}, stderr {:?}",
            tail.stderr
        );
    };
    assert_eq!(said, "up");
    id
}

/// The first line the job writes to the pipe `ready`, trimmed, or `None` once the exit record
/// of the run whose lease directory is `lease` is there and the job wrote no line. Waits for
/// one of the two events, with no clock: `create_and_start` returns before `crun run` has
/// started, so a run that fails or ends before its job opens the pipe is seen only by its
/// record, which the supervisor writes once `crun run` has returned.
fn up_or_ended(ready: &Path, lease: &Path) -> Option<String> {
    // Read and write: the open does not wait for the job, and the pipe never reads as ended
    // while this end is open. Not blocking: a read takes what is there.
    let mut pipe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(ready)
        .expect("the ready pipe");
    // Watched before the record is looked for, so it cannot appear unseen in between.
    let watch = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
        .expect("an inotify");
    inotify::add_watch(
        &watch,
        lease,
        inotify::WatchFlags::CREATE | inotify::WatchFlags::MOVED_TO,
    )
    .expect("watch the lease directory");
    let record = exit_record_path(lease);
    let mut said = Vec::new();
    loop {
        // Looked for before the pipe is read: a job writes its line before it ends, and the
        // record is written after that, so a run seen ended has its line in the pipe already.
        let ended = record.exists();
        take_what_is_there(&mut pipe, &mut said);
        if let Some(end) = said.iter().position(|&byte| byte == b'\n') {
            return Some(String::from_utf8_lossy(&said[..end]).trim().to_owned());
        }
        if ended {
            return None;
        }
        let mut fds = [
            PollFd::new(&pipe, PollFlags::IN),
            PollFd::new(&watch, PollFlags::IN),
        ];
        // A signal that interrupts the wait only wakes the loop, which looks again.
        match poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(why) => panic!("wait for the pipe or the lease directory: {why}"),
        }
        // The events only wake the loop, which looks again; they are read so the next poll
        // waits for new ones.
        let mut events = [0u8; 4096];
        while rustix::io::read(&watch, &mut events).is_ok() {}
    }
}

/// Appends to `said` what the non-blocking `pipe` holds now.
fn take_what_is_there(pipe: &mut fs::File, said: &mut Vec<u8>) {
    let mut chunk = [0u8; 64];
    while let Ok(read @ 1..) = pipe.read(&mut chunk) {
        said.extend_from_slice(&chunk[..read]);
    }
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
/// host whose process is in the host's pid namespace, and it lists an interface other than
/// loopback. Catches a host job given namespaces of its own (it could not change the
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
        "{links}cat /proc/net/dev > {dir}/netdev; echo $$ > {dir}/process; \
         echo up > {dir}/ready; exec sleep 100000",
        dir = sync.display()
    );
    let spec = host_spec(&mut s, "h1", &script);
    let id = start_and_wait(&s, &mut runtime, &spec, &sync);
    // Every namespace at once, so a failure names all that differ.
    let seen: Vec<(&str, String, String)> = SHARED
        .iter()
        .map(|ns| (*ns, wrote(&sync, ns), own_namespace(ns)))
        .filter(|(_, job, host)| job != host)
        .collect();
    assert!(
        seen.is_empty(),
        "namespaces not the host's (job, host): {seen:?}"
    );
    let pid = wrote(&sync, "process");
    let theirs = fs::read_link(format!("/proc/{pid}/ns/pid")).expect("the job's pid on the host");
    assert_eq!(theirs.display().to_string(), own_namespace("pid"));
    // The network namespace is the host's (above); what the job lists of it is more than a
    // contained job's loopback. The host's own list is not compared: another test running
    // beside this one may add or remove an interface between the two reads.
    let listed = wrote(&sync, "netdev");
    let seen = interfaces(&listed);
    assert!(
        seen.iter().any(|name| name != "lo"),
        "the job sees loopback only: {listed}"
    );
    assert_eq!(runtime.stop(&id, Duration::ZERO), Ok(()));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// The file every host start on a node locks while `crun` is at its `/dev` step: the runtime
/// takes the same one (js-contain's `oci::start_lock`).
const HOST_START_LOCK: &str = "/dev/.js-host-start.lock";

/// How many host jobs [`host_jobs_start_while_another_start_replaces_dev_ptmx`] starts.
const PTMX_STARTS: usize = 24;

/// What `crun` does to `<root>/dev/ptmx` as it starts a host container, whose root is the
/// host's `/` (crun's `create_missing_devs`): a link `ptmx -> pts/ptmx` made by force, an
/// entry already there unlinked and the link made again. Done under the host start lock, as
/// every host start takes it. What it cannot do (another start removed or made the entry in
/// between) is left: the next round makes the link again.
fn replace_dev_ptmx_as_crun_does(lock: &fs::File) {
    lock.lock().expect("the host start lock");
    let ptmx = Path::new("/dev/ptmx");
    if std::os::unix::fs::symlink("pts/ptmx", ptmx).is_err() {
        let _ = fs::remove_file(ptmx);
        let _ = std::os::unix::fs::symlink("pts/ptmx", ptmx);
    }
    lock.unlock().expect("unlock the host start lock");
}

/// Proves a host job starts while another host start is at the step of `crun`'s that
/// replaces the host's `/dev/ptmx`: a thread does that step over and over (as a second host
/// job starting on the node does, under the host start lock) while [`PTMX_STARTS`] host jobs
/// start one after another, and every one of them comes up. Afterwards `/dev/ptmx` is the link
/// to `pts/ptmx` both leave. Catches host starts that are not serialized on the node: `crun`
/// of one start unlinks the `/dev/ptmx` the other is about to unlink, and its start fails
/// with `creating symlink for /dev/ptmx: File exists` (the job ends 126 before it is up).
///
/// It changes the host's `/dev/ptmx` as every host job's `crun` does.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
fn host_jobs_start_while_another_start_replaces_dev_ptmx() {
    let mut s = crun_subject("host_job_ptmx");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-ptmx");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(HOST_START_LOCK)
        .expect("the host start lock");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let other = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                replace_dev_ptmx_as_crun_does(&lock);
                // Lets a start waiting for the lock take it between two rounds.
                std::thread::yield_now();
            }
            replace_dev_ptmx_as_crun_does(&lock);
        })
    };
    for n in 0..PTMX_STARTS {
        let run = format!("p{n}");
        let sync = s.sync_dir(&run);
        let script = format!("echo up > {}/ready; exec sleep 100000", sync.display());
        let spec = host_spec(&mut s, &run, &script);
        let id = start_and_wait(&s, &mut runtime, &spec, &sync);
        assert_eq!(runtime.stop(&id, Duration::ZERO), Ok(()), "{run}");
        assert_eq!(runtime.remove(&id), Ok(()), "{run}");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    other.join().expect("the other start's thread");
    let link = fs::read_link("/dev/ptmx").expect("/dev/ptmx is a link");
    assert_eq!(link, Path::new("pts/ptmx"));
}

/// Proves a host job is accounted in its lease's cgroup: its process is in the
/// container's cgroup `ctr` directly below the lease's (as its `/proc/<pid>/cgroup` says and
/// the cgroup's `cgroup.procs` lists), and the lease's `memory.max` reads `max`: a host job's
/// memory booking is no limit either.
/// Catches a host job that runs in the agent's cgroup or the host's, where its use is not
/// counted with its lease's and the lease's kill does not reach it.
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
    let id = start_and_wait(&s, &mut runtime, &spec, &sync);
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
    assert_eq!(memory.trim(), "max");
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
        ("t1", "trap 'exit 7' TERM;", Some(TERM_GRACE)),
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
        let id = start_and_wait(&s, &mut runtime, &spec, &sync);
        let child = wrote(&sync, "child");
        let lease = s.lease_cgroup(&id.0);
        if let Some(grace) = grace {
            assert_eq!(runtime.stop(&id, grace), Ok(()), "{run}");
            runtime.wait_ended(&id);
            let code = if grace == TERM_GRACE { 7 } else { 137 };
            let want = Status::Exited(Exit {
                code,
                oom_killed: false,
            });
            // A job that SIGTERM did not end whole is killed when the grace is spent, and
            // ends 137 instead of 7.
            assert_eq!(
                runtime.poll(&id),
                Ok(want),
                "{run}: the host job did not end whole on SIGTERM within its grace"
            );
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
    let id = start_and_wait(&s, &mut runtime, &spec, &sync);
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

/// Proves a host job that ends before it says it is up fails its start with how it ended
/// and what it wrote to stderr, instead of leaving the test waiting for ever on a ready pipe
/// that nothing will open: the job writes a line to stderr and exits 3 without opening it.
/// Catches a start that waits only for the pipe.
#[test]
#[ignore = "real crun: run through cargo xtask crun-tests"]
#[should_panic(
    expected = "the host job ended before it was up: Ok(Exited(Exit { code: 3, oom_killed: false })), stderr \"gone\\n\""
)]
fn a_host_job_that_ends_before_it_is_up_fails_its_start() {
    let mut s = crun_subject("host_job_ends_early");
    s.prepared();
    let mut runtime = s.open_allowing_host_jobs("crun-host-early");
    let sync = s.sync_dir("e1");
    let spec = host_spec(&mut s, "e1", "echo gone >&2; exit 3");
    start_and_wait(&s, &mut runtime, &spec, &sync);
}
