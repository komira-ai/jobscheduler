//! A run becoming a container, and a container's end read.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;

use js_agent::lifecycle::{Body, Exit, RunHandle, RunSpec, Runtime, Status, label};
use js_agent_wire::Isolation;
use js_agent_wire::RunFile;
use serde_json::{Value, json};

use crate::node::{LONG, Node, SUB_GID, SUB_UID, given};

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Proves the whole of a run the runtime makes and reads. A run becomes: the lease's
/// directories, given to the job user or to the job's id (first id of the subordinate range
/// plus 1000) in the order the layout says, then the run's scratch directory to the job's
/// id, then the lease's cgroup and its four files to the job user; a cgroup limited by the
/// booking (`memory.max` the booking, no swap, `cpu.max` the booked cores,
/// `memory.oom.group`); a bundle that runs the image's entrypoint with the run's arguments
/// in the image's environment with the scratch directory bound. It lists with the run's
/// label, the agent's and the time it was made. The program's exit 3 polls as 3, again as
/// the same, and removing it leaves no directory, no cgroup and no listing, repeated without
/// error. Catches a limit that is not the booking, files the job cannot write or `crun`
/// cannot use, and a removal that leaves a lease behind.
#[test]
fn a_run_is_made_started_read_and_removed() {
    let node = Node::new("run-life");
    let mut runtime = node.open();
    let before = given(&node.dir).len();
    let spec = node.spec("r1-exit");
    let id = runtime.create_and_start(&spec).expect("started");
    let name = id.0.clone();
    assert!(
        name.starts_with("js-") && name.ends_with("-r1-exit"),
        "{name}"
    );
    runtime.wait_ended(&id);
    let want = Status::Exited(Exit {
        code: 3,
        oom_killed: false,
    });
    assert_eq!(runtime.poll(&id), Ok(want));
    assert_eq!(runtime.poll(&id), Ok(want));

    let found = runtime.list_labelled().expect("lists");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert_eq!(found[0].running_ms, 0);
    assert!(!found[0].never_started);
    let labels = BTreeMap::from([
        (label::AGENT.to_owned(), "node-1".to_owned()),
        (label::CREATED_MS.to_owned(), "5000".to_owned()),
        (label::RUN.to_owned(), "r1-exit".to_owned()),
    ]);
    assert_eq!(found[0].labels, labels);

    let (lease, cgroup) = (node.lease_of(&name), node.cgroup_of(&name));
    assert_eq!(read(&cgroup.join("memory.max")), (1_u64 << 30).to_string());
    assert_eq!(read(&cgroup.join("memory.swap.max")), "0");
    assert_eq!(read(&cgroup.join("cpu.max")), "150000 100000");
    assert_eq!(read(&cgroup.join("memory.oom.group")), "1");

    let bundle: Value =
        serde_json::from_str(&read(&lease.join("bundle/config.json"))).expect("a config");
    assert_eq!(
        bundle["process"]["args"],
        json!(["/dev/js-init", "--", "/bin/tool", "--go"])
    );
    assert!(
        bundle["process"]["env"]
            .as_array()
            .expect("env")
            .contains(&json!("A=1"))
    );
    let mounts = bundle["mounts"].as_array().expect("mounts");
    let scratch = mounts
        .iter()
        .find(|mount| mount["destination"] == "/scratch")
        .expect("a scratch mount");
    assert_eq!(scratch["source"], json!(spec.scratch));

    let (job, job_id) = ((node.uid, node.gid), (SUB_UID + 1000, SUB_GID + 1000));
    let own = |path: std::path::PathBuf, (uid, gid): (u32, u32)| (path, uid, gid);
    assert_eq!(
        given(&node.dir)[before..],
        [
            own(lease.clone(), job),
            own(lease.join("root"), job_id),
            own(lease.join("upper"), job_id),
            own(lease.join("work"), job_id),
            own(lease.join("rw"), job),
            own(lease.join("rw/home"), job_id),
            own(spec.scratch.clone(), job_id),
            own(cgroup.join(""), job),
            own(cgroup.join("cgroup.procs"), job),
            own(cgroup.join("cgroup.threads"), job),
            own(cgroup.join("cgroup.subtree_control"), job),
        ]
    );

    assert_eq!(runtime.remove(&id), Ok(()));
    assert!(!lease.exists() && !cgroup.exists());
    assert_eq!(runtime.poll(&id), Ok(Status::Gone));
    assert!(runtime.list_labelled().expect("lists").is_empty());
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a container that is created and not started lists as `never_started` (and
/// listing removes nothing), polls as running as the lifecycle's contract says, has no
/// control group, stops without a call to `crun`, starts once (a second start, and the
/// start of one that is not there, are refused) and is removed. Catches a staged lease that
/// is taken for a job that ran.
#[test]
fn a_staged_container_lists_as_never_started_and_starts_once() {
    let node = Node::new("run-staged");
    let mut runtime = node.open();
    let id = runtime.stage(&node.spec("r2-hold")).expect("staged");
    let found = runtime.list_labelled().expect("lists");
    assert!(found[0].never_started);
    assert_eq!(runtime.list_labelled().expect("lists").len(), 1);
    assert_eq!(runtime.poll(&id), Ok(Status::Running));
    assert_eq!(runtime.cgroup(&id), None);
    assert_eq!(runtime.stop(&id, LONG), Ok(()));
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
    assert!(runtime.list_labelled().expect("lists").is_empty());

    let id = runtime.stage(&node.spec("r2-exit")).expect("staged");
    runtime.start_staged(&id).expect("started");
    let twice = runtime.start_staged(&id).expect_err("started once");
    assert!(twice.to_string().contains("was started"));
    let gone = runtime
        .start_staged(&RunHandle("js-none".to_owned()))
        .expect_err("not there");
    assert!(gone.to_string().contains("is gone"));
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
    runtime.wait_ended(&id);
}

/// Proves an out-of-memory kill is told apart by whose limit it was: the kernel's killer
/// ran for the lease's own limit (`memory.events` counts `oom` and `max` in the lease), the
/// exit is 137 and flagged; it killed a process of a lease whose own limit did not make it
/// run (a node short of memory), the exit is 137 and not flagged. Catches a flag set for
/// every kill, or for none.
#[test]
fn a_kill_is_flagged_out_of_memory_only_at_the_leases_own_limit() {
    let node = Node::new("run-oom");
    let mut runtime = node.open();
    for (run, oom_killed) in [("r3-oom", true), ("r4-busy", false)] {
        let id = runtime.create_and_start(&node.spec(run)).expect("started");
        runtime.wait_ended(&id);
        let want = Status::Exited(Exit {
            code: 137,
            oom_killed,
        });
        assert_eq!(runtime.poll(&id), Ok(want), "{run}");
        runtime.remove(&id).expect("removed");
    }
}

/// Proves a container that did not run (crun's log has an error record) polls as exited
/// with code 126, not as the exit status of crun. Catches a failed start that reads as a
/// job that failed.
#[test]
fn a_container_that_did_not_run_exits_126() {
    let node = Node::new("run-fail");
    let mut runtime = node.open();
    let id = runtime
        .create_and_start(&node.spec("r5-fail"))
        .expect("started");
    runtime.wait_ended(&id);
    let want = Status::Exited(Exit {
        code: 126,
        oom_killed: false,
    });
    assert_eq!(runtime.poll(&id), Ok(want));
    runtime.remove(&id).expect("removed");
}

/// Proves each start the runtime cannot make is refused with its reason and creates nothing:
/// a host job, a GPU, an image that is not pinned, a run id that cannot name a container,
/// one that has a container, run files where there is no files directory, an image the
/// store lacks and cannot fetch, an image with no kept config, and a hand-over that fails
/// (the scratch directory), which also removes the lease directory it made. Catches a
/// refusal that leaves a directory or a listing behind.
#[test]
fn a_start_the_runtime_cannot_make_is_refused_and_leaves_nothing() {
    let node = Node::new("run-declined-start");
    let mut runtime = node.open();
    let other = format!("registry.example/job@sha256:{}", "cd".repeat(32));
    let named = |image: &str| {
        let image = image.to_owned();
        move |s: &mut RunSpec| {
            s.body = Body::Container {
                image: image.clone(),
                args: Vec::new(),
            }
        }
    };
    let mut refused = |run: &str, mutate: &dyn Fn(&mut RunSpec)| {
        let mut spec = node.spec(run);
        mutate(&mut spec);
        runtime.create_and_start(&spec).expect_err(run).to_string()
    };
    assert!(refused("h1", &|s| s.isolation = Isolation::Host).contains("contained jobs only"));
    assert!(refused("g1", &|s| s.gpu_indices = vec![0]).contains("cannot use a GPU"));
    assert!(refused("u1", &named("registry.example/job:1")).contains("not pinned"));
    assert!(refused("a/b", &|_| {}).contains("cannot name a container"));
    assert!(refused("f1", &|s| s.files = Some(s.scratch.clone())).contains("no run files"));
    assert!(refused("i1", &named(&other)).contains("no registry in this test"));
    std::fs::create_dir_all(
        node.scratch
            .join("oci-images")
            .join("cd".repeat(32))
            .join("rootfs"),
    )
    .expect("an image without a config");
    assert!(refused("c1", &named(&other)).contains("cannot be read"));
    let elsewhere = node.dir.join("refuse-scratch");
    std::fs::create_dir_all(&elsewhere).expect("scratch");
    let why = refused("s1-exit", &|s| s.scratch = elsewhere.clone());
    assert!(why.contains("scratch directory to the job"), "{why}");
    assert!(runtime.list_labelled().expect("lists").is_empty());
    let leftovers: Vec<String> = std::fs::read_dir(&node.scratch)
        .expect("store")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("js-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// Proves a run id that has a container is refused while that container exists, and is
/// accepted again once it is removed. Catches two containers under one name.
#[test]
fn a_run_id_with_a_container_is_refused_until_it_is_removed() {
    let node = Node::new("run-in-use");
    let mut runtime = node.open();
    let id = runtime
        .create_and_start(&node.spec("r6-exit"))
        .expect("started");
    let again = runtime
        .create_and_start(&node.spec("r6-exit"))
        .expect_err("in use");
    assert!(again.to_string().contains("exists"));
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
    let id = runtime
        .create_and_start(&node.spec("r6-exit"))
        .expect("again");
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");
}

/// Proves a start whose `crun` cannot be started is taken apart: the error is the start's,
/// and no container, directory or listing is left. Catches a failed start that leaves a
/// lease to be adopted.
#[test]
fn a_start_that_cannot_run_crun_is_taken_apart() {
    let node = Node::new("run-start-fails");
    let mut runtime = node.open();
    let why = runtime
        .create_and_start(&node.spec("r7-exit"))
        .expect_err("not started");
    assert!(why.to_string().contains("start crun"), "{why}");
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves the run's files are given to the job: with a files root, the files of the run
/// are handed over (each file and the directory, through the same call as everything else
/// given away) before the container starts, and a run whose files directory is not there
/// is refused. Catches a job that cannot read its files.
#[test]
fn the_files_of_a_run_are_given_to_the_job() {
    let node = Node::new("run-files");
    let (_files_dir, root) = node.files_root();
    let mut settings = node.settings("/root");
    settings.files = Some(root.clone());
    let hooks = Node::hooks(LONG, LONG);
    let mut runtime = js_agent::crun::CrunRuntime::open_with(
        settings,
        std::sync::Arc::new(crate::node::NoImages),
        hooks,
    )
    .expect("opens");
    let file = RunFile {
        name: "token".to_owned(),
        contents: "secret".to_owned(),
    };
    let dir = root.write("r8-exit", &[file]).expect("files");
    let mut spec = node.spec("r8-exit");
    spec.files = Some(dir.clone());
    let id = runtime.create_and_start(&spec).expect("started");
    let ids = (SUB_UID + 1000, SUB_GID + 1000);
    let given = given(&node.dir);
    assert!(given.contains(&(dir.join("token"), ids.0, ids.1)));
    assert!(given.contains(&(dir.clone(), ids.0, ids.1)));
    assert_eq!(
        std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
        0o500
    );
    runtime.wait_ended(&id);
    runtime.remove(&id).expect("removed");

    let mut spec = node.spec("r9-exit");
    spec.files = Some(root.of("r9-exit"));
    let why = runtime.create_and_start(&spec).expect_err("no files");
    assert!(why.to_string().contains("files to the job"), "{why}");
}
