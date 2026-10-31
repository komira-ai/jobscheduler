//! Unit tests of the OCI backend's start-up checks, helper jobs and image store sweep:
//! what needs no `crun` (`/usr/bin/true` and `/usr/bin/false` stand in where a program
//! only has to exit 0 or 1). What a lease does with a runtime is `tests/fake_oci.rs`'s.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use serde_json::json;

use super::image::{FetchError, ImageCheckError, ImageSource, PinnedImage};
use super::*;
use crate::subids::IdFiles;

pub(super) fn scratch(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("oci-{name}"))
}

/// A configuration whose runtime exits `ok` (0 or 1) whatever it is asked, with ids in
/// files under `dir`.
pub(super) fn config(dir: &Path, ok: bool) -> OciConfig {
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
        resolv_conf: RESOLV_CONF.to_owned(),
        exclusive_scratch: false,
        route_wait: None,
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

/// Catches the root a caller looks for (`image::published_root`, which `js-agent image-pull`
/// reports and checks) drifting from where the store keeps an image: both name the same
/// directory, which is absent until the image is unpacked and then is the one `has` finds.
#[test]
fn the_published_root_is_where_the_store_keeps_an_unpacked_image() {
    let dir = scratch("published-root");
    let scratch = dir.join("scratch");
    let oci = start(&config(&dir, true), &scratch).expect("started");
    let image = pinned();
    let root = image::published_root(&scratch, &image.digest);
    assert_eq!(root, oci.images.rootfs_of(&image.digest));
    assert_eq!(
        root,
        scratch
            .join("oci-images")
            .join("a".repeat(64))
            .join("rootfs")
    );
    assert!(!oci.images.has(&image));
    std::fs::create_dir_all(&root).expect("rootfs");
    assert!(oci.images.has(&image));
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

/// A runtime that is a script: for every `crun run` it appends to `log` the helper's id,
/// and, for an unpack, the mode and owner of each stage directory in `images` at that
/// moment (what the helper finds when it starts). `fail` is a `case` pattern of helper ids
/// that exit 1.
fn scripted_runtime(dir: &Path, images: &Path, fail: &str) -> PathBuf {
    let path = dir.join("runtime.sh");
    let log = dir.join("runs.log");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do [ \"$prev\" = --bundle ] && bundle=$a; prev=$a; last=$a; done\n\
         stat -c 'dir %a %u' \"$(dirname \"$bundle\")\" >> {log} 2>/dev/null\necho \"$last\" >> {log}\n\
         case \"$last\" in\n  helper-unpack-*) stat -c 'stage %a %u' {images}/.stage-*[0-9] >> {log} \
         2>/dev/null;;\nesac\ncase \"$last\" in\n  {fail}) exit 1;;\nesac\nexit 0\n",
        log = log.display(),
        images = images.display(),
    );
    std::fs::write(&path, script).expect("runtime");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The lines a [`scripted_runtime`] logged.
fn runs(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("runs.log"))
        .expect("the runtime ran")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Catches an unpack stage that anyone but the helper's user can write: when the helper
/// container starts its unpack, the stage directory it was given is not writable by group
/// or others (a local account could then rename the `rootfs` the helper makes and put its
/// own tree in the image's place, or pre-create it).
#[tokio::test]
async fn the_unpack_stage_is_not_group_or_other_writable_when_the_helper_starts() {
    let dir = scratch("image-stage-mode");
    let scratch = dir.join("scratch");
    let runtime = scripted_runtime(&dir, &scratch.join("oci-images"), "never");
    let config = OciConfig {
        runtime,
        ..config(&dir, true)
    };
    let oci = Oci::on(
        &config,
        Scripted::new(Ok(vec!["0.tar".to_owned()])),
        &scratch,
        "/actions",
        "x86_64",
    )
    .expect("started");
    oci.check_pinned(&pinned()).await.expect("unpacked");
    let stages: Vec<String> = runs(&dir)
        .into_iter()
        .filter(|l| l.starts_with("stage "))
        .collect();
    assert_eq!(
        stages.len(),
        1,
        "the unpack helper saw one stage: {stages:?}"
    );
    let mode = u32::from_str_radix(stages[0].split(' ').nth(1).expect("mode"), 8).expect("octal");
    assert_eq!(mode & 0o022, 0, "the stage is writable by others: {mode:o}");
    assert_ne!(mode & 0o200, 0, "its owner can write it: {mode:o}");
}

/// Catches a stage given to the unpack helper by the wrong means for the daemon's user: a
/// rootless daemon cannot `chown` to a subordinate id, so a setup helper container gives
/// it the stage, before the unpack helper starts; a root daemon gives it itself and runs
/// no extra container.
#[tokio::test]
async fn the_stage_is_given_to_the_helper_before_it_unpacks_unless_the_daemon_is_root() {
    let dir = scratch("image-stage-give");
    let scratch = dir.join("scratch");
    let runtime = scripted_runtime(&dir, &scratch.join("oci-images"), "never");
    let config = OciConfig {
        runtime,
        ..config(&dir, true)
    };
    let oci = Oci::on(
        &config,
        Scripted::new(Ok(vec!["0.tar".to_owned()])),
        &scratch,
        "/actions",
        "x86_64",
    )
    .expect("started");
    oci.check_pinned(&pinned()).await.expect("unpacked");
    let helpers: Vec<String> = runs(&dir)
        .iter()
        .filter(|l| l.starts_with("helper-"))
        .map(|l| l.split('-').nth(1).expect("kind").to_owned())
        .collect();
    let want: &[&str] = if rustix::process::geteuid().is_root() {
        &["unpack"]
    } else {
        &["give", "unpack"]
    };
    assert_eq!(helpers, want);
}

/// Catches the owner string drifting from the id the unpack helper writes as: the stage is
/// given to `HELPER_OWNER` in a setup helper's namespace, and the unpack helper's id 0 is
/// the same host id (the first subordinate id).
#[test]
fn the_helper_owner_is_the_host_id_of_the_unpack_helpers_root() {
    let ids = crate::subids::DaemonIds {
        uid: 1000,
        gid: 1001,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let host = |maps: Mappings, id: u32, group: bool| {
        let list = if group { maps.1 } else { maps.0 };
        list.iter()
            .find(|m| id >= m.container_id() && id - m.container_id() < m.size())
            .map(|m| m.host_id() + (id - m.container_id()))
    };
    let (uid, gid) = image::HELPER_OWNER.split_once(':').expect("uid:gid");
    let (uid, gid): (u32, u32) = (uid.parse().expect("uid"), gid.parse().expect("gid"));
    for group in [false, true] {
        let setup = host(ids.setup_mappings(), if group { gid } else { uid }, group);
        let action = host(ids.action_mappings(), 0, group);
        assert_eq!(setup, action, "group: {group}");
        assert_eq!(action, Some(if group { 200_000 } else { 100_000 }));
    }
}

/// Catches a stage made by name instead of by descriptor, or left writable: it is made
/// only if the name is free (a symlink in its place is refused and what it points at is not
/// touched), is `0755` whatever the mode of a directory the daemon makes, and is given
/// through `chown` the directory that was made, to exactly the owner asked, and to nobody
/// when none is.
#[test]
fn a_stage_is_made_by_descriptor_with_the_helpers_owner_and_no_write_for_others() {
    use std::os::fd::AsFd as _;
    let dir = scratch("stage-fd");
    let stage = dir.join("stage");
    let owner = (
        rustix::fs::Uid::from_raw(1234),
        rustix::fs::Gid::from_raw(4321),
    );
    let mut given = Vec::new();
    image::make_stage(&stage, Some(owner), &mut |fd, uid, gid| {
        given.push((
            crate::walk::identity(&fd.as_fd()).expect("identity"),
            uid,
            gid,
        ));
        Ok(())
    })
    .expect("made");
    let meta = std::fs::metadata(&stage).expect("stage");
    assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(given, [((meta.dev(), meta.ino()), owner.0, owner.1)]);

    // The real change of owner, to the ids this process has.
    let mine = (
        rustix::fs::Uid::from_raw(rustix::process::getuid().as_raw()),
        rustix::fs::Gid::from_raw(rustix::process::getgid().as_raw()),
    );
    let own = dir.join("own");
    image::make_stage(&own, Some(mine), &mut image::fchown_fd).expect("made");
    assert_eq!(std::fs::metadata(&own).expect("own").uid(), mine.0.as_raw());

    // No owner, no change of owner.
    let rootless = dir.join("rootless");
    image::make_stage(&rootless, None, &mut |_, _, _| panic!("chown")).expect("made");
    assert!(rootless.is_dir());

    // The name is taken (by a symlink to a directory elsewhere): refused, target untouched.
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&elsewhere, &link).expect("link");
    let refused = image::make_stage(&link, Some(owner), &mut |_, _, _| panic!("chown"));
    assert!(refused.is_err());
    let mode = std::fs::metadata(&elsewhere)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

/// Catches a helper run that skips the preparation of its state directory (the start-up
/// sweep builds its own, apart from the unpack and give helpers): every helper's
/// directory, whether run by the sweep or by an unpack, is `0700` and the running user's
/// when `crun` starts (a root daemon's is the job user's, which `crun` runs as; here it is
/// this user's), so no other account can use it.
#[tokio::test]
async fn every_helpers_state_directory_is_private_to_the_ids_crun_runs_as() {
    let dir = scratch("helper-state-dir");
    let scratch = dir.join("scratch");
    let images = scratch.join("oci-images");
    let runtime = scripted_runtime(&dir, &images, "never");
    let config = OciConfig {
        runtime,
        ..config(&dir, true)
    };
    // A stage a killed daemon left: the next start's sweep runs a helper to remove it.
    std::fs::create_dir_all(images.join(".stage-aa-1-0")).expect("stage");
    let oci = Oci::on(
        &config,
        Scripted::new(Ok(vec!["0.tar".to_owned()])),
        &scratch,
        "/actions",
        "x86_64",
    )
    .expect("started");
    oci.check_pinned(&pinned()).await.expect("unpacked");
    let me = rustix::process::getuid().as_raw().to_string();
    let seen = runs(&dir);
    let dirs: Vec<&String> = seen.iter().filter(|l| l.starts_with("dir ")).collect();
    // The sweep's helper, the give helper (a rootless daemon) or none, the unpack's.
    assert!(dirs.len() >= 2, "{seen:?}");
    for line in dirs {
        let words: Vec<&str> = line.split(' ').collect();
        assert_eq!(words[1], "700", "{line}");
        assert_eq!(words[2], me, "{line}");
    }
}

/// Catches a failed unpack reported as success, a fetch's blobs kept after it, and the
/// failure reported without the helper's words. The runtime fails the unpack and the
/// removal of its stage alike (and lets the hand-over of the stage through); the removal's
/// failure does not hide the unpack's.
#[tokio::test]
async fn a_failed_unpack_is_reported_and_leaves_no_blobs() {
    let dir = scratch("image-unpack-fails");
    let scratch = dir.join("scratch");
    let runtime = scripted_runtime(
        &dir,
        &scratch.join("oci-images"),
        "helper-unpack-*|helper-rm-*",
    );
    let oci = Oci::on(
        &OciConfig {
            runtime,
            ..config(&dir, true)
        },
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
    // No job user is configured, so nothing is dropped to for the keeper either.
    assert!(oci.job_drop().is_none());
}

/// Catches a helper's directory given by a path that was replaced by a link (the link, not
/// the directory, would be followed) and a directory not given to the account crun runs as:
/// a directory is given (to the daemon's own ids here, which changes nothing), and a link to
/// one is refused with its path in the words.
#[test]
fn a_helpers_directory_is_given_by_descriptor_and_never_through_a_link() {
    let dir = scratch("give-job");
    let oci = start(&config(&dir, true), &dir.join("s")).expect("starts");
    let real = dir.join("real");
    std::fs::create_dir(&real).expect("dir");
    oci.give_to_job_user(&real).expect("given");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    let why = oci.give_to_job_user(&link).expect_err("a link").to_string();
    assert!(why.contains("link"), "{why}");
    let image = PinnedImage {
        reference: format!("r/x@sha256:{}", "ab".repeat(32)),
        digest: format!("sha256:{}", "ab".repeat(32)),
    };
    assert_eq!(
        oci.image_dir(&image),
        dir.join("s/oci-images").join("ab".repeat(32))
    );
}

/// A source that counts its fetches and publishes nothing.
#[derive(Debug, Default)]
struct Counting(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl ImageSource for Counting {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

/// Catches an image that is removed and then never fetched again by the daemon that fetched
/// it once (the memory of the fetch outliving the files), and one fetched twice when it was
/// not removed: a second use fetches nothing, a removal makes the next use fetch again, a
/// removal of what is gone is no error, and the image is there after.
#[tokio::test]
async fn a_removed_image_is_fetched_again_and_a_present_one_is_not() {
    let dir = scratch("image-removed");
    let source = Arc::new(Counting::default());
    let oci = Oci::on(
        &config(&dir, true),
        source.clone(),
        &dir.join("s"),
        "/actions",
        "x86_64",
    )
    .expect("starts");
    let image = PinnedImage {
        reference: format!("r/x@sha256:{}", "ab".repeat(32)),
        digest: format!("sha256:{}", "ab".repeat(32)),
    };
    let fetched = || source.0.load(std::sync::atomic::Ordering::SeqCst);
    oci.check_pinned(&image).await.expect("fetched");
    oci.check_pinned(&image).await.expect("not again");
    assert_eq!(fetched(), 1);
    assert!(oci.image_dir(&image).is_dir());
    // A removal that fails says so and keeps the image.
    let store = dir.join("s/oci-images");
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).expect("mode");
    assert!(oci.remove_image(&image).await.is_err());
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).expect("mode");
    assert!(oci.image_dir(&image).is_dir());
    oci.remove_image(&image).await.expect("removed");
    assert!(!oci.image_dir(&image).exists());
    oci.remove_image(&image).await.expect("already gone");
    oci.check_pinned(&image).await.expect("fetched again");
    assert_eq!(fetched(), 2);
    assert!(oci.image_dir(&image).is_dir());
}

/// Catches the egress settings of the backend not reaching a job's bundle: a configured
/// `resolv.conf` (the resolvers an allow-listed network names, and only those) replaces the
/// loopback one, and a route wait puts `--wait-route <seconds>` between the init and the
/// job in the container's arguments, while the defaults leave both as they were.
#[tokio::test]
async fn the_resolvers_and_the_route_wait_reach_the_bundle() {
    let dir = scratch("egress-bundle");
    let mut configured = config(&dir, true);
    configured.resolv_conf = "nameserver 192.0.2.53\noptions timeout:1 attempts:1\n".to_owned();
    configured.route_wait = Some(std::time::Duration::from_secs(7));
    let oci = start(&configured, &dir.join("scratch")).expect("started");
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("lease");
    oci.create(&spec).await.expect("bundle");
    let resolv = std::fs::read_to_string(spec.dir.join("etc/resolv.conf")).expect("resolv.conf");
    assert_eq!(
        resolv,
        "nameserver 192.0.2.53\noptions timeout:1 attempts:1\n"
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(spec.dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    assert_eq!(
        config["process"]["args"],
        json!([
            "/dev/js-init",
            "--wait-route",
            "7",
            "--",
            "tool",
            "--flag",
            "a b"
        ])
    );

    let plain = start(&config_default(&dir), &dir.join("scratch2")).expect("started");
    let mut spec = crate::container::tests::spec();
    spec.dir = dir.join("lease2");
    plain.create(&spec).await.expect("bundle");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(spec.dir.join("bundle/config.json")).expect("read"))
            .expect("json");
    assert_eq!(
        config["process"]["args"],
        json!(["/dev/js-init", "--", "tool", "--flag", "a b"])
    );
}

/// The default configuration of the tests, with the defaults of the egress settings.
fn config_default(dir: &Path) -> OciConfig {
    let config = config(dir, true);
    assert_eq!(config.resolv_conf, RESOLV_CONF);
    assert_eq!(config.route_wait, None);
    config
}

/// Catches the pinned seccomp profile, resolved for the capability set a job really gets
/// (none), refusing a system call a job's network path needs: the calls an allow-listed job
/// makes to open a socket, connect, send, receive and ask the socket about itself are all
/// in an allow entry. (What stops a job from reconfiguring the network is the kernel and the
/// empty capability set, not this filter: see `a_job_cannot_alter_the_rules_even_with_every_capability_of_its_own_namespace` in the
/// agent's egress tests.)
#[test]
fn the_seccomp_profile_resolved_for_a_job_allows_the_calls_of_a_network_client() {
    let section = seccomp_section("amd64", &[]).expect("amd64 has a filter");
    let allowed: std::collections::BTreeSet<&str> = section["syscalls"]
        .as_array()
        .expect("syscalls")
        .iter()
        .filter(|entry| entry["action"] == "SCMP_ACT_ALLOW")
        .flat_map(|entry| entry["names"].as_array().expect("names"))
        .filter_map(|name| name.as_str())
        .collect();
    for call in [
        "socket",
        "connect",
        "bind",
        "sendto",
        "recvfrom",
        "sendmsg",
        "recvmsg",
        "getsockopt",
        "setsockopt",
        "getsockname",
        "getpeername",
        "shutdown",
        "poll",
        "epoll_wait",
    ] {
        assert!(allowed.contains(call), "{call} is not allowed: {allowed:?}");
    }
}

/// Catches two agents on one scratch directory refusing each other, and a start that
/// sweeps the stage of an unpack another agent has under way: a lone backend sweeps the
/// stages a dead run left, a second one opens beside it (shared) and sweeps nothing, so the
/// first's in-flight stage survives the second's open; once both are dropped the directory
/// can be started again and sweeps what is there.
#[test]
fn agents_share_a_scratch_directory_and_only_a_lone_one_sweeps() {
    let dir = scratch("shared-backends");
    let scratch = dir.join("scratch");
    let images = scratch.join("oci-images");
    let leftover = images.join(".stage-aa-1-0");
    std::fs::create_dir_all(&leftover).expect("a dead run's stage");
    let first = start(&config(&dir, true), &scratch).expect("starts");
    assert!(!leftover.exists(), "a lone backend left the stage it found");
    let in_flight = images.join(".stage-bb-2-0");
    std::fs::create_dir_all(&in_flight).expect("the first's stage");
    let second = start(&config(&dir, true), &scratch).expect("opens beside the first");
    assert!(in_flight.is_dir(), "the second backend swept a live stage");
    drop(first);
    // The second is still live: a third opens beside it and sweeps nothing either.
    let third = start(&config(&dir, true), &scratch).expect("opens beside the second");
    assert!(in_flight.is_dir(), "the third backend swept a live stage");
    drop((second, third));
    assert_eq!(started(&config(&dir, true), &scratch), "started");
    assert!(
        !in_flight.exists(),
        "a lone backend left a dead run's stage"
    );
}

/// Catches a pull that runs beside an agent (its stage would be shared with the agent's
/// sweep and the agent's unpacks with its own), an agent that starts a second pull
/// beside it, and a lock that outlives its holder: a backend that needs the directory alone
/// is refused, with the lock file named and nothing swept, while an agent or another such
/// backend is live, and is let in once they are dropped; a lock file that cannot be opened
/// is not skipped.
#[test]
fn a_backend_that_needs_the_directory_alone_is_refused_while_any_other_is_live() {
    let dir = scratch("one-backend");
    let scratch = dir.join("scratch");
    let alone = OciConfig {
        exclusive_scratch: true,
        ..config(&dir, true)
    };
    let agent = start(&config(&dir, true), &scratch).expect("starts");
    let stage = scratch.join("oci-images/.stage-aa-1-0");
    std::fs::create_dir_all(&stage).expect("stage");
    let refused = start(&alone, &scratch).expect_err("locked");
    let OciError::Locked { path, .. } = &refused else {
        panic!("not the lock: {refused}");
    };
    assert_eq!(path, &scratch.join(lock::LOCK_FILE));
    assert!(refused.to_string().contains(".js-oci.lock"), "{refused}");
    assert!(stage.is_dir(), "the live backend's stage was swept");
    drop(agent);
    let pull = start(&alone, &scratch).expect("alone now");
    assert!(!stage.exists(), "the pull left a dead run's stage");
    // A second pull is refused beside the first.
    assert!(matches!(
        start(&alone, &scratch),
        Err(OciError::Locked { .. })
    ));
    drop(pull);
    assert_eq!(started(&alone, &scratch), "started");

    // A directory where the lock file should be: not skipped, and not a lock refusal.
    let blocked = dir.join("blocked");
    std::fs::create_dir_all(blocked.join(lock::LOCK_FILE)).expect("directory");
    let said = started(&config(&dir, true), &blocked);
    assert!(said.contains(lock::LOCK_FILE), "{said}");
    assert!(said.contains("Is a directory"), "{said}");
}

/// Catches an unpacked image that is only in the page cache when it is published (a power
/// loss would leave a published directory without its data) by pinning what the flush
/// steps do: each succeeds on a directory and reports a path that is gone, so a flush that
/// swallows its error shows. A flush that is skipped altogether, and the order (flush,
/// rename, flush) of `Images::acquire`, cannot be seen from a running process: a power
/// loss is not a test.
#[test]
fn the_flush_steps_of_a_publish_succeed_on_a_directory_and_fail_on_a_missing_one() {
    let dir = scratch("flush");
    image::flush_fs(&dir).expect("syncfs");
    image::flush_dir(&dir).expect("fsync");
    let gone = dir.join("gone");
    assert_eq!(
        image::flush_fs(&gone).expect_err("missing").kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        image::flush_dir(&gone).expect_err("missing").kind(),
        std::io::ErrorKind::NotFound
    );
}

/// Catches the ownership change that follows a link (a job user's link in place of the
/// work directory would give root's target away) and one that is lost: a directory is
/// changed by its descriptor (to its own ids here, which any user may do), a link to it is
/// refused, and so is a path that is not there.
#[test]
fn a_work_directory_is_given_by_descriptor_and_never_through_a_link() {
    let dir = scratch("chown-dir");
    let real = dir.join("real");
    std::fs::create_dir(&real).expect("dir");
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    chown_dir(&real, uid, gid).expect("own directory");
    assert_eq!(
        real.metadata().expect("meta").permissions().mode() & 0o777,
        0o700
    );
    let link = dir.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    assert!(chown_dir(&link, uid, gid).is_err(), "followed a link");
    assert!(chown_dir(&dir.join("absent"), uid, gid).is_err());
}

/// Catches a backend whose `job_drop` is empty or built from other ids than the job user it
/// was configured with: a daemon that is root (told so, as a test cannot be root) with a job
/// user returns the drop for exactly that user and the configured tool path, and a daemon that
/// is not root returns none.
#[test]
fn a_root_daemon_with_a_job_user_hands_out_the_drop_for_that_user() {
    let dir = scratch("job-drop");
    let mut config = config(&dir, true);
    let job = rustix::process::getuid().as_raw() + 1;
    let ids = IdFiles::in_dir(&dir.join("ids"));
    std::fs::write(&ids.passwd, format!("job:x:{job}:{job}::/:/bin/sh\n")).expect("passwd");
    std::fs::write(&ids.subuid, "job:300000:65536\n").expect("subuid");
    std::fs::write(&ids.subgid, "job:400000:65536\n").expect("subgid");
    config.job_user = Some((job, job + 5));
    let on = |daemon: (u32, u32)| {
        Oci::on_as(
            &config,
            Arc::new(NoImages),
            &dir.join(format!("s{}", daemon.0)),
            "/actions",
            "x86_64",
            daemon,
        )
    };
    let root = on((0, 0)).expect("starts as root");
    assert_eq!(
        root.job_drop(),
        crate::oci::JobDrop::new(Some((job, job + 5)), config.id_tool_path.as_deref(), 0).as_ref()
    );
    assert!(root.job_drop().is_some());
    let user = on((job, job)).expect("starts as the job user");
    assert!(user.job_drop().is_none());
}
