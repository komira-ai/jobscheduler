//! What the checks can see: the agent must share `init`'s pid and mount namespaces, the
//! state root must not share a file system with anything a job can fill, and an agent that
//! watches nothing says so. All three are checked when the agent starts, through the file
//! seam, so a test changes what the seam answers and not the machine it runs on.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::support::{Host, Scratch, code, linux_host, run_scripted, welcome};

/// The user containers run as, for an agent of any user.
fn container_user() -> String {
    let uid = rustix::process::geteuid().as_raw();
    match uid {
        0 => "1:1".to_owned(),
        _ => format!("{uid}:{}", rustix::process::getegid().as_raw()),
    }
}

/// Runs the agent on `host` with `extra` and returns (status, stderr, messages sent).
fn start_on(host: &Host, extra: &[&str]) -> (String, String, usize) {
    let (ran, sent, _) = run_scripted(extra, host, vec![welcome(1)], 1);
    let sent = sent.borrow().len();
    (ran.status(), ran.err, sent)
}

/// A private directory `name` under `dir`.
fn private(dir: &Scratch, name: &str) -> PathBuf {
    let path = dir.path(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// The flags that enable containers, with the state root `state`.
fn containers(dir: &Scratch, state: &std::path::Path) -> Vec<String> {
    let exec = dir.write("docker", "#!/bin/sh\n", 0o755);
    vec![
        "--container-bin".to_owned(),
        exec.display().to_string(),
        "--container-user".to_owned(),
        container_user(),
        "--state-root".to_owned(),
        state.display().to_string(),
    ]
}

fn args(owned: &[String]) -> Vec<&str> {
    owned.iter().map(String::as_str).collect()
}

/// Asserts that an agent that does not share `init`'s pid namespace, or its mount
/// namespace, does not start with containers (exit 2, nothing sent, the flag and the
/// namespace named), whichever side differs, and neither does one that cannot read the
/// links to tell; and that the same agent starts when it runs no containers, and starts
/// when the namespaces are the same. It catches an agent that cannot see its jobs'
/// processes or mounts and checks them anyway, which would call every host clean.
#[test]
fn an_agent_that_cannot_see_the_host_does_not_start_with_containers() {
    let dir = Scratch::new("reach-namespaces");
    let state = private(&dir, "state");
    let owned = containers(&dir, &state);
    let (status, err, sent) = start_on(&linux_host(), &args(&owned));
    assert_eq!(status, code(0), "the same namespaces start: {err}");
    assert_eq!(sent, 2);

    for (path, kind) in [
        ("/proc/self/ns/pid", "pid"),
        ("/proc/1/ns/pid", "pid"),
        ("/proc/self/ns/mnt", "mnt"),
        ("/proc/1/ns/mnt", "mnt"),
    ] {
        let mut host = linux_host();
        host.files = host.files.with_link(path, &format!("{kind}:[4026532999]"));
        let (status, err, sent) = start_on(&host, &args(&owned));
        assert_eq!(status, code(2), "{path}");
        assert!(
            err.contains("--container-bin: the agent is in a")
                && err.contains(&format!("{kind} namespace of its own")),
            "{path}: {err}"
        );
        assert_eq!(sent, 0, "{path}");

        let mut host = linux_host();
        host.files = host.files.without_link(path);
        let (status, err, sent) = start_on(&host, &args(&owned));
        assert_eq!(status, code(2), "{path} unreadable");
        assert!(
            err.contains("--container-bin: cannot tell whether the agent sees the host")
                && err.contains(path),
            "{path}: {err}"
        );
        assert_eq!(sent, 0, "{path} unreadable");

        let (status, err, _) = start_on(&host, &["--state-root", &state.display().to_string()]);
        assert_eq!(status, code(0), "no containers, no need to see jobs: {err}");
    }
}

/// Asserts that an agent whose namespace links match `init`'s but whose `/proc` is a
/// container's (no pid 2, or a pid 1 in a container's control group) does not start with
/// containers: exit 2, nothing sent, the flag and the reason named; and that it starts when
/// it runs no containers. It catches an agent in a container, or under `PrivatePIDs` with a
/// `/proc` of its own, that runs its jobs on the host through a socket and checks the
/// wrong machine.
#[test]
fn an_agent_with_a_container_proc_does_not_start_with_containers() {
    let dir = Scratch::new("reach-container-proc");
    let state = private(&dir, "state");
    let owned = containers(&dir, &state);
    let mut hosts = Vec::new();
    let mut host = linux_host();
    host.files = host.files.without("/proc/2/stat");
    hosts.push(("no pid 2", host, "pid 2 cannot be read"));
    let mut host = linux_host();
    host.files = host.files.with("/proc/2/stat", "2 (sh) S 0 0");
    hosts.push(("pid 2 is a shell", host, "pid 2 is \"sh\""));
    let mut host = linux_host();
    host.files = host.files.with("/proc/1/cgroup", "0::/docker/4f3a9c\n");
    hosts.push(("a container cgroup", host, "pid 1 is in the control group"));
    for (name, host, why) in hosts {
        let (status, err, sent) = start_on(&host, &args(&owned));
        assert_eq!(status, code(2), "{name}");
        assert!(
            err.contains("--container-bin: the agent's /proc is not the host's:")
                && err.contains(why),
            "{name}: {err}"
        );
        assert_eq!(sent, 0, "{name}");

        let (status, err, _) = start_on(&host, &["--state-root", &state.display().to_string()]);
        assert_eq!(status, code(0), "{name}: no containers: {err}");
    }
}

/// Asserts that a state root on the same file system as a mount root, or as the scratch
/// root, ends the agent with status 2 before anything is sent, naming the flags; that a
/// root that cannot be looked at ends it too (the check fails closed); and that roots on
/// other file systems start it, and so does a state root with no root beside it. It
/// catches a job that fills the disk that holds the taint marker, so that the marker
/// cannot be written and a restart forgets the taint.
#[test]
fn a_state_root_on_a_file_system_a_job_can_fill_is_refused() {
    let dir = Scratch::new("reach-devices");
    let (state, scratch, data) = (
        private(&dir, "state"),
        private(&dir, "scratch"),
        private(&dir, "data"),
    );
    let (state_s, scratch_s) = (state.display().to_string(), scratch.display().to_string());
    let mount = format!("data={}", data.display());
    let whole = fs::canonicalize(dir.path("")).unwrap();
    let with_scratch = ["--state-root", &state_s, "--scratch-root", &scratch_s];
    let with_mount = ["--state-root", &state_s, "--mount-root", &mount];

    let (status, err, _) = start_on(&linux_host(), &with_scratch);
    assert_eq!(status, code(0), "other file systems: {err}");
    let (status, err, _) = start_on(&linux_host(), &with_mount);
    assert_eq!(status, code(0), "other file systems: {err}");
    let (status, err, _) = start_on(&linux_host(), &["--state-root", &state_s]);
    assert_eq!(status, code(0), "nothing beside the state root: {err}");

    let mut same = linux_host();
    same.files = same.files.with_device_under(&whole, 7);
    for (flags, text) in [
        (&with_scratch, "the --scratch-root"),
        (&with_mount, "the --mount-root"),
    ] {
        let (status, err, sent) = start_on(&same, flags);
        assert_eq!(status, code(2), "{flags:?}");
        assert!(
            err.contains("--state-root: on the same file system as")
                && err.contains(text)
                && err.contains("could not be written"),
            "{flags:?}: {err}"
        );
        assert_eq!(sent, 0, "{flags:?}");
    }
    let (status, err, _) = start_on(&same, &["--state-root", &state_s]);
    assert_eq!(status, code(0), "a state root alone shares nothing: {err}");

    for (unlooked, flags) in [(&state, &with_scratch), (&scratch, &with_scratch)] {
        let mut blind = linux_host();
        blind.files = blind
            .files
            .unlooked_under(&fs::canonicalize(unlooked).unwrap());
        let (status, err, sent) = start_on(&blind, flags);
        assert_eq!(status, code(2), "{unlooked:?}");
        assert!(err.contains("Permission denied"), "{unlooked:?}: {err}");
        assert_eq!(sent, 0, "{unlooked:?}");
    }
}

/// Asserts that every mount root is compared with the state root, not only the first: with
/// two mount roots of which only the second is on the state root's device, the agent
/// refuses to start and names the second; with neither on it, it starts. It catches a
/// comparison that stops after the first root.
#[test]
fn a_second_mount_root_on_the_state_roots_device_is_refused() {
    let dir = Scratch::new("reach-two-roots");
    let (state, first, second) = (
        private(&dir, "state"),
        private(&dir, "first"),
        private(&dir, "second"),
    );
    let state_s = state.display().to_string();
    let (mount_a, mount_b) = (
        format!("a={}", first.display()),
        format!("b={}", second.display()),
    );
    let flags = [
        "--state-root",
        &state_s,
        "--mount-root",
        &mount_a,
        "--mount-root",
        &mount_b,
    ];
    let (status, err, _) = start_on(&linux_host(), &flags);
    assert_eq!(status, code(0), "other file systems: {err}");

    let (state_c, second_c) = (
        fs::canonicalize(&state).unwrap(),
        fs::canonicalize(&second).unwrap(),
    );
    let mut host = linux_host();
    host.files = host
        .files
        .with_device_under(&state_c, 7)
        .with_device_under(&second_c, 7);
    let (status, err, sent) = start_on(&host, &flags);
    assert_eq!(status, code(2), "{err}");
    assert!(
        err.contains("--state-root: on the same file system as the --mount-root")
            && err.contains(&second_c.display().to_string())
            && !err.contains(&fs::canonicalize(&first).unwrap().display().to_string()),
        "{err}"
    );
    assert_eq!(sent, 0);
}

/// Asserts that an agent with containers and no `--watch-dir` says so when it starts and
/// names the directories worth watching, and that one with a `--watch-dir`, and one with no
/// containers, say nothing of the kind. It catches an agent that checks two files and lets
/// the operator believe the host is watched.
#[test]
fn an_agent_that_watches_no_directory_says_so() {
    let dir = Scratch::new("reach-watch-warning");
    let state = private(&dir, "state");
    let watched = dir.path("watched");
    fs::create_dir(&watched).unwrap();
    let owned = containers(&dir, &state);
    let (status, err, _) = start_on(&linux_host(), &args(&owned));
    assert_eq!(status, code(0), "{err}");
    assert!(
        err.contains("js-agent: warning: no --watch-dir given: after a job only"),
        "{err}"
    );
    for suggested in [
        "/etc/cron.d",
        "/etc/sudoers.d",
        "/root/.ssh",
        "/etc/ld.so.conf.d",
    ] {
        assert!(err.contains(suggested), "{suggested}: {err}");
    }

    let watching = [
        owned.clone(),
        vec!["--watch-dir".to_owned(), watched.display().to_string()],
    ]
    .concat();
    let (status, err, _) = start_on(&linux_host(), &args(&watching));
    assert_eq!(status, code(0), "{err}");
    assert!(!err.contains("--watch-dir given"), "{err}");

    let (status, err, _) = start_on(
        &linux_host(),
        &["--state-root", &state.display().to_string()],
    );
    assert_eq!(status, code(0), "{err}");
    assert!(!err.contains("--watch-dir given"), "no containers: {err}");
}
