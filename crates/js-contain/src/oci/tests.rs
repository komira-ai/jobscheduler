//! Unit tests of the OCI backend's start-up checks, helper jobs and image store sweep:
//! what needs no `crun` (`/usr/bin/true` and `/usr/bin/false` stand in where a program
//! only has to exit 0 or 1). What a lease does with a runtime is `tests/fake_oci.rs`'s.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use serde_json::json;

use super::image::{FetchError, ImageCheckError, ImageSource, PinnedImage};
use super::*;
use crate::subids::IdFiles;

fn scratch(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("oci-{name}"))
}

/// A configuration whose runtime exits `ok` (0 or 1) whatever it is asked, with ids in
/// files under `dir`.
fn config(dir: &Path, ok: bool) -> OciConfig {
    let files = IdFiles::in_dir(&dir.join("ids"));
    std::fs::create_dir_all(dir.join("ids")).expect("ids");
    std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
    let uid = rustix::process::getuid().as_raw();
    std::fs::write(&files.subuid, format!("{uid}:100000:65536\n")).expect("subuid");
    std::fs::write(&files.subgid, format!("{uid}:200000:65536\n")).expect("subgid");
    OciConfig {
        runtime: PathBuf::from(if ok {
            "/usr/bin/true"
        } else {
            "/usr/bin/false"
        }),
        daemon: std::env::current_exe().expect("test binary"),
        id_files: files,
        check_search: false,
        id_tool_path: Some(id_tools(dir, 0o4755, 0o4755).into_os_string()),
        init_dest: init::DEFAULT_DEST.to_owned(),
        helper_modes: helper::HelperModes::default(),
        scrub_env: crun::no_scrub,
        job_user: None,
        logs: LogLimits::default(),
    }
}

/// A directory under `dir` with `newuidmap` and `newgidmap` of the given modes.
fn id_tools(dir: &Path, uid_mode: u32, gid_mode: u32) -> PathBuf {
    let tools = dir.join(format!("tools-{uid_mode:o}-{gid_mode:o}"));
    std::fs::create_dir_all(&tools).expect("tools");
    for (name, mode) in [("newuidmap", uid_mode), ("newgidmap", gid_mode)] {
        let path = tools.join(name);
        std::fs::write(&path, b"#!/bin/true\n").expect("tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
    tools
}

/// A source that fetches nothing: the start-up checks never reach the registry.
#[derive(Debug)]
struct NoImages;

#[async_trait::async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed("no registry in this test".to_owned()))
    }
}

fn start(config: &OciConfig, scratch: &Path) -> Result<Arc<Oci>, OciError> {
    Oci::on(config, Arc::new(NoImages), scratch, "/actions", "x86_64")
}

/// Catches the helpers running the daemon's binary from where it was installed: a helper
/// with a job's ids cannot search a 0750 home directory the binary may sit in, so the
/// daemon copies itself under the scratch directory and runs that copy.
#[test]
fn helpers_run_a_copy_of_the_daemon_under_the_scratch_directory() {
    let dir = scratch("daemon-copy");
    let oci = start(&config(&dir, true), &dir.join("s")).expect("starts");
    assert_eq!(oci.daemon, dir.join("s/oci-bin/daemon"));
    let mode = std::fs::metadata(&oci.daemon)
        .expect("copy")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let original = std::fs::read(std::env::current_exe().expect("test binary")).expect("read");
    assert_eq!(std::fs::read(&oci.daemon).expect("copy"), original);
}

/// Catches an error that does not name the path it was about, and one that is lost.
#[test]
fn an_io_error_names_its_path() {
    let missing = Path::new("/nonexistent-js/a");
    let error = at(missing, std::fs::read(missing)).expect_err("absent");
    let text = String::from(error);
    assert!(text.starts_with("/nonexistent-js/a: "), "{text}");
    assert_eq!(at(missing, Ok(7)).expect("passes"), 7);
    let said = String::from(JobError::from("crun said no".to_owned()));
    assert_eq!(said, "crun said no");
}

/// What starting the backend on `scratch` says, as text: `Ok` for a backend that started.
fn started(config: &OciConfig, scratch: &Path) -> String {
    match start(config, scratch) {
        Ok(_) => "started".to_owned(),
        Err(e) => e.to_string(),
    }
}

/// Catches a backend that starts on a node that cannot run it, and an error that does not
/// say which check failed: an architecture with no seccomp profile, a missing runtime or
/// daemon binary, a scratch directory or helper root that cannot be made, a closed
/// ancestor of the scratch directory, and ids the node does not have.
#[test]
fn the_backend_refuses_a_node_that_cannot_run_it() {
    let dir = scratch("refuses");
    let good = config(&dir, true);
    assert_eq!(started(&good, &dir.join("scratch")), "started");

    let on = Oci::on(
        &good,
        Arc::new(NoImages),
        &dir.join("s1"),
        "/actions",
        "riscv64",
    );
    let refused = on.err().map(|e| e.to_string());
    let says = "this machine's architecture (riscv64) has no seccomp profile";
    assert_eq!(refused.as_deref(), Some(says));

    let mut absent = good.clone();
    absent.runtime = dir.join("absent-crun");
    let said = started(&absent, &dir.join("s2"));
    assert!(
        said.starts_with(&format!("{} is not a file", absent.runtime.display())),
        "{said}"
    );
    let mut absent = good.clone();
    absent.daemon = dir.join("absent-daemon");
    let said = started(&absent, &dir.join("s3"));
    assert!(said.contains("absent-daemon is not a file"), "{said}");

    std::fs::write(dir.join("a-file"), b"x").expect("file");
    let said = started(&good, &dir.join("a-file/scratch"));
    assert!(said.contains("a-file/scratch: "), "{said}");

    let scratch = dir.join("s4");
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::write(scratch.join("oci-helper"), b"in the way").expect("file");
    let said = started(&good, &scratch);
    assert!(said.contains("oci-helper: "), "{said}");

    let closed = dir.join("closed");
    std::fs::create_dir_all(closed.join("scratch")).expect("scratch");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let mut searching = good.clone();
    searching.check_search = true;
    let said = started(&searching, &closed.join("scratch"));
    assert!(
        said.contains("closed is not searchable by others"),
        "{said}"
    );
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    std::fs::write(&good.id_files.subuid, "").expect("empty");
    let said = started(&good, &dir.join("s5"));
    assert!(said.contains("has no range"), "{said}");
}

/// Catches a node whose `crun` could not map the container's ids starting anyway, so
/// that the first job fails instead of the start: each of the two tools missing from
/// `PATH` (and no `PATH` at all), present but neither setuid nor holding file
/// capabilities, or setuid but not executable, is refused naming the tool and the
/// file; a setuid pair is accepted from the second `PATH` directory, and the first
/// directory without the tool is passed over.
#[test]
fn the_backend_refuses_a_node_without_usable_id_tools() {
    let dir = scratch("id-tools");
    let good = config(&dir, true);
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("empty");
    let with_path = |path: Option<std::ffi::OsString>| OciConfig {
        id_tool_path: path,
        ..good.clone()
    };
    let joined = |dirs: &[&Path]| std::env::join_paths(dirs).expect("PATH");
    let said = started(&with_path(None), &dir.join("s1"));
    assert!(said.starts_with("newuidmap: not found on PATH"), "{said}");
    let said = started(&with_path(Some(empty.clone().into())), &dir.join("s2"));
    assert!(said.starts_with("newuidmap: not found on PATH"), "{said}");
    assert!(said.contains("package uidmap"), "{said}");

    let half = id_tools(&dir, 0o4755, 0o4755);
    std::fs::remove_file(half.join("newgidmap")).expect("remove");
    let said = started(&with_path(Some(half.into())), &dir.join("s3"));
    assert!(said.starts_with("newgidmap: not found on PATH"), "{said}");

    let plain = id_tools(&dir, 0o755, 0o4755);
    let said = started(&with_path(Some(plain.clone().into())), &dir.join("s4"));
    assert!(
        said.starts_with("newuidmap: ")
            && said.contains("is neither setuid nor holds file capabilities"),
        "{said}"
    );
    assert!(
        said.contains(&plain.join("newuidmap").display().to_string()),
        "{said}"
    );

    let closed = id_tools(&dir, 0o4755, 0o4600);
    let said = started(&with_path(Some(closed.into())), &dir.join("s5"));
    assert!(
        said.starts_with("newgidmap: ") && said.contains("is not executable"),
        "{said}"
    );

    let both = joined(&[&empty, &id_tools(&dir, 0o4755, 0o4755)]);
    assert_eq!(started(&with_path(Some(both)), &dir.join("s6")), "started");
}

/// Catches the search check refusing a scratch directory that is fine: one under the
/// system's temporary directory (every ancestor searchable) is accepted.
#[test]
fn a_scratch_directory_others_can_reach_is_accepted() {
    let dir = scratch("searchable");
    let under_tmp = crate::test_dir::TestDir::under(std::env::temp_dir(), "js-oci-unit");
    let mut config = config(&dir, true);
    config.check_search = true;
    let said = started(&config, &under_tmp);
    assert_eq!(said, "started");
}

/// Catches an unpack a killed daemon left being kept (a stage directory the container's
/// ids own, a tar), a sweep that removes the images themselves or things that are not
/// stages, one that hides a failure, and a start that goes on after one.
#[test]
fn the_start_removes_the_unpacks_a_killed_daemon_left() {
    let dir = scratch("sweep");
    let scratch = dir.join("scratch");
    let images = scratch.join("oci-images");
    // Nothing yet: the images directory does not exist.
    assert_eq!(started(&config(&dir, true), &scratch), "started");
    std::fs::create_dir_all(images.join(".stage-aa-1-0")).expect("stage");
    std::fs::write(images.join(".stage-aa-1-0.tar"), b"tar").expect("tar");
    std::fs::create_dir_all(images.join("bb/rootfs")).expect("an image");
    std::fs::write(images.join("note"), b"x").expect("a file");
    // `true` stands in for the helper that deletes what is in the stage.
    assert_eq!(started(&config(&dir, true), &scratch), "started");
    assert!(!images.join(".stage-aa-1-0").exists());
    assert!(!images.join(".stage-aa-1-0.tar").exists());
    assert!(images.join("bb/rootfs").is_dir());
    assert!(images.join("note").is_file());

    std::fs::create_dir_all(images.join(".stage-cc-1-1")).expect("stage");
    let said = started(&config(&dir, false), &scratch);
    assert!(
        said.contains("removing unpacks a previous daemon left: "),
        "{said}"
    );
    assert!(
        images.join(".stage-cc-1-1").exists(),
        "kept when it could not be removed"
    );
}

/// Catches a helper root that is not idempotent (the second lease to prepare it fails on
/// the links the first made), one that cannot say it failed, and links that do not lead
/// into `/usr`.
#[test]
fn the_helper_root_is_made_once_and_again_without_error() {
    let base = scratch("rootfs");
    let dir = base.join("root");
    helper::prepare_rootfs(&dir, "/js/daemon").expect("made");
    helper::prepare_rootfs(&dir, "/js/daemon").expect("made again");
    for (name, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        assert_eq!(
            std::fs::read_link(dir.join(name)).expect(name),
            Path::new(target)
        );
    }
    for point in ["dev", "usr", "etc", "js", "t/0", "t/7"] {
        assert!(dir.join(point).is_dir(), "{point}");
    }
    assert!(dir.join("js/daemon").is_file());
    assert!(dir.join("etc/ld.so.cache").is_file());

    // A root that cannot be written to refuses, and says so.
    let closed = scratch("rootfs-closed");
    std::fs::create_dir_all(closed.join("root/dev")).expect("partly made");
    for point in ["usr", "etc", "js"] {
        std::fs::create_dir_all(closed.join("root").join(point)).expect("dir");
    }
    for i in 0..8 {
        std::fs::create_dir_all(closed.join(format!("root/t/{i}"))).expect("dir");
    }
    for file in ["etc/ld.so.cache", "js/daemon"] {
        std::fs::write(closed.join("root").join(file), b"").expect("file");
    }
    std::fs::set_permissions(closed.join("root"), std::fs::Permissions::from_mode(0o500))
        .expect("chmod");
    assert!(helper::prepare_rootfs(&closed.join("root"), "/js/daemon").is_err());
    std::fs::set_permissions(closed.join("root"), std::fs::Permissions::from_mode(0o700))
        .expect("chmod");
    std::fs::write(closed.join("a-file"), b"x").expect("file");
    assert!(helper::prepare_rootfs(&closed.join("a-file/root"), "/js/daemon").is_err());
}

/// Catches a helper's configuration running a shell, mapping the wrong namespace,
/// binding the daemon without being asked to, or binding a directory read-write that
/// the job does not write.
#[test]
fn a_helper_runs_one_program_in_the_namespace_it_is_given() {
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let root = Path::new("/scratch/oci-helper");
    let render = |mapping: Mapping, job: &helper::Job| -> serde_json::Value {
        let config = helper::config(root, &ids, mapping, "/actions/helper-x", job);
        serde_json::to_value(config).expect("json")
    };
    let modes = helper::HelperModes::default();
    let give = helper::give_job(
        &modes,
        Path::new("/usr/local/bin/daemon"),
        "1:1",
        Some(Path::new("/scratch/l/root")),
        &[Path::new("/scratch/l/upper"), Path::new("/scratch/l/work")],
    );
    assert_eq!(
        give.args,
        [
            "/js/daemon",
            "__give-dirs",
            "1:1",
            "/t/0",
            "--whole",
            "/t/1",
            "/t/2"
        ]
    );
    let config = render(Mapping::Setup, &give);
    assert_eq!(config["process"]["args"], json!(give.args));
    assert_eq!(config["process"]["noNewPrivileges"], true);
    // None of the defaults `oci-spec` fills in: a hostname needs a UTS namespace, masked
    // paths a /proc, resources a cgroup.
    for absent in ["hostname", "annotations"] {
        assert!(config.get(absent).is_none(), "{absent}");
    }
    // Its cgroup is named, under the jobs' parent; crun makes none of its own.
    assert_eq!(config["linux"]["cgroupsPath"], "/actions/helper-x");
    for absent in ["resources", "seccomp"] {
        assert!(config["linux"].get(absent).is_none(), "{absent}");
    }
    // Every capability set is the given one or empty, as `capset` needs.
    let caps = &config["process"]["capabilities"];
    assert_eq!(caps["inheritable"], json!([]));
    assert_eq!(caps["ambient"], json!([]));
    assert_eq!(caps["permitted"].as_array().map(Vec::len), Some(11));
    assert_eq!(config["linux"]["maskedPaths"], json!([]));
    assert_eq!(config["linux"]["readonlyPaths"], json!([]));
    let uids = &config["linux"]["uidMappings"];
    assert_eq!(uids[0], json!({"containerID": 0, "hostID": 990, "size": 1}));
    assert_eq!(
        uids[1],
        json!({"containerID": 1, "hostID": 100_000, "size": 65_536})
    );
    let sources: Vec<_> = config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .map(|m| {
            (
                m["destination"].as_str().unwrap().to_owned(),
                m["options"][1].clone(),
            )
        })
        .collect();
    assert!(
        sources.contains(&("/js/daemon".to_owned(), json!("ro"))),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("/t/2".to_owned(), json!("rw"))),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("/usr".to_owned(), json!("ro"))),
        "{sources:?}"
    );

    // No cache: no directories-only tree; no daemon bind for a job that runs none.
    let whole = helper::give_job(
        &modes,
        Path::new("/d"),
        "0:0",
        None,
        &[Path::new("/scratch/l/root")],
    );
    assert_eq!(whole.args[3], "-");
    let remove = render(
        Mapping::Action,
        &helper::remove_job(Path::new("/scratch/l")),
    );
    assert_eq!(
        remove["linux"]["uidMappings"],
        json!([{"containerID": 0, "hostID": 100_000, "size": 65_536}])
    );
    assert!(
        remove["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["destination"] != "/js/daemon")
    );
    assert_eq!(
        remove["process"]["args"],
        json!(["/usr/bin/find", "/t/0", "-mindepth", "1", "-delete"])
    );
    // Only the daemon's own binary and the node's tools are ever run.
    let programs = ["/js/daemon", "/usr/bin/find"];
    for argv in [&give.args, &whole.args] {
        assert!(programs.contains(&argv[0].as_str()), "{argv:?}");
    }
}

/// Catches the unpack running anything but the daemon's own unpack mode, or
/// a step reading or writing the wrong directory: the stage is writable, the blobs are
/// not, the daemon's binary is bound, and the layers follow in order as names.
#[test]
fn the_unpack_is_one_daemon_mode_over_two_directories() {
    let layers = ["0.tar.gz".to_owned(), "1.tar".to_owned()];
    let job = helper::unpack_job(
        &helper::HelperModes::default(),
        Path::new("/the/daemon"),
        Path::new("/s/stage"),
        Path::new("/s/blobs"),
        &layers,
    );
    assert_eq!(
        job.args,
        [
            "/js/daemon",
            "__unpack-layers",
            "/t/0",
            "/t/1",
            "0.tar.gz",
            "1.tar"
        ]
    );
    assert_eq!(job.daemon.as_deref(), Some(Path::new("/the/daemon")));
    assert_eq!(job.binds[0].source, Path::new("/s/stage"));
    assert!(job.binds[0].writable);
    assert_eq!(job.binds[1].source, Path::new("/s/blobs"));
    assert!(!job.binds[1].writable);
    assert_eq!(job.binds.len(), 2);
}

/// Catches a lease's bundle lacking a file the container's name lookups read, or a file
/// the container's ids cannot read (they are others of the daemon user's). `hosts` names
/// `localhost` for both families; `resolv.conf` has the loopback nameserver nothing
/// listens on, so a name outside `hosts` is refused at once rather than waited for.
#[tokio::test]
async fn the_bundle_has_hosts_hostname_and_resolv_conf_others_can_read() {
    let dir = scratch("bundle");
    let oci = start(&config(&dir, true), &dir.join("scratch")).expect("started");
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("lease");
    oci.create(&spec).await.expect("bundle");
    for (name, content) in [
        ("hosts", "127.0.0.1 localhost\n::1 localhost\n"),
        ("hostname", "localhost\n"),
        (
            "resolv.conf",
            "nameserver 127.0.0.1\noptions timeout:1 attempts:1\n",
        ),
    ] {
        let path = spec.dir.join("etc").join(name);
        assert_eq!(
            std::fs::read_to_string(&path).expect(name),
            content,
            "{name}"
        );
        let mode = std::fs::metadata(&path).expect(name).permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "{name}");
    }
}

/// A source that records how often it was asked and what for, and answers as told.
#[derive(Debug)]
struct Scripted {
    calls: std::sync::atomic::AtomicUsize,
    answer: Result<Vec<String>, FetchError>,
}

impl Scripted {
    fn new(answer: Result<Vec<String>, FetchError>) -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            answer,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ImageSource for Scripted {
    async fn fetch(&self, _: &PinnedImage, blobs: &Path) -> Result<Vec<String>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Yields so that a second caller that is not held to one flight gets in.
        tokio::task::yield_now().await;
        if let Ok(names) = &self.answer {
            for name in names {
                std::fs::write(blobs.join(name), b"layer").expect("blob");
                std::fs::set_permissions(blobs.join(name), std::fs::Permissions::from_mode(0o600))
                    .expect("chmod");
            }
        }
        self.answer.clone()
    }
}

fn pinned() -> PinnedImage {
    PinnedImage {
        reference: format!("registry.example/library/tool@sha256:{}", "a".repeat(64)),
        digest: format!("sha256:{}", "a".repeat(64)),
    }
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Catches an image store that fetches again for every use (two checks racing on one
/// digest must share one fetch, and a check of an image already unpacked asks the source
/// nothing), and one that leaves its blobs or its stage behind or names the finished
/// image by anything but the digest's hex. A runtime that exits 0 stands in for the
/// helper that unpacks.
#[tokio::test]
async fn an_image_is_fetched_once_and_leaves_only_the_finished_directory() {
    let dir = scratch("image-once");
    let source = Scripted::new(Ok(vec!["0.tar".to_owned()]));
    let scratch = dir.join("scratch");
    let oci = Oci::on(
        &config(&dir, true),
        source.clone(),
        &scratch,
        "/actions",
        "x86_64",
    )
    .expect("started");
    let image = pinned();
    let (a, b) = tokio::join!(oci.check_pinned(&image), oci.check_pinned(&image));
    a.expect("first");
    b.expect("second");
    assert_eq!(source.calls(), 1, "two checks raced and fetched twice");
    let images = scratch.join("oci-images");
    let hex = "a".repeat(64);
    assert_eq!(
        names(&images),
        std::slice::from_ref(&hex),
        "stage or blobs left"
    );
    // The stand-in made no root filesystem: with one there, the source is not asked.
    std::fs::create_dir_all(images.join(&hex).join(image::ROOTFS)).expect("rootfs");
    oci.check_pinned(&image).await.expect("unpacked already");
    assert_eq!(source.calls(), 1, "an unpacked image was fetched again");
}

/// Catches a source's failure lost or reworded (a registry's words are the operator's
/// only clue), an index digest taken for a failure (the caller's message is different),
/// and a failed fetch leaving its stage or blobs in the store.
#[tokio::test]
async fn a_failed_fetch_is_reported_as_it_was_and_leaves_nothing() {
    let dir = scratch("image-failed");
    let image = pinned();
    for (answer, want) in [
        (
            FetchError::Failed("registry said no".to_owned()),
            ImageCheckError::Failed("registry said no".to_owned()),
        ),
        (FetchError::Index, ImageCheckError::Index(image.clone())),
    ] {
        let scratch = dir.join("scratch");
        let oci = Oci::on(
            &config(&dir, true),
            Scripted::new(Err(answer)),
            &scratch,
            "/actions",
            "x86_64",
        )
        .expect("started");
        assert_eq!(oci.check_pinned(&image).await, Err(want));
        assert_eq!(names(&scratch.join("oci-images")), Vec::<String>::new());
    }
    let shown = ImageCheckError::Index(image.clone()).to_string();
    assert!(shown.contains(&image.reference), "{shown}");
}

/// Catches a failed unpack reported as success, a fetch's blobs kept after it, and the
/// failure reported without the helper's words. The runtime that exits 1 fails the unpack
/// and the removal of its stage alike; the removal's failure does not hide the unpack's.
#[tokio::test]
async fn a_failed_unpack_is_reported_and_leaves_no_blobs() {
    let dir = scratch("image-unpack-fails");
    let scratch = dir.join("scratch");
    let oci = Oci::on(
        &config(&dir, false),
        Scripted::new(Ok(vec!["0.tar".to_owned()])),
        &scratch,
        "/actions",
        "x86_64",
    )
    .expect("started");
    let image = pinned();
    let Err(ImageCheckError::Failed(why)) = oci.check_pinned(&image).await else {
        panic!("the unpack failed but the check did not");
    };
    assert!(why.contains("crun run (helper helper-unpack-"), "{why}");
    let left = names(&scratch.join("oci-images"));
    assert!(left.iter().all(|n| !n.ends_with("-blobs")), "{left:?}");
    assert!(!left.contains(&"a".repeat(64)), "{left:?}");
}

/// Catches the helper names a caller configures not reaching the helpers: with another
/// binary path and other mode arguments, the job's argv, the bind of the host binary,
/// and the mount points of the helper root carry exactly those and none of the defaults.
#[test]
fn configured_helper_names_are_the_only_names_in_the_helpers() {
    let modes = helper::HelperModes {
        give: "give".to_owned(),
        whole: "--all".to_owned(),
        unpack: "unpack".to_owned(),
        binary: "/other/bin/host".to_owned(),
    };
    let give = helper::give_job(&modes, Path::new("/d"), "1:1", None, &[Path::new("/a")]);
    assert_eq!(
        give.args,
        ["/other/bin/host", "give", "1:1", "-", "--all", "/t/0"]
    );
    let unpack = helper::unpack_job(
        &modes,
        Path::new("/d"),
        Path::new("/s"),
        Path::new("/b"),
        &[],
    );
    assert_eq!(unpack.args[..2], ["/other/bin/host", "unpack"]);
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let config = helper::config(Path::new("/r"), &ids, Mapping::Setup, "/x/h", &give);
    let text = serde_json::to_string(&config).expect("json");
    assert!(
        text.contains(r#""destination":"/other/bin/host""#),
        "{text}"
    );
    assert!(!text.contains("/js/daemon"), "{text}");
    let dir = scratch("modes-root");
    helper::prepare_rootfs(&dir, &modes.binary).expect("made");
    assert!(dir.join("other/bin/host").is_file());
    assert!(!dir.join("js").exists());
}

/// Catches a configured init path not reaching the bundle a lease is made with.
#[tokio::test]
async fn the_bundle_names_the_init_where_the_config_says() {
    let dir = scratch("init-dest");
    let mut config = config(&dir, true);
    config.init_dest = "/dev/other-init".to_owned();
    let oci = start(&config, &dir.join("scratch")).expect("started");
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("lease");
    oci.create(&spec).await.expect("bundle");
    let text = std::fs::read_to_string(spec.dir.join("bundle/config.json")).expect("config");
    assert!(text.contains("/dev/other-init"), "{text}");
    assert!(!text.contains("/dev/js-init"), "{text}");
}

/// A root daemon names the job user: the containers map that user's subordinate range, not
/// the daemon's, and a user with no range is refused. It catches a daemon that maps its own
/// ids when told to run the containers as another user.
#[test]
fn the_backend_maps_the_job_users_range_when_the_daemon_is_not_that_user() {
    let dir = scratch("job-user");
    let mut config = config(&dir, true);
    let own = rustix::process::getuid().as_raw();
    let job = own + 1;
    let ids = IdFiles::in_dir(&dir.join("ids"));
    std::fs::write(
        &ids.passwd,
        format!("daemon:x:{own}:{own}::/:/bin/sh\njob:x:{job}:{job}::/:/bin/sh\n"),
    )
    .expect("passwd");
    std::fs::write(
        &ids.subuid,
        format!("{own}:100000:65536\njob:300000:65536\n"),
    )
    .expect("subuid");
    std::fs::write(
        &ids.subgid,
        format!("{own}:200000:65536\njob:400000:65536\n"),
    )
    .expect("subgid");
    config.job_user = Some((job, job));
    let oci = start(&config, &dir.join("s1")).expect("starts");
    assert_eq!(
        (oci.ids.uid, oci.ids.sub_uid, oci.ids.sub_gid),
        (job, 300_000, 400_000)
    );
    config.job_user = Some((job + 1, job + 1));
    let said = started(&config, &dir.join("s2"));
    assert!(said.contains("has no range"), "{said}");
}

/// Asserts what the egress spike tests rely on: the exposed section is the resolution of the
/// pinned profile for a job with no capability (26 entries on amd64: a change to the profile
/// or to the resolution moves this number, and the spike with it), carries the default
/// action, and an architecture without a filter is an error naming it.
#[test]
fn the_exposed_seccomp_section_is_the_resolved_pinned_profile() {
    let section = seccomp_section("amd64", &[]).expect("amd64 has a filter");
    assert_eq!(section["syscalls"].as_array().expect("syscalls").len(), 26);
    assert!(section["defaultAction"].is_string(), "{section}");
    let error = seccomp_section("riscv64", &[]).expect_err("no filter");
    assert!(error.contains("riscv64"), "{error}");
}

/// Catches a root daemon starting containers in a range it was never given, and a job
/// user that is root: the refusal is explicit at `Oci::new` (`Some((0, 0))`, `Some((uid,
/// 0))`) and for a root process with no job user (`run_ids` with uid 0, which `Oci::new`
/// reads from `getuid`), and holds when the files give root a range.
#[test]
fn a_root_daemon_or_a_root_job_user_is_refused() {
    let dir = scratch("root-refused");
    let mut root_range = config(&dir, true);
    std::fs::write(
        &root_range.id_files.subuid,
        "root:100000:65536\n0:100000:65536\n",
    )
    .expect("subuid");
    std::fs::write(
        &root_range.id_files.subgid,
        "root:200000:65536\n0:200000:65536\n",
    )
    .expect("subgid");
    let uid = rustix::process::getuid().as_raw();
    for job in [Some((0, 0)), Some((uid.max(1), 0)), Some((0, 1000))] {
        root_range.job_user = job;
        let refused = start(&root_range, &dir.join("s")).expect_err("root job user");
        assert!(
            matches!(
                refused,
                OciError::Subids(crate::subids::SubidError::Root { .. })
            ),
            "{job:?}: {refused:?}"
        );
    }
    // No job user, and the daemon is root: no range of its own is mapped.
    root_range.job_user = None;
    let refused = run_ids(&root_range, 0, 0).expect_err("root daemon");
    assert!(
        matches!(refused, SubidError::Root { uid: 0, gid: 0 }),
        "{refused:?}"
    );
    // The same files serve a user that is not root.
    let ids = run_ids(&root_range, 990, 990);
    assert!(
        matches!(ids, Err(SubidError::Missing { .. })),
        "a non-root user with no entry is refused for that, not as root: {ids:?}"
    );
}

/// Catches a lease whose directories, owners or ids are not what a root daemon needs to
/// make them: the lease directory and the writable directory the job user's, everything
/// the job writes the job's id in the container (the first id of the subordinate range
/// plus the job's), parents before children, all under the lease's own name.
#[test]
fn a_lease_names_its_directories_and_who_owns_them() {
    let dir = scratch("lease-dirs");
    let oci = start(&config(&dir, true), &dir.join("s")).expect("starts");
    let lease = oci.lease_dirs("js-x-r1");
    let base = dir.join("s/js-x-r1");
    assert_eq!(
        lease.to_make(),
        [
            (base.clone(), Owner::Job),
            (base.join("root"), Owner::Container),
            (base.join("upper"), Owner::Container),
            (base.join("work"), Owner::Container),
            (base.join("rw"), Owner::Job),
            (base.join("rw/home"), Owner::Container),
        ]
    );
    assert_eq!(lease.dir, base);
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    assert_eq!(oci.job_ids(), (uid, gid));
    assert_eq!(oci.container_ids(), (101_000, 201_000));
}
