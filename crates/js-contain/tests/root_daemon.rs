//! js-contain's `Oci` driven by a ROOT daemon that runs every program as a dedicated
//! non-root job user (privilege option B of `docs/design/agent-privilege.md`), against a
//! real `crun`. `#[ignore]`d: it needs root, `crun`, `uidmap`, a static `busybox`, a job
//! user with a subordinate id range and a writable cgroup v2 mount. The `agent-privilege`
//! job of `.github/workflows/ci.yml` prepares all of it and runs this file under `sudo`;
//! the variables it sets are:
//!
//! - `JS_B_CRUN`, `JS_B_BUSYBOX`: the programs;
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
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use js_contain::container::{ACTION_ID, ContainerLimits, ContainerSpec, Ended};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_contain::oci::{Oci, OciConfig, OciError, stderr_path, stdout_path};
use js_contain::subids::{IdFiles, user_ids};

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

static JOB_USER: OnceLock<(u32, u32)> = OnceLock::new();

/// The `EnvScrub` of a root daemon: every program the backend starts (`crun`, in every
/// verb) runs as the job user, with no supplementary groups, from `/`.
fn as_job_user(command: &mut std::process::Command) {
    use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};
    let (uid, gid) = *JOB_USER.get().expect("the job user is set");
    command
        .current_dir("/")
        .env("HOME", "/")
        .env_remove("XDG_RUNTIME_DIR");
    // SAFETY: the closure makes system calls only, in the forked child before it execs.
    unsafe {
        command.pre_exec(move || {
            let (uid, gid) = (
                rustix::process::Uid::from_raw(uid),
                rustix::process::Gid::from_raw(gid),
            );
            set_thread_groups(&[])?;
            set_thread_res_gid(gid, gid, gid)?;
            set_thread_res_uid(uid, uid, uid)?;
            Ok(())
        });
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

fn config(job: (u32, u32)) -> OciConfig {
    OciConfig {
        runtime: PathBuf::from(var("JS_B_CRUN")),
        daemon: std::env::current_exe().unwrap(),
        scrub_env: as_job_user,
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

/// B: a root daemon starts a job container with the real `crun` as the job user, and
/// the container's user namespace maps onto the job user's subordinate range, not root's:
/// inside, id 0 is the first id of that range, the process is the unprivileged id 1000
/// with no capability, and the files `crun` made for the lease belong to the job user.
#[tokio::test]
#[ignore = "needs root, crun, busybox and a job user: see the module docs"]
async fn a_root_daemon_runs_a_container_mapped_onto_the_job_users_subordinate_ids() {
    let (uid, gid) = (id_var("JS_B_UID"), id_var("JS_B_GID"));
    assert_ne!(uid, 0, "the job user is not root");
    JOB_USER.set((uid, gid)).unwrap();
    let ids = user_ids(&IdFiles::system(), uid, gid).unwrap();
    assert_ne!(ids.sub_uid, 0);

    let work = world_dir("js-b-root-daemon");
    let scratch = work.join("scratch");

    // Before: a scratch root only root can search is refused by the backend's own check,
    // because the job user's ids could not reach a lease below it.
    mkdir(&scratch, 0o700);
    let refused = Oci::new(
        &config((uid, gid)),
        Arc::new(NoImages),
        &scratch,
        "/js-b-test",
    );
    assert!(
        matches!(refused, Err(OciError::NotSearchable(_))),
        "{:?}",
        refused.err()
    );
    // And without the job user a root daemon has no range of its own to map.
    mkdir(&scratch, 0o711);
    let own = OciConfig {
        job_user: None,
        ..config((uid, gid))
    };
    let own = Oci::new(&own, Arc::new(NoImages), &scratch, "/js-b-test");
    assert!(matches!(own, Err(OciError::Subids(_))), "{:?}", own.err());

    // The image store, as an unpack would leave it.
    let hex = "b".repeat(64);
    let image = PinnedImage {
        reference: format!("local/busybox@sha256:{hex}"),
        digest: format!("sha256:{hex}"),
    };
    busybox_rootfs(&scratch.join("oci-images").join(&hex).join("rootfs"));

    // After: 0711 is accepted.
    let cgroup = Path::new("/sys/fs/cgroup/js-b-test");
    let _ = fs::remove_dir(cgroup.join("lease1"));
    mkdir(&cgroup.join("supervisor"), 0o755);
    let oci = Oci::new(
        &config((uid, gid)),
        Arc::new(NoImages),
        &scratch,
        "/js-b-test",
    )
    .unwrap_or_else(|why| panic!("a root daemon starts the backend: {why}"));
    oci.check_pinned(&image).await.unwrap();

    // The delegated cgroup subtree, owned by the job user; this process (and so every
    // program it starts) sits in a leaf of it.
    mkdir(&cgroup.join("lease1"), 0o755);
    fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids").unwrap();
    fs::write(
        cgroup.join("lease1/cgroup.subtree_control"),
        "+memory +pids",
    )
    .unwrap();
    delegate(cgroup, uid, gid);
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
        cgroup_parent: "/js-b-test/lease1".to_owned(),
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
    assert_eq!(ended, Ok(Ended::Exited(0)), "{said}");

    let lines: Vec<&str> = stdout.lines().collect();
    let map: Vec<&str> = lines
        .first()
        .copied()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    assert_eq!(
        map,
        ["0".to_owned(), ids.sub_uid.to_string(), "65536".to_owned()],
        "the namespace maps id 0 onto the job user's first subordinate id: {said}"
    );
    assert_eq!(
        lines.len(),
        4,
        "one mapping line, then id, gid, CapEff: {said}"
    );
    assert_eq!(lines[1], "1000", "the job is the unprivileged id: {said}");
    assert_eq!(lines[2], "1000", "{said}");
    assert!(
        lines[3].ends_with("0000000000000000"),
        "no capability: {said}"
    );

    // crun ran as the job user: its log and state belong to that user (root made the
    // lease directory over to it, and nothing else of root's is in the way).
    assert_eq!(fs::metadata(&reports.log).unwrap().uid(), uid, "{said}");
    oci.remove("lease1").unwrap();
}
