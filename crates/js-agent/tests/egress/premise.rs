//! The premise of the earlier egress decision, re-proved under the privilege model that was
//! built: the decision assumed a root `crun` that joins a network namespace root made. Under
//! option B `crun` runs as the job user, and a namespace made by root is owned by the
//! initial user namespace, which the job user's new user namespace has no capability over.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};

use js_contain::subids::{IdFiles, user_ids};
use serde_json::json;

use crate::container::{job_user, mkdir, world_dir};
use crate::sys::{ip, serial, tool, var};

/// A bundle whose container maps one id range of the job user, runs `echo started` and has
/// the network namespace `net` (`None` for a new one), run by the distribution's `crun` as
/// the job user: the way the root agent starts every container. Returns `(exit success,
/// output)`.
fn crun_as_job_user(label: &str, net: Option<&str>) -> (bool, String) {
    let (uid, gid) = job_user();
    let ids = user_ids(&IdFiles::system(), uid, gid).unwrap();
    let dir = world_dir(&format!("js-e-premise-{label}"));
    let rootfs = dir.join("rootfs");
    for sub in ["proc", "dev", "usr"] {
        mkdir(&rootfs.join(sub), 0o755);
    }
    for (link, target) in [
        ("bin", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        symlink(target, rootfs.join(link)).unwrap();
    }
    let network = net.map_or_else(
        || json!({"type": "network"}),
        |path| json!({"type": "network", "path": path}),
    );
    let config = json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": {"uid": 1000, "gid": 1000},
            "args": ["/bin/sh", "-c", "echo started"],
            "env": ["PATH=/usr/bin:/bin"],
            "cwd": "/",
            "capabilities": {"bounding": [], "effective": [], "inheritable": [], "permitted": [], "ambient": []},
            "noNewPrivileges": true,
        },
        "root": {"path": "rootfs", "readonly": true},
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc", "options": ["nosuid", "noexec", "nodev"]},
            {"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "mode=755", "size=65536k"]},
            {"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind", "ro"]},
        ],
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"}, {"type": "uts"}, {"type": "mount"},
                network, {"type": "user"},
            ],
            "uidMappings": [{"containerID": 0, "hostID": ids.sub_uid, "size": 65536}],
            "gidMappings": [{"containerID": 0, "hostID": ids.sub_gid, "size": 65536}],
        },
    });
    fs::write(
        dir.join("config.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let state = dir.join("state");
    mkdir(&state, 0o700);
    std::os::unix::fs::chown(&state, Some(uid), Some(gid)).unwrap();
    // Root plays the agent: the same drop the backend makes before it execs `crun`.
    let mut crun = Command::new(var("JS_B_CRUN"));
    crun.arg("--root")
        .arg(&state)
        .args(["run", "--bundle"])
        .arg(&dir)
        .arg(format!("premise{label}"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/")
        .current_dir("/")
        .stdin(Stdio::null());
    // SAFETY: the closure makes three system calls and nothing else in the forked child.
    unsafe {
        crun.pre_exec(move || {
            let (uid, gid) = (
                rustix::process::Uid::from_raw(uid),
                rustix::process::Gid::from_raw(gid),
            );
            rustix::thread::set_thread_groups(&[])?;
            rustix::thread::set_thread_res_gid(gid, gid, gid)?;
            rustix::thread::set_thread_res_uid(uid, uid, uid)?;
            Ok(())
        });
    }
    let output = crun.output().expect("crun");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
    (output.status.success(), said)
}

/// Asserts that `crun`, run as the job user as option B runs it, cannot join a network
/// namespace that root made with `ip netns add`: it fails at `setns` with EPERM and the job
/// never runs. The control is the same bundle with a new network namespace, which runs, so
/// the refusal is the join's. This is why the design wires a namespace the container
/// created, from the host, instead of handing the container one made beforehand.
#[test]
#[ignore = "needs root, crun, a job user: see the module docs of the egress tests"]
fn crun_as_the_job_user_cannot_join_a_network_namespace_root_made() {
    let _serial = serial();
    let ip_bin = tool("ip");
    let _ = Command::new(&ip_bin)
        .args(["netns", "del", "js-e-root-ns"])
        .output();
    ip(&["netns", "add", "js-e-root-ns"]);
    let (joined, said) = crun_as_job_user("join", Some("/run/netns/js-e-root-ns"));
    let (control, control_said) = crun_as_job_user("new", None);
    let _ = Command::new(&ip_bin)
        .args(["netns", "del", "js-e-root-ns"])
        .output();
    println!("join a root-made netns by path, as the job user: {said}");
    println!("control, a new netns: {control_said}");
    assert!(control, "the control bundle does not run: {control_said}");
    assert!(control_said.contains("started"), "{control_said}");
    assert!(!joined, "crun joined a namespace root made: {said}");
    assert!(
        said.contains("setns") && said.contains("Operation not permitted"),
        "the failure is the kernel's ownership rule on setns: {said}"
    );
    assert!(!said.contains("started"), "the job ran: {said}");
}
