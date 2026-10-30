//! The OCI backend against a stand-in `crun` (`tests/bin/fake_crun.rs`) that runs nothing:
//! what the backend asks of the runtime, in what order, and how it reads the answer.
//! Whether a real `crun` honours the bundle is not something these tests can say.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use js_contain::container::{CONTAINER_OWNER, ContainerLimits, ContainerSpec, Dirs, Ended};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{LogLimits, Oci, OciConfig, stderr_path, stdout_path};
use js_contain::subids::IdFiles;

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
            .join(format!("oci-runner-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let crun = dir.join("crun");
        std::fs::copy(env!("CARGO_BIN_EXE_js-fake-crun"), &crun).expect("copy the runtime");
        std::fs::write(crun.with_extension("script"), script).expect("script");
        Self { dir, crun }
    }

    /// A configuration that runs the stand-in, with ids and id tools made under the
    /// directory.
    fn config(&self) -> OciConfig {
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
        OciConfig {
            runtime: self.crun.clone(),
            daemon: std::env::current_exe().expect("test binary"),
            id_files: files,
            check_search: false,
            id_tool_path: Some(tools.into_os_string()),
            ..OciConfig::default()
        }
    }

    /// The backend, started on this node's scratch directory.
    fn oci(&self) -> Arc<Oci> {
        Oci::new(
            &self.config(),
            Arc::new(NoImages),
            &self.dir.join("scratch"),
            "/js.slice/actions",
        )
        .expect("starts")
    }

    /// The backend with `logs` as the limits on a job's output.
    fn oci_keeping(&self, logs: LogLimits) -> Arc<Oci> {
        let config = OciConfig {
            logs,
            ..self.config()
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
        let lease = self.dir.join("scratch/lease-1-2");
        ContainerSpec {
            name: "lease-1-2".to_owned(),
            image: format!("registry.example/library/tool@sha256:{}", "a".repeat(64)),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            input_root: lease.join("root"),
            upper: lease.join("upper"),
            overlay_work: lease.join("work"),
            dir: lease,
            cgroup_parent: "/js.slice/actions/lease-1-2".to_owned(),
            exec_root: "/job/root".to_owned(),
            working_directory: String::new(),
            env: Vec::new(),
            argv: vec!["tool".to_owned()],
            limits: ContainerLimits::DEFAULT,
            shared: Vec::new(),
        }
    }
}

/// Runs the container of `spec` to its end and says how it ended.
async fn run(oci: &Oci, spec: &ContainerSpec) -> Result<Ended, String> {
    oci.create(spec).await.expect("bundle");
    let mut running = oci.start(spec).expect("started");
    running.wait().await;
    oci.ended(spec, &running).await
}

/// Catches the default configuration drifting from what a node installs: the system's
/// `crun`, the id files and a search check that is on.
#[test]
fn the_default_configuration_is_the_systems() {
    let config = OciConfig::default();
    assert_eq!(config.runtime, Path::new("/usr/bin/crun"));
    assert!(config.check_search);
    assert_eq!(config.id_files, IdFiles::system());
    assert_eq!(config.id_tool_path, std::env::var_os("PATH"));
}

/// Catches a container run with the wrong command line (`crun` must be given the lease's
/// own state directory, its log and its bundle, and the container's name) and an exit
/// status read as anything but the container's: 0 and any other code, when neither
/// `crun`'s log nor the init's report has a word to say.
#[tokio::test]
async fn a_container_that_exits_ends_with_its_code() {
    for (script, want) in [("", Ended::Exited(0)), ("run-exit 3", Ended::Exited(3))] {
        let node = Node::new("exit", script);
        let spec = node.spec();
        assert_eq!(run(&node.oci(), &spec).await, Ok(want));
        let calls = node.calls();
        let lease = spec.dir.display();
        let want = format!(
            "--root {lease}/crun --log {lease}/crun.log --log-format json run --bundle {lease}/bundle lease-1-2"
        );
        assert!(
            calls.iter().any(|c| c.contains(&want)),
            "{want:?} not in {calls:?}"
        );
    }
}

/// Catches a non-zero exit taken for the job's when `crun` itself said it failed (an
/// error record in its log) or the init reported that it could not start the program:
/// each is a container that did not run, however it exited.
#[tokio::test]
async fn a_container_that_did_not_run_is_told_by_the_words_of_crun_and_the_init() {
    let node = Node::new(
        "notrun",
        "run-exit 1\nrun-log {\"level\":\"error\",\"msg\":\"cgroup: no\"}",
    );
    let spec = node.spec();
    let oci = node.oci();
    assert_eq!(
        run(&oci, &spec).await,
        Ok(Ended::NotRun("exited 1".to_owned()))
    );
    let reports = oci.reports(&spec);
    assert_eq!(reports.log, spec.dir.join("crun.log"));
    assert_eq!(reports.init, spec.dir.join("init-report"));
    let words = std::fs::read_to_string(&reports.log).expect("log");
    assert!(words.contains("cgroup: no"), "{words}");

    let node = Node::new("notrun-init", "run-exit 126");
    let spec = node.spec();
    let oci = node.oci();
    oci.create(&spec).await.expect("bundle");
    let mut running = oci.start(&spec).expect("started");
    running.wait().await;
    // The init writes its report to the file `start` made, once the container is going.
    std::fs::write(
        &oci.reports(&spec).init,
        "open executable: Permission denied\n",
    )
    .expect("report");
    assert_eq!(
        oci.ended(&spec, &running).await,
        Ok(Ended::NotRun("exited 126".to_owned()))
    );
}

/// Catches a `crun` that cannot be run taken for a container that exited, or a helper or
/// blocking call that hides it: the runtime that vanished, and one that is no longer
/// executable, are each an error with the runtime's name or the system's reason, on every
/// path that starts it (the container's run, a helper, and the blocking calls).
#[tokio::test]
async fn a_runtime_that_cannot_be_run_is_an_error_not_an_exit() {
    let node = Node::new("gone", "");
    let spec = node.spec();
    let oci = node.oci();
    std::fs::remove_file(&node.crun).expect("remove the runtime");
    let why = run(&oci, &spec).await.expect_err("no runtime");
    assert!(why.contains(&node.crun.display().to_string()), "{why}");
    let helper = oci
        .unpack_layers(&node.dir, &node.dir, &[])
        .await
        .expect_err("helper");
    assert!(
        helper
            .to_string()
            .contains(&node.crun.display().to_string()),
        "{helper}"
    );
    let killed = oci.terminate("lease-1-2").await.expect_err("kill");
    assert!(
        killed.contains(&node.crun.display().to_string()),
        "{killed}"
    );

    let node = Node::new("not-executable", "");
    let spec = node.spec();
    let oci = node.oci();
    std::fs::set_permissions(&node.crun, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let why = run(&oci, &spec).await.expect_err("not executable");
    assert!(why.starts_with("crun run: "), "{why}");
    let helper = oci
        .unpack_layers(&node.dir, &node.dir, &[])
        .await
        .expect_err("helper")
        .to_string();
    assert!(helper.starts_with("crun run (helper "), "{helper}");
    let blocking = oci
        .remove_dir(&node.dir.join("scratch"))
        .expect_err("helper");
    assert!(
        blocking.starts_with(&format!("run {}: ", node.crun.display())),
        "{blocking}"
    );
}

/// Catches a stop that sends the wrong signal or to the wrong container, a removal that
/// does not ask `crun` to delete by force, and a failure of either that is not reported
/// with `crun`'s words.
#[tokio::test]
async fn a_container_is_signalled_and_removed_by_name() {
    let node = Node::new("signal", "");
    let oci = node.oci();
    oci.terminate("lease-1-2").await.expect("kill");
    oci.remove("lease-1-2").expect("delete");
    let calls = node.calls();
    assert!(
        calls.iter().any(|c| c.ends_with("kill lease-1-2 15")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("delete --force lease-1-2")),
        "{calls:?}"
    );

    let node = Node::new(
        "signal-fail",
        "kill-fail no such container\ndelete-fail busy",
    );
    let oci = node.oci();
    let why = oci.terminate("lease-1-2").await.expect_err("kill fails");
    assert!(why.starts_with("crun kill: "), "{why}");
    // The state directory is not there, so a failed delete is the race it is taken for.
    oci.remove("lease-1-2").expect("nothing left to delete");
}

/// Catches the lease's directories handed over without being opened first (the
/// container's ids are others of the daemon user's and cannot search them), and a helper
/// failure lost: the helper is one `crun run` of a bundle of its own, and its words come
/// back.
#[tokio::test]
async fn directories_are_opened_and_given_by_a_helper() {
    let node = Node::new("give", "");
    let spec = node.spec();
    let oci = node.oci();
    for dir in [&spec.input_root, &spec.upper, &spec.overlay_work] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    let dirs = |helper| Dirs {
        lease: &spec.dir,
        root: &spec.input_root,
        upper: &spec.upper,
        work: &spec.overlay_work,
        helper,
    };
    oci.give(CONTAINER_OWNER, &dirs(None)).await.expect("given");
    oci.give(CONTAINER_OWNER, &dirs(Some(Path::new("/helper"))))
        .await
        .expect("given, directories only");
    let mode = |p: PathBuf| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(spec.dir.clone()), 0o755);
    for name in ["tmp", "var-tmp", "run", "home"] {
        assert_eq!(mode(spec.dir.join("rw").join(name)), 0o755, "{name}");
    }
    let helpers: Vec<_> = node
        .calls()
        .into_iter()
        .filter(|c| c.contains("run --bundle") && c.contains("/helper-"))
        .collect();
    assert_eq!(helpers.len(), 2, "{helpers:?}");

    let node = Node::new("give-fail", "helper-fail the helper broke");
    let oci = node.oci();
    let spec = node.spec();
    let why = oci
        .give(
            "0:0",
            &Dirs {
                lease: &spec.dir,
                root: &spec.input_root,
                upper: &spec.upper,
                work: &spec.overlay_work,
                helper: None,
            },
        )
        .await
        .expect_err("the helper fails");
    assert!(why.contains("the helper broke"), "{why}");
    assert!(why.contains("crun run (helper "), "{why}");
}

/// Catches a removal that reports success when its helper failed (the directory would be
/// left owned by ids the daemon cannot remove), one that leaves the emptied directory
/// itself behind, and the unpack of layers not running a helper or hiding its failure.
#[tokio::test]
async fn a_directory_is_removed_by_a_helper_and_layers_unpacked_by_one() {
    let node = Node::new("remove", "");
    let oci = node.oci();
    let gone = node.dir.join("to-remove");
    std::fs::create_dir(&gone).expect("dir");
    oci.remove_dir(&gone).expect("blocking");
    assert!(!gone.exists());
    std::fs::create_dir(&gone).expect("dir");
    oci.remove_dir_async(&gone).await.expect("async");
    assert!(!gone.exists());
    let (stage, blobs) = (node.dir.join("stage"), node.dir.join("blobs"));
    oci.unpack_layers(&stage, &blobs, &["0.tar".to_owned()])
        .await
        .expect("unpack");
    let helpers = node
        .calls()
        .into_iter()
        .filter(|c| c.contains("/helper-"))
        .count();
    assert_eq!(helpers, 3);

    let node = Node::new("remove-fail", "helper-fail not removed");
    let oci = node.oci();
    let kept = node.dir.join("kept");
    std::fs::create_dir(&kept).expect("dir");
    for why in [
        oci.remove_dir(&kept).expect_err("blocking").to_string(),
        oci.remove_dir_async(&kept)
            .await
            .expect_err("async")
            .to_string(),
        oci.unpack_layers(&kept, &kept, &[])
            .await
            .expect_err("unpack")
            .to_string(),
    ] {
        assert!(why.contains("not removed"), "{why}");
    }
    assert!(kept.exists());
}

/// Catches a job's output sent anywhere but the two pipes the backend drains (a file the
/// runtime was handed, or one stream's text in the other), and an unterminated or
/// terminated last line lost: each stream's text is in its own file and in the tail, as
/// written.
#[tokio::test]
async fn the_output_of_a_container_is_kept_apart_in_files_and_the_tail() {
    let node = Node::new("out", "run-out to stdout\nrun-err to stderr");
    let spec = node.spec();
    // An hour of grace: a write end the backend kept would make the run wait it out, which
    // the hang guard turns into a failure (the wait is not timed).
    let oci = node.oci_keeping(LogLimits {
        drain_grace: std::time::Duration::from_secs(3600),
        ..LogLimits::default()
    });
    oci.create(&spec).await.expect("bundle");
    let mut running = oci.start(&spec).expect("started");
    tokio::time::timeout(std::time::Duration::from_secs(120), running.wait())
        .await
        .expect("the run ended without waiting out the drain");
    assert_eq!(oci.ended(&spec, &running).await, Ok(Ended::Exited(0)));
    assert_eq!(
        std::fs::read_to_string(stdout_path(&spec.dir)).expect("stdout"),
        "to stdout\n"
    );
    assert_eq!(
        std::fs::read_to_string(stderr_path(&spec.dir)).expect("stderr"),
        "to stderr\n"
    );
    let tail = running.tail();
    assert_eq!(tail.stdout.bytes, b"to stdout\n");
    assert_eq!(tail.stderr.bytes, b"to stderr\n");
    assert_eq!((tail.stdout.total, tail.stderr.total), (10, 10));
}

/// Catches an output that is not bounded on disk, a drain that stops at the cap (the
/// stand-in would block on a full pipe and never exit), and a tail that is not the end
/// of the stream: 1 MiB written through caps of 10 KiB (file) and 1 KiB (tail).
#[tokio::test]
async fn a_container_that_prints_more_than_the_cap_exits_and_leaves_a_bounded_log() {
    const WRITTEN: usize = 1024 * 1024;
    let node = Node::new("flood", &format!("run-flood {WRITTEN}\nrun-exit 4"));
    let spec = node.spec();
    let limits = LogLimits {
        file_cap: 10 * 1024,
        tail: 1024,
        ..LogLimits::default()
    };
    let oci = node.oci_keeping(limits);
    oci.create(&spec).await.expect("bundle");
    let mut running = oci.start(&spec).expect("started");
    running.wait().await;
    assert_eq!(oci.ended(&spec, &running).await, Ok(Ended::Exited(4)));
    let file = std::fs::read(stdout_path(&spec.dir)).expect("stdout");
    assert_eq!(file.len(), 10 * 1024);
    assert!(file.iter().all(|b| *b == b'x'));
    let tail = running.tail();
    assert_eq!(tail.stdout.total, WRITTEN as u64);
    assert!(tail.stdout.file_cut);
    assert_eq!(tail.stdout.bytes.len(), 1024);
    assert_eq!(tail.stdout.bytes.last(), Some(&b'\n'));
    assert_eq!(tail.stderr.total, 0);
    assert!(!tail.stderr.file_cut);
}

/// Catches a start that goes on when the file of a stream cannot be made: the error
/// names the path.
#[tokio::test]
async fn a_log_file_that_cannot_be_made_is_a_start_error_naming_it() {
    let node = Node::new("nofile", "");
    let spec = node.spec();
    let oci = node.oci();
    oci.create(&spec).await.expect("bundle");
    std::fs::create_dir(stdout_path(&spec.dir)).expect("a directory in the way");
    let why = oci.start(&spec).expect_err("no file");
    assert!(
        why.contains(&stdout_path(&spec.dir).display().to_string()),
        "{why}"
    );
}
