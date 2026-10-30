//! js-contain's `Oci` driven by a ROOT daemon that runs every program as a dedicated
//! non-root job user (privilege option B of `docs/design/agent-privilege.md`), against a
//! real `crun`. `#[ignore]`d: it needs root, `crun`, `uidmap`, a static `busybox`, a job
//! user with a subordinate id range and a writable cgroup v2 mount. The `agent-privilege`
//! job of `.github/workflows/ci.yml` prepares all of it and runs this file under `sudo`;
//! the variables it sets are:
//!
//! - `JS_B_CRUN`, `JS_B_BUSYBOX`: the programs (`JS_B_CRUN` is the distribution's
//!   `/usr/bin/crun`: the path is what AppArmor attaches its profile to, and the tests that
//!   run a container check it);
//! - `JS_B_UID`, `JS_B_GID`: the job user (an account in `/etc/passwd`, with a range in
//!   `/etc/subuid` and `/etc/subgid`).
//!
//! What this stands in for: the image is pre-unpacked into the store (the unpack helper
//! belongs to a later change), and the hand-over of the lease directories to the job's ids
//! is done directly by the root test, as a root daemon can (`chown`), with no helper
//! container. A missing variable or a failed step is a failure, never a skip.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use js_contain::container::{ACTION_ID, ContainerLimits, ContainerSpec, Ended};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{Oci, OciConfig, OciError, stderr_path, stdout_path};
use js_contain::subids::{IdFiles, SubidError, user_ids};

#[derive(Debug)]
struct NoImages;

#[async_trait]
impl ImageSource for NoImages {
    async fn fetch(&self, _: &PinnedImage, _: &Path) -> Result<Vec<String>, FetchError> {
        Err(FetchError::Failed(
            "the image is already in the store".to_owned(),
        ))
    }
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}: see the module docs"))
}

fn id_var(name: &str) -> u32 {
    var(name).parse().unwrap()
}

fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A directory below `/tmp` that every user can search.
fn world_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    mkdir(&dir, 0o755);
    dir
}

/// A minimal image root: busybox and the mount points the bundle names.
fn busybox_rootfs(rootfs: &Path) {
    for dir in [
        "bin", "proc", "dev", "sys", "tmp", "var/tmp", "run", "root", "etc", "work",
    ] {
        mkdir(&rootfs.join(dir), 0o755);
    }
    for file in ["hosts", "hostname", "resolv.conf"] {
        fs::write(rootfs.join("etc").join(file), "").unwrap();
    }
    let busybox = rootfs.join("bin/busybox");
    fs::copy(var("JS_B_BUSYBOX"), &busybox).unwrap();
    fs::set_permissions(&busybox, fs::Permissions::from_mode(0o755)).unwrap();
    for applet in ["sh", "cat", "id", "grep"] {
        symlink("busybox", rootfs.join("bin").join(applet)).unwrap();
    }
}

/// The configuration of a root daemon: the job user, and nothing else of the privilege drop
/// (the backend does it; there is no `scrub_env` here).
fn config(job: (u32, u32), runtime: &Path) -> OciConfig {
    OciConfig {
        runtime: runtime.to_owned(),
        id_tool_path: Some("/usr/bin:/bin".into()),
        daemon: std::env::current_exe().unwrap(),
        job_user: Some(job),
        ..OciConfig::default()
    }
}

/// Lets the job user manage the cgroup subtree `dir` (as a unit's `Delegate=` does): the
/// directory and its control files belong to the job user, and the controllers a lease
/// needs are enabled for its children.
fn delegate(dir: &Path, uid: u32, gid: u32) {
    for name in [
        "",
        "cgroup.procs",
        "cgroup.threads",
        "cgroup.subtree_control",
    ] {
        let path = dir.join(name);
        lchown(&path, Some(uid), Some(gid)).unwrap();
    }
}

/// What one container run under the root daemon left.
struct Run {
    ended: Result<Ended, String>,
    stdout: String,
    /// Everything there is to read about the run, for an assertion message.
    said: String,
    ids: js_contain::subids::DaemonIds,
    log_uid: Option<u32>,
}

/// The job user of the CI job, from the environment it prepares.
fn job_user() -> (u32, u32) {
    let (uid, gid) = (id_var("JS_B_UID"), id_var("JS_B_GID"));
    assert_ne!(uid, 0, "the job user is not root");
    (uid, gid)
}

/// A root daemon on `runtime` starts a busybox container that prints its own view of its
/// ids and capabilities, with the cgroup, scratch and lease directories of `label`.
async fn run_container(label: &str, runtime: &Path) -> Run {
    let (uid, gid) = job_user();
    let ids = user_ids(&IdFiles::system(), uid, gid).unwrap();
    assert_ne!(ids.sub_uid, 0);

    let work = world_dir(&format!("js-b-{label}"));
    let scratch = work.join("scratch");
    let cgroup_parent = format!("/js-b-{label}");

    // Before: a scratch root only root can search is refused by the backend's own check,
    // because the job user's ids could not reach a lease below it.
    mkdir(&scratch, 0o700);
    let refused = Oci::new(
        &config((uid, gid), runtime),
        Arc::new(NoImages),
        &scratch,
        &cgroup_parent,
    );
    assert!(
        matches!(refused, Err(OciError::NotSearchable(_))),
        "{:?}",
        refused.err()
    );
    // And without the job user a root daemon is refused: root's own ids are not a range
    // to map.
    mkdir(&scratch, 0o711);
    let own = OciConfig {
        job_user: None,
        ..config((uid, gid), runtime)
    };
    let own = Oci::new(&own, Arc::new(NoImages), &scratch, &cgroup_parent);
    assert!(
        matches!(own, Err(OciError::Subids(SubidError::Root { .. }))),
        "{:?}",
        own.err()
    );

    // The image store, as an unpack would leave it.
    let hex = "b".repeat(64);
    let image = PinnedImage {
        reference: format!("local/busybox@sha256:{hex}"),
        digest: format!("sha256:{hex}"),
    };
    busybox_rootfs(&scratch.join("oci-images").join(&hex).join("rootfs"));

    // After: 0711 is accepted.
    let cgroup = PathBuf::from(format!("/sys/fs/cgroup{cgroup_parent}"));
    let _ = fs::remove_dir(cgroup.join("lease1"));
    mkdir(&cgroup.join("supervisor"), 0o755);
    let oci = Oci::new(
        &config((uid, gid), runtime),
        Arc::new(NoImages),
        &scratch,
        &cgroup_parent,
    )
    .unwrap_or_else(|why| panic!("a root daemon starts the backend: {why}"));
    oci.check_pinned(&image).await.unwrap();

    // The delegated cgroup subtree, owned by the job user; this process sits in a leaf of
    // it, beside the containers.
    mkdir(&cgroup.join("lease1"), 0o755);
    fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids").unwrap();
    fs::write(
        cgroup.join("lease1/cgroup.subtree_control"),
        "+memory +pids",
    )
    .unwrap();
    delegate(&cgroup, uid, gid);
    delegate(&cgroup.join("lease1"), uid, gid);
    fs::write(
        cgroup.join("supervisor/cgroup.procs"),
        std::process::id().to_string(),
    )
    .unwrap();

    // The lease directories. What a helper container gives a rootless daemon, a root daemon
    // does itself: the lease belongs to the job user; the directories the job writes
    // belong to the id the job runs as (subordinate id 1000 of the range).
    let lease = scratch.join("lease1");
    mkdir(&lease, 0o755);
    lchown(&lease, Some(uid), Some(gid)).unwrap();
    let (job_uid, job_gid) = (ids.sub_uid + ACTION_ID, ids.sub_gid + ACTION_ID);
    for name in [
        "root",
        "upper",
        "work",
        "rw/tmp",
        "rw/var-tmp",
        "rw/run",
        "rw/home",
    ] {
        let dir = lease.join(name);
        mkdir(&dir, 0o755);
        lchown(&dir, Some(job_uid), Some(job_gid)).unwrap();
    }
    let spec = ContainerSpec {
        name: "lease1".to_owned(),
        image: image.reference.clone(),
        image_digest: image.digest.clone(),
        dir: lease.clone(),
        cgroup_parent: format!("{cgroup_parent}/lease1"),
        exec_root: "/work".to_owned(),
        input_root: lease.join("root"),
        upper: lease.join("upper"),
        overlay_work: lease.join("work"),
        working_directory: String::new(),
        env: vec![("PATH".to_owned(), "/bin".to_owned())],
        argv: [
            "/bin/sh",
            "-c",
            "cat /proc/self/uid_map; id -u; id -g; grep CapEff /proc/self/status",
        ]
        .map(str::to_owned)
        .to_vec(),
        limits: ContainerLimits::DEFAULT,
        shared: Vec::new(),
        mounts: Vec::new(),
    };
    oci.create(&spec).await.unwrap();
    let (out_path, err_path) = (stdout_path(&lease), stderr_path(&lease));
    let mut running = oci.start(&spec).unwrap();
    running.wait().await;
    let ended = oci.ended(&spec, &running).await;
    let stdout = fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = fs::read_to_string(&err_path).unwrap_or_default();
    let reports = oci.reports(&spec);
    let said = format!(
        "ended {ended:?}\nstdout:\n{stdout}\nstderr:\n{stderr}\ncrun log:\n{}\ninit report:\n{}",
        fs::read_to_string(&reports.log).unwrap_or_default(),
        fs::read_to_string(&reports.init).unwrap_or_default()
    );
    let log_uid = fs::metadata(&reports.log).ok().map(|m| m.uid());
    let _ = oci.remove("lease1");
    Run {
        ended,
        stdout,
        said,
        ids,
        log_uid,
    }
}

/// The value of the AppArmor user-namespace restriction on this host (`1`: an unconfined
/// process may not create a user namespace).
fn userns_restriction() -> String {
    fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map_or_else(|e| format!("unreadable: {e}"), |v| v.trim().to_owned())
}

/// B: a root daemon starts a job container with the real, distribution `crun` as the job
/// user, and the container's user namespace maps onto the job user's subordinate range,
/// not root's: inside, id 0 is the first id of that range, the process is the
/// unprivileged id 1000 with no capability, and the files `crun` made for the lease belong
/// to the job user. The CI job runs it with the AppArmor restriction on, so it also shows
/// the distribution's `crun` profile letting the job user create the namespace.
#[tokio::test]
#[ignore = "needs root, crun, busybox and a job user: see the module docs"]
async fn a_root_daemon_runs_a_container_mapped_onto_the_job_users_subordinate_ids() {
    let runtime = PathBuf::from(var("JS_B_CRUN"));
    assert_eq!(
        runtime,
        Path::new("/usr/bin/crun"),
        "the backend runs the distribution's crun by its path: the profile attaches to the path"
    );
    println!(
        "apparmor_restrict_unprivileged_userns = {}",
        userns_restriction()
    );
    let run = run_container("root-daemon", &runtime).await;
    assert_eq!(run.ended, Ok(Ended::Exited(0)), "{}", run.said);
    let ids = run.ids;
    let lines: Vec<&str> = run.stdout.lines().collect();
    let map: Vec<&str> = lines
        .first()
        .copied()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    assert_eq!(
        map,
        ["0".to_owned(), ids.sub_uid.to_string(), "65536".to_owned()],
        "the namespace maps id 0 onto the job user's first subordinate id: {}",
        run.said
    );
    assert_eq!(
        lines.len(),
        4,
        "one mapping line, then id, gid, CapEff: {}",
        run.said
    );
    assert_eq!(
        lines[1], "1000",
        "the job is the unprivileged id: {}",
        run.said
    );
    assert_eq!(lines[2], "1000", "{}", run.said);
    assert!(
        lines[3].ends_with("0000000000000000"),
        "no capability: {}",
        run.said
    );
    // crun ran as the job user: its log belongs to that user (root made the lease
    // directory over to it, and nothing else of root's is in the way).
    assert_eq!(run.log_uid, Some(job_user().0), "{}", run.said);
}

/// The control of the AppArmor restriction, the other half of the test above: the same
/// `crun` binary at another path is refused. AppArmor attaches the distribution's profile
/// (which grants `userns`) to the path `/usr/bin/crun`; a copy is unconfined, and with
/// `kernel.apparmor_restrict_unprivileged_userns=1` an unconfined process of the job user
/// may not create the user namespace. The test refuses to run with the restriction off,
/// where it would show nothing, and asserts which failure occurs: the runtime reports the
/// refused namespace, not any other failure.
#[tokio::test]
#[ignore = "needs root, crun, busybox, a job user and kernel.apparmor_restrict_unprivileged_userns=1"]
async fn a_copy_of_crun_is_refused_the_user_namespace_while_the_distribution_path_is_not() {
    assert_eq!(
        userns_restriction(),
        "1",
        "this control proves nothing with the restriction off"
    );
    let dir = world_dir("js-b-copybin");
    let copy = dir.join("crun");
    fs::copy(var("JS_B_CRUN"), &copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
    let run = run_container("crun-copy", &copy).await;
    println!("crun copy under the restriction: {}", run.said);
    assert_ne!(run.ended, Ok(Ended::Exited(0)), "{}", run.said);
    assert!(
        run.said.contains("Operation not permitted") || run.said.contains("Permission denied"),
        "the failure is the refused user namespace: {}",
        run.said
    );
    assert!(
        run.stdout.is_empty(),
        "the container never ran: {}",
        run.said
    );
}

/// Every program the backend starts runs as the job user, from the backend and not from
/// a hook of the caller: a stand-in `crun` records, for each verb (`kill`, `delete` and a
/// helper `run`), the ids and groups it runs with and its environment. All of them are the
/// job user's three ids each (real, effective and saved) with no supplementary group
/// (root's own group list is not inherited), and the environment is `HOME` and `PATH`
/// only (no `XDG_RUNTIME_DIR`, nothing of the daemon's).
#[tokio::test]
#[ignore = "needs root and a job user: see the module docs"]
async fn every_crun_command_of_a_root_daemon_runs_as_the_job_user_with_a_clean_environment() {
    let (uid, gid) = job_user();
    let dir = world_dir("js-b-drop");
    mkdir(&dir, 0o777);
    let crun = dir.join("crun");
    fs::copy(env!("CARGO_BIN_EXE_js-fake-crun"), &crun).unwrap();
    fs::set_permissions(&crun, fs::Permissions::from_mode(0o755)).unwrap();
    let scratch = dir.join("scratch");
    mkdir(&scratch, 0o711);
    let oci = Oci::new(
        &config((uid, gid), &crun),
        Arc::new(NoImages),
        &scratch,
        "/js-b-drop",
    )
    .unwrap();
    let lease = scratch.join("lease1");
    mkdir(&lease, 0o755);
    oci.terminate("lease1").await.unwrap();
    oci.remove("lease1").unwrap();
    let helper_dir = scratch.join("to-remove");
    mkdir(&helper_dir, 0o755);
    oci.remove_dir_async(&helper_dir).await.unwrap();

    let who = fs::read_to_string(crun.with_extension("who")).unwrap();
    let calls = fs::read_to_string(crun.with_extension("calls")).unwrap();
    let lines: Vec<&str> = who.lines().collect();
    let verbs = |verb: &str| {
        calls
            .lines()
            .any(|l| l.split_whitespace().any(|w| w == verb))
    };
    assert!(
        verbs("kill") && verbs("delete") && verbs("run"),
        "the three paths ran crun: {calls}"
    );
    assert_eq!(lines.len(), calls.lines().count(), "{who}\n{calls}");
    let expected = format!(
        "uid={uid},{uid},{uid},{uid} gid={gid},{gid},{gid},{gid} groups= env=HOME,PATH \
         path=/usr/bin:/bin home=/"
    );
    for line in lines {
        assert_eq!(line, expected, "{who}");
    }
}
