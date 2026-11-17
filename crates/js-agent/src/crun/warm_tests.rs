//! The pure parts of the warm sandboxes (the module `warm`): how many an image gets, which
//! runs one can carry, what is bound into it and when it is up.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_agent_wire::{HostPort, Isolation};
use js_contain::container::HostMount;
use js_testkit::dir::TestDir;
use js_types::JobNetwork;
use js_types::PortMap;

use super::spec::container_spec;
use super::warm::{KEEP_MS, Pool, RECENT_MS, binds_of, fits, init_of};
use crate::lifecycle::{Body, RunSpec};

/// A pool of at most `per_image` sandboxes an image.
fn pool(per_image: u32) -> Pool {
    Pool::new(per_image, Arc::new(js_contain::job_fs::Kernel), 7)
}

/// A contained run of an image that a sandbox can carry.
fn run() -> RunSpec {
    RunSpec {
        run_id: "r1".to_owned(),
        body: Body::Container {
            image: "registry.example/job@sha256:00".to_owned(),
            args: Vec::new(),
        },
        env: Vec::new(),
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        pids: None,
        gpu_indices: Vec::new(),
        mounts: Vec::new(),
        scratch: PathBuf::from("/scratch/r1"),
        files: None,
        slot: 0,
        isolation: Isolation::Contained,
        network: JobNetwork::None,
        ports: Vec::new(),
        host_ports: Vec::new(),
        labels: std::collections::BTreeMap::new(),
        bundle: None,
        log_sink: None,
    }
}

/// Proves how many sandboxes an image is to have: none before any start and once its last
/// start is [`KEEP_MS`] old; one for a start older than [`RECENT_MS`] but kept; the starts of
/// the last [`RECENT_MS`] otherwise, at most the pool's bound; and starts older than the window
/// are forgotten as new ones come. Catches a pool that keeps sandboxes for an image nobody
/// runs, or grows past its bound.
#[test]
fn an_image_gets_as_many_sandboxes_as_it_started_recently_within_the_bound() {
    let mut pool = pool(2);
    assert_eq!(pool.target("a", 0), 0);
    pool.note_start("a", 1_000);
    assert_eq!(pool.target("a", 1_000), 1);
    pool.note_start("a", 1_001);
    pool.note_start("a", 1_002);
    assert_eq!(pool.target("a", 1_002), 2, "three starts, bound two");
    assert_eq!(
        pool.target("a", 1_000 + RECENT_MS + 5),
        1,
        "kept, none recent"
    );
    assert_eq!(pool.target("a", 1_002 + KEEP_MS), 0, "all too old");
    assert_eq!(pool.target("b", 1_000), 0, "another image");
    pool.note_start("a", 1_000 + KEEP_MS + 10);
    assert_eq!(
        pool.target("a", 1_000 + KEEP_MS + 10),
        1,
        "the old starts are gone"
    );
    assert!(pool.idle_handles().is_empty());
    assert!(!pool.started_warm("x"));
    pool.forget("x");
}

/// Proves which runs a held container can carry: a contained run of an image with no
/// network, GPU, port or host port; and not a host run, a native body, a run with no image,
/// or one asking for any of those. Catches a sandbox given a run whose container would differ
/// from it in what a bind cannot change.
#[test]
fn only_a_plain_contained_run_of_an_image_fits_a_sandbox() {
    assert_eq!(
        fits(&run()),
        Some(("registry.example/job@sha256:00", &[][..]))
    );
    let mut cases: Vec<RunSpec> = Vec::new();
    let mut host = run();
    host.isolation = Isolation::Host;
    cases.push(host);
    let mut wired = run();
    wired.network = JobNetwork::AllowList;
    cases.push(wired);
    let mut gpu = run();
    gpu.gpu_indices = vec![0];
    cases.push(gpu);
    let mut ported = run();
    ported.ports = vec![PortMap {
        host: 8080,
        container: 80,
    }];
    cases.push(ported);
    let mut host_port = run();
    host_port.host_ports =
        vec![serde_json::from_str::<HostPort>(r#"{"host_port":5000}"#).expect("a host port")];
    cases.push(host_port);
    let mut bare = run();
    bare.body = Body::Container {
        image: String::new(),
        args: Vec::new(),
    };
    cases.push(bare);
    let mut native = run();
    native.body =
        Body::Native(serde_json::from_str(r#"{"program":"/bin/true"}"#).expect("a native body"));
    cases.push(native);
    for (n, case) in cases.iter().enumerate() {
        assert_eq!(fits(case), None, "case {n}");
    }
}

/// Proves the binds of a run in a sandbox: `/work` from the run lease's upper directory and
/// the home first, then the container's own mounts in order; a mount the run names at
/// `/work` takes the upper directory's place, once. Catches a run whose `/work` is the
/// sandbox's own, or whose copy-on-write `/work` is bound twice.
#[test]
fn a_runs_work_and_home_come_first_then_its_own_mounts() {
    let image =
        super::spec::pinned(&format!("r.example/a@sha256:{}", "a".repeat(64))).expect("pinned");
    let lease = js_contain::oci::LeaseDirs {
        dir: PathBuf::from("/l"),
        root: PathBuf::from("/l/root"),
        upper: PathBuf::from("/l/upper"),
        work: PathBuf::from("/l/work"),
        home: PathBuf::from("/l/rw/home"),
    };
    let container = container_spec("w", &run(), &image, &lease, "/actions/w");
    let targets = |binds: Vec<HostMount>| -> Vec<(String, PathBuf)> {
        binds.into_iter().map(|b| (b.target, b.source)).collect()
    };
    let (upper, home) = (Path::new("/r/upper"), Path::new("/r/home"));
    assert_eq!(
        targets(binds_of(&container, (upper, home))),
        [
            ("/work".to_owned(), upper.to_owned()),
            ("/root".to_owned(), home.to_owned()),
            ("/scratch".to_owned(), PathBuf::from("/scratch/r1")),
        ]
    );
    let mut cow = container.clone();
    cow.mounts.push(HostMount {
        target: "/work".to_owned(),
        source: PathBuf::from("/m/cow"),
        read_only: false,
        recursive: true,
        no_exec: false,
    });
    assert_eq!(
        targets(binds_of(&cow, (upper, home))),
        [
            ("/work".to_owned(), PathBuf::from("/m/cow")),
            ("/root".to_owned(), home.to_owned()),
            ("/scratch".to_owned(), PathBuf::from("/scratch/r1")),
        ]
    );
}

/// Proves a sandbox is up when its container's group holds exactly one process, its init:
/// not with none, two, a word that is not a pid, or no group to read. Catches a run handed
/// to a container that `crun` is still setting up.
#[test]
fn a_sandbox_is_up_when_its_init_is_alone_in_its_group() {
    let dir = TestDir::under(std::env::temp_dir(), "warm-init-of");
    assert_eq!(init_of(&dir), None, "no group");
    std::fs::create_dir_all(dir.join("ctr")).expect("ctr");
    for (text, want) in [
        ("42\n", Some(42)),
        ("", None),
        ("42\n43\n", None),
        ("x\n", None),
    ] {
        std::fs::write(dir.join("ctr/cgroup.procs"), text).expect("procs");
        assert_eq!(init_of(&dir), want, "{text:?}");
    }
}
