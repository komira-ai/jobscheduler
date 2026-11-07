//! A host job (`isolation = host`) on a runtime opened with host jobs allowed: the lease and
//! bundle it gets, the starts it is refused, and its stop, kill and removal through the
//! lease's cgroup, as for a contained job.

use std::sync::Arc;
use std::time::Duration;

use js_agent::crun::{CrunRuntime, CrunSettings};
use js_agent::lifecycle::{Exit, RunHandle, RunSpec, Runtime, Status};
use js_agent_wire::Isolation;
use js_types::JobNetwork;
use serde_json::{Value, json};

use crate::node::{LONG, NoImages, Node, given};

/// A runtime on `node` that launches host jobs (`--allow-host-jobs`).
fn open_allowing(node: &Node) -> CrunRuntime {
    let settings = CrunSettings {
        host_jobs: true,
        ..node.settings("/root")
    };
    CrunRuntime::open_with(settings, Arc::new(NoImages), Node::hooks(LONG, LONG)).expect("opens")
}

/// The run `run_id` of `node` as a host job.
fn host(node: &Node, run_id: &str) -> RunSpec {
    RunSpec {
        isolation: Isolation::Host,
        ..node.spec(run_id)
    }
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn exited(code: i32) -> Status {
    Status::Exited(Exit {
        code,
        oom_killed: false,
    })
}

/// Stages the host run `run`, starts it once it can say it is up, and waits for that.
fn started_and_up(node: &Node, runtime: &mut CrunRuntime, run: &str) -> RunHandle {
    let id = runtime.stage(&host(node, run)).expect("staged");
    node.pipe(&id.0);
    runtime.start_staged(&id).expect("started");
    node.up(&id);
    id
}

/// Proves the whole of a host run the runtime makes and reads. It becomes: the lease's
/// directory alone (no overlay, home or input root), given to the agent itself, as are the
/// run's scratch directory and the lease's cgroup, and nothing given to the job's ids; a cgroup limited by the booking as a contained run's (`memory.max`, no
/// swap, `cpu.max`, `memory.oom.group`); a bundle with no namespace, the host's `/` as its
/// root, no mount, root with every capability, that runs the image's entrypoint with the
/// run's arguments under the init at its host path, in the run's scratch directory. Its
/// exit 3 polls as 3, and removing it leaves no directory, no cgroup and no listing.
/// Catches a host job given a contained job's sandbox, a lease handed to the job user (whose
/// `crun` cannot make a container without a user namespace), and limits that are not the
/// booking's.
#[test]
fn a_host_run_is_made_started_read_and_removed() {
    let node = Node::new("host-life");
    let mut runtime = open_allowing(&node);
    let before = given(&node.dir).len();
    let spec = host(&node, "r1-exit");
    let id = runtime.create_and_start(&spec).expect("started");
    runtime.wait_ended(&id);
    assert_eq!(runtime.poll(&id), Ok(exited(3)));

    let (lease, cgroup) = (node.lease_of(&id.0), node.cgroup_of(&id.0));
    assert_eq!(read(&cgroup.join("memory.max")), (1_u64 << 30).to_string());
    assert_eq!(read(&cgroup.join("memory.swap.max")), "0");
    assert_eq!(read(&cgroup.join("cpu.max")), "150000 100000");
    assert_eq!(read(&cgroup.join("memory.oom.group")), "1");
    for dir in ["root", "upper", "work", "rw", "etc"] {
        assert!(!lease.join(dir).exists(), "{dir}");
    }
    // In the stand-in the agent's ids and the job user's are this test's: what tells them
    // apart is that nothing goes to the job's ids and only the lease directory is made.
    let own = |path: std::path::PathBuf| (path, node.uid, node.gid);
    assert_eq!(
        given(&node.dir)[before..],
        [
            own(lease.clone()),
            own(spec.scratch.clone()),
            own(cgroup.clone()),
            own(cgroup.join("cgroup.procs")),
            own(cgroup.join("cgroup.threads")),
            own(cgroup.join("cgroup.subtree_control")),
        ]
    );

    let bundle: Value =
        serde_json::from_str(&read(&lease.join("bundle/config.json"))).expect("a config");
    assert_eq!(bundle["linux"]["namespaces"], json!([]));
    assert_eq!(bundle["root"], json!({"path": "/", "readonly": false}));
    assert_eq!(bundle["mounts"], json!([]));
    assert_eq!(bundle["process"]["user"], json!({"uid": 0, "gid": 0}));
    assert_eq!(bundle["process"]["cwd"], json!(spec.scratch));
    assert_eq!(
        bundle["linux"]["cgroupsPath"],
        json!(format!("/actions/{}/ctr", id.0))
    );
    let init = node.scratch.join("oci-bin/js-init");
    assert_eq!(
        bundle["process"]["args"],
        json!([init, "--end-descendants", "--", "/bin/tool", "--go"])
    );
    let env = bundle["process"]["env"].as_array().expect("env");
    assert!(env.contains(&json!("A=1")), "{env:?}");
    let held = bundle["process"]["capabilities"]["effective"]
        .as_array()
        .expect("capabilities");
    assert!(held.contains(&json!("CAP_SYS_ADMIN")), "{held:?}");

    assert_eq!(runtime.remove(&id), Ok(()));
    assert!(!lease.exists() && !cgroup.exists());
    assert_eq!(runtime.poll(&id), Ok(Status::Gone));
    assert!(runtime.list_labelled().expect("lists").is_empty());
}

/// Proves a host start is refused, before anything is made, by a runtime opened without
/// host jobs (an agent started without `--allow-host-jobs`), and by one that allows them when
/// it names a mount, run files or a GPU (it sees the host's own paths and devices); each
/// refusal leaves no lease directory and no cgroup of any name, and no listing. Catches a host job launched
/// on an agent that never agreed to one, and a host job given a bind that would change the
/// host's mounts.
#[test]
fn a_host_start_is_refused_without_the_flag_and_with_a_mount_files_or_a_gpu() {
    let node = Node::new("host-declined");
    let mut contained_only = node.open();
    let refused = contained_only
        .create_and_start(&host(&node, "h0"))
        .expect_err("not allowed")
        .to_string();
    assert_eq!(
        refused,
        "a host job is refused: this agent was started without --allow-host-jobs"
    );
    drop(contained_only);
    let mut runtime = open_allowing(&node);
    let mut refuse = |run: &str, mutate: &dyn Fn(&mut RunSpec)| {
        let mut spec = host(&node, run);
        mutate(&mut spec);
        runtime.create_and_start(&spec).expect_err(run).to_string()
    };
    let mount = |s: &mut RunSpec| {
        s.mounts = vec![js_agent::lifecycle::ResolvedMount {
            target: "/data".to_owned(),
            host: s.scratch.clone(),
            ro: true,
        }];
    };
    let no_mount = "a host job is given no mount and no run files: it sees the host's own paths";
    assert_eq!(refuse("h1", &mount), no_mount);
    assert_eq!(
        refuse("h2", &|s| s.files = Some(s.scratch.clone())),
        no_mount
    );
    assert_eq!(
        refuse("h3", &|s| s.gpu_indices = vec![0]),
        "a host job books no GPU: it sees every device of the host"
    );
    assert!(runtime.list_labelled().expect("lists").is_empty());
    // No lease of any name: no `js-` directory in the store, and no cgroup in `actions` but
    // the helpers'.
    let names = |dir: &std::path::Path| -> Vec<String> {
        let entries = std::fs::read_dir(dir).expect("a directory");
        let names = entries.map(|entry| entry.expect("an entry").file_name());
        names
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    };
    let leases: Vec<String> = names(&node.scratch)
        .into_iter()
        .filter(|name| name.starts_with("js-"))
        .collect();
    assert_eq!(leases, Vec::<String>::new());
    let cgroups: Vec<String> = names(&node.cgroups.join("actions"))
        .into_iter()
        .filter(|name| node.cgroups.join("actions").join(name).is_dir())
        .collect();
    assert_eq!(cgroups, ["helpers"]);
}

/// Proves a host job that asks for a network is made on an agent whose ceiling is none, and
/// is not wired: it has the host's network, and a contained job asking the same is refused.
/// Catches a host job refused for a ceiling that does not apply to it, and one the runtime
/// tries to wire into a namespace it does not have.
#[test]
fn a_host_job_has_the_hosts_network_whatever_the_ceiling() {
    let node = Node::new("host-network");
    let mut runtime = open_allowing(&node);
    let open = |mut spec: RunSpec| {
        spec.network = JobNetwork::Open;
        spec
    };
    let contained = runtime.create_and_start(&open(node.spec("c1-exit")));
    let why = contained.expect_err("the ceiling is none").to_string();
    assert!(why.contains("ceiling is none"), "{why}");
    let id = runtime
        .create_and_start(&open(host(&node, "h1-exit")))
        .expect("started");
    runtime.wait_ended(&id);
    assert_eq!(runtime.poll(&id), Ok(exited(3)));
    assert_eq!(runtime.remove(&id), Ok(()));
}

/// Proves a host job is stopped as a contained one is: a stop sends SIGTERM and a job that
/// handles it ends with its own status; one that ignores it is ended through the lease's
/// `cgroup.kill` once the grace is spent (137); and removing one that still runs (a cancel)
/// kills it first and leaves nothing of the lease. Catches a host job that outlives its stop
/// or its cancel because it shares the host's pid namespace.
#[test]
fn a_host_job_is_stopped_killed_and_removed_through_its_lease() {
    let node = Node::new("host-stop");
    let mut runtime = open_allowing(&node);
    let term = started_and_up(&node, &mut runtime, "r1-hold");
    assert_eq!(runtime.stop(&term, LONG), Ok(()));
    assert_eq!(runtime.poll(&term), Ok(exited(143)));
    assert_eq!(runtime.remove(&term), Ok(()));

    let stubborn = started_and_up(&node, &mut runtime, "r2-stubborn");
    assert_eq!(runtime.stop(&stubborn, Duration::ZERO), Ok(()));
    assert_eq!(runtime.poll(&stubborn), Ok(exited(137)));
    assert_eq!(runtime.remove(&stubborn), Ok(()));

    let cancelled = started_and_up(&node, &mut runtime, "r3-stubborn");
    assert_eq!(runtime.remove(&cancelled), Ok(()));
    assert_eq!(runtime.poll(&cancelled), Ok(Status::Gone));
    assert!(!node.lease_of(&cancelled.0).exists());
    assert!(!node.cgroup_of(&cancelled.0).exists());
}
