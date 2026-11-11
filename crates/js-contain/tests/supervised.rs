//! `Oci::start_supervised` end to end: the daemon's binary in its supervise mode (the
//! helper binary, `tests/bin/helper.rs`, which answers `__supervise-run`) runs the
//! stand-in `crun` (`tests/bin/fake_crun.rs`) over a lease, and the handle reads how the
//! run ended from the exit record. Whether a real `crun` honours the bundle is not
//! something these tests can say.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use js_contain::container::{ContainerLimits, ContainerSpec, Ended, HostMount};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{
    Oci, OciConfig, RunEnd, exit_record_path, read_exit_record, recorded_supervisor, stderr_path,
    stdout_path, supervisor_path,
};
use js_contain::subids::IdFiles;
use js_testkit::spawn::install_copy;

/// A source nothing is fetched from: these tests never name an image.
#[derive(Debug)]
struct NoImages;

#[async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed("no registry in this test".to_owned()))
    }
}

/// A scratch directory of one test, with a copy of the stand-in runtime in it.
struct Node {
    dir: PathBuf,
    crun: PathBuf,
}

impl Node {
    /// A fresh directory for `name`, whose runtime runs `script`.
    fn new(name: &str, script: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("supervised-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let crun = dir.join("crun");
        // Made by a process of its own: see `install_copy`.
        install_copy(Path::new(env!("CARGO_BIN_EXE_js-fake-crun")), &crun, "755");
        std::fs::write(crun.with_extension("script"), script).expect("script");
        Self { dir, crun }
    }

    /// The backend, whose daemon is the helper binary and whose runtime is the stand-in,
    /// with ids and id tools made under the directory.
    fn oci(&self) -> Arc<Oci> {
        let files = IdFiles::in_dir(&self.dir.join("ids"));
        std::fs::create_dir_all(self.dir.join("ids")).expect("ids");
        std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
        let uid = rustix::process::getuid().as_raw();
        std::fs::write(&files.subuid, format!("{uid}:100000:65536\n")).expect("subuid");
        std::fs::write(&files.subgid, format!("{uid}:200000:65536\n")).expect("subgid");
        let tools = self.dir.join("tools");
        std::fs::create_dir_all(&tools).expect("tools");
        for name in ["newuidmap", "newgidmap"] {
            let path = tools.join(name);
            std::fs::write(&path, b"#!/bin/true\n").expect("tool");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755))
                .expect("chmod");
        }
        let config = OciConfig {
            runtime: self.crun.clone(),
            daemon: PathBuf::from(env!("CARGO_BIN_EXE_js-helper")),
            id_files: files,
            check_search: false,
            id_tool_path: Some(tools.into_os_string()),
            ..OciConfig::default()
        };
        Oci::new(
            &config,
            Arc::new(NoImages),
            &self.dir.join("scratch"),
            "/js.slice/actions",
        )
        .expect("starts")
    }

    /// Every line the stand-in recorded: one call each, its arguments joined by spaces.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.crun.with_extension("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The container `lease-1-2` of this node.
    fn spec(&self) -> ContainerSpec {
        self.spec_of("lease-1-2")
    }

    /// The container `name` of this node, in a lease directory of its own.
    fn spec_of(&self, name: &str) -> ContainerSpec {
        let lease = self.dir.join("scratch").join(name);
        ContainerSpec {
            name: name.to_owned(),
            image: format!("registry.example/library/tool@sha256:{}", "a".repeat(64)),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            input_root: lease.join("root"),
            upper: lease.join("upper"),
            overlay_work: lease.join("work"),
            dir: lease,
            cgroup_parent: format!("/js.slice/actions/{name}"),
            isolation: js_contain::Isolation::Contained,
            exec_root: "/job/root".to_owned(),
            working_directory: String::new(),
            env: Vec::new(),
            argv: vec!["tool".to_owned()],
            limits: ContainerLimits::DEFAULT,
            shared: Vec::new(),
            mounts: [
                ("/scratch", false),
                ("/workspace", false),
                ("/run/js-files", true),
            ]
            .map(|(target, read_only)| HostMount {
                target: target.to_owned(),
                source: PathBuf::from("/srv/host-mount"),
                read_only,
                recursive: true,
                no_exec: false,
            })
            .to_vec(),
            devices: Vec::new(),
            mounted_root: None,
        }
    }
}

/// Catches a supervised run whose end or output does not reach the handle: the stand-in
/// `crun` is run over the lease's bundle by the supervisor, the handle sees the exit code,
/// both streams' tails with their counts, and the time the record was written, the
/// output is also in the lease's file, and the end reads as the container's code.
#[tokio::test]
async fn a_supervised_container_ends_with_its_code_and_output_in_its_record() {
    let node = Node::new("ends", "run-exit 3\nrun-out hello\nrun-err oops");
    let (oci, spec) = (node.oci(), node.spec());
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    assert_eq!(run.lease(), spec.dir);
    // The lease's directory names its supervisor, for a teardown that lost the ledger.
    assert_eq!(recorded_supervisor(&spec.dir), Some(run.id()));
    run.wait().await;
    assert_eq!(run.run_end(), RunEnd::Exited(3));
    let tail = run.tail();
    assert_eq!(tail.stdout.bytes, b"hello\n");
    assert_eq!(tail.stdout.total, 6);
    assert_eq!(tail.stderr.bytes, b"oops\n");
    assert_eq!(tail.stderr.total, 5);
    assert!(!tail.stdout.file_cut);
    assert!(run.finished_ms().is_some());
    let file = std::fs::read(stdout_path(&spec.dir)).expect("file");
    assert_eq!(file, b"hello\n");
    let ended = oci.ended_as(&spec, run.run_end()).await;
    assert_eq!(ended, Ok(Ended::Exited(3)));
    let lease = spec.dir.display();
    let want = format!("run --bundle {lease}/bundle lease-1-2");
    let calls = node.calls();
    assert!(calls.iter().any(|c| c.ends_with(&want)), "{calls:?}");
}

/// Catches a second start over a lease that was used: with the record there it is refused
/// before anything runs; with the record gone but the lease's capture files left, the
/// supervisor cannot make its files and never says it is ready, so the start fails.
#[tokio::test]
async fn a_lease_is_started_once() {
    let node = Node::new("twice", "");
    let (oci, spec) = (node.oci(), node.spec());
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    run.wait().await;

    let why = oci
        .start_supervised(&spec, None)
        .expect_err("the record is there");
    assert!(why.contains("an exit record is already there"), "{why}");
    std::fs::remove_file(exit_record_path(&spec.dir)).expect("remove the record");
    let why = oci
        .start_supervised(&spec, None)
        .expect_err("the files are there");
    assert!(why.contains("did not say it is ready"), "{why}");
    assert_eq!(read_exit_record(&spec.dir), Ok(None));
}

/// Catches a handle that cannot be rebuilt by a later process: the run adopted by its
/// supervisor's id reads the same end and tail from the same record; adopted over a lease
/// that has no record, the supervisor being gone, the run failed.
#[tokio::test]
async fn a_run_is_adopted_by_its_supervisor_id() {
    let node = Node::new("adopt", "run-exit 4\nrun-out kept");
    let (oci, spec) = (node.oci(), node.spec());
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    run.wait().await;

    let adopted = oci.adopt_supervised(&spec.dir, run.id());
    assert_eq!(adopted.id(), run.id());
    assert_eq!(adopted.lease(), spec.dir);
    assert_eq!(adopted.run_end(), RunEnd::Exited(4));
    assert_eq!(adopted.tail(), run.tail());
    assert_eq!(adopted.finished_ms(), run.finished_ms());

    let mut orphan = oci.adopt_supervised(&node.dir.join("no-lease"), run.id());
    orphan.wait().await;
    assert_eq!(
        orphan.run_end(),
        RunEnd::Failed("the supervisor ended without an exit record".to_owned())
    );
}

/// Catches `crun`'s words about a container that did not run read as its exit code: a
/// supervised run whose `crun` logged an error is a container that did not run, and a run
/// that failed is the error it failed with.
#[tokio::test]
async fn the_words_of_crun_are_read_for_a_supervised_run() {
    let script = "run-exit 1\nrun-log {\"level\":\"error\",\"msg\":\"cgroup: no\"}";
    let node = Node::new("not-run", script);
    let (oci, spec) = (node.oci(), node.spec());
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    run.wait().await;
    assert_eq!(run.run_end(), RunEnd::Exited(1));
    assert_eq!(
        oci.ended_as(&spec, run.run_end()).await,
        Ok(Ended::NotRun("exited 1".to_owned()))
    );
    let failed = RunEnd::Failed("it broke".to_owned());
    let ended = oci.ended_as(&spec, failed).await;
    assert_eq!(ended, Err("it broke".to_owned()));
}

/// Proves the file that names a lease's supervisor is written as a new file, so a link put in
/// its place (the lease's directory is the job user's) is removed, never written through: the
/// file the link pointed at keeps what it held, and the lease names the started supervisor.
/// Catches a root agent that writes through a link the job user made.
#[tokio::test]
async fn the_supervisor_file_is_made_new_and_never_written_through_a_link() {
    let node = Node::new("no-link", "run-exit 0");
    let (oci, spec) = (node.oci(), node.spec());
    oci.create(&spec).await.expect("bundle");
    let target = node.dir.join("elsewhere");
    std::fs::write(&target, "kept").expect("target");
    std::os::unix::fs::symlink(&target, supervisor_path(&spec.dir)).expect("link");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    assert_eq!(std::fs::read_to_string(&target).expect("target"), "kept");
    assert_eq!(recorded_supervisor(&spec.dir), Some(run.id()));
    run.wait().await;
}

/// Catches a supervisor started with the runtime's file cap in place of the run's: the
/// supervisor of a run given a cap of 5 bytes keeps 5 bytes of each stream in the lease's
/// files and records that they were cut, and the supervisor of a run given none keeps the
/// runtime's (1 MiB, so all 17 bytes of each), while both tails hold the whole of what was
/// printed.
#[tokio::test]
async fn the_supervisor_is_given_the_runs_cap() {
    let node = Node::new("cap", "run-out 0123456789abcdef\nrun-err fedcba9876543210");
    let oci = node.oci();
    let read = |path: PathBuf| std::fs::read_to_string(path).expect("file");

    let spec = node.spec_of("lease-1-5");
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, Some(5)).expect("started");
    run.wait().await;
    assert_eq!(run.run_end(), RunEnd::Exited(0));
    assert_eq!(read(stdout_path(&spec.dir)), "01234");
    assert_eq!(read(stderr_path(&spec.dir)), "fedcb");
    let tail = run.tail();
    assert!(tail.stdout.file_cut && tail.stderr.file_cut);
    assert_eq!(tail.stdout.bytes, b"0123456789abcdef\n");
    assert_eq!((tail.stdout.total, tail.stderr.total), (17, 17));

    let spec = node.spec_of("lease-1-6");
    oci.create(&spec).await.expect("bundle");
    let mut run = oci.start_supervised(&spec, None).expect("started");
    run.wait().await;
    assert_eq!(read(stdout_path(&spec.dir)), "0123456789abcdef\n");
    assert_eq!(read(stderr_path(&spec.dir)), "fedcba9876543210\n");
    let tail = run.tail();
    assert!(!tail.stdout.file_cut && !tail.stderr.file_cut);
}
