//! The control group of a started container: where the usage of a lease is read from.

use std::path::{Path, PathBuf};

use js_agent::cli_runtime::{CGROUP_ROOT, ProcRead, Settings};
use js_agent::lifecycle::Runtime;

use super::super::scripted::Scripted;
use super::super::{BIN, docker, in_another_pid_namespace, open_with, spec};

/// The settings of an agent whose containers belong in `/sys/fs/cgroup/jobs.slice`.
fn in_jobs_slice() -> Settings {
    Settings::new(Path::new(BIN), None, None)
        .unwrap()
        .with_actions_cgroup(Some(Path::new("/sys/fs/cgroup/jobs.slice")))
        .unwrap()
}

/// Runs one container of `world` with `read` as the agent's `/proc` and returns what
/// `cgroup` says of it.
fn cgroup_with(settings: Settings, read: ProcRead) -> Option<PathBuf> {
    let world: Scripted = docker();
    let mut rt =
        open_with(&world, "linux", settings).with_proc_files(read, in_another_pid_namespace);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    rt.cgroup(&id)
}

fn systemd_table(_path: &str) -> Option<String> {
    Some("0::/jobs.slice/docker-abc.scope\n".to_owned())
}

/// Asserts that the control group of a container is the unified line of its process's
/// `/proc/<pid>/cgroup` below the cgroup mount, picked from a table that also holds
/// version 1 lines, and that the `/proc` file asked for is the one of the pid the engine
/// printed. It catches a control group taken from the wrong line (a version 1 controller
/// path) or the wrong process, which would read another lease's usage.
#[test]
fn the_control_group_is_the_unified_line_of_the_containers_process() {
    fn mixed(path: &str) -> Option<String> {
        assert_eq!(path, "/proc/4000/cgroup");
        Some("12:memory:/old\n0::/jobs.slice/docker-abc.scope\n1:name=x:/y\n".to_owned())
    }
    let found = cgroup_with(in_jobs_slice(), mixed);
    let expected = Path::new(CGROUP_ROOT).join("jobs.slice/docker-abc.scope");
    assert_eq!(found, Some(expected));
}

/// Asserts that no control group is reported without `--actions-cgroup`, and none for a
/// table the agent cannot use: no file, no unified line, a path with `..`, the root
/// control group, and a container outside the actions control group or that is the actions
/// control group itself. It catches a lease sampled from a directory that its node's
/// counter does not hold, which would make the leases add up to more than the node.
#[test]
fn a_control_group_outside_the_actions_group_is_not_reported() {
    let plain = Settings::new(Path::new(BIN), None, None).unwrap();
    assert_eq!(cgroup_with(plain, systemd_table), None, "no flag");
    let tables: [(&str, ProcRead); 6] = [
        ("no file", |_| None),
        ("v1 only", |_| Some("12:memory:/old\n".to_owned())),
        ("dotdot", |_| Some("0::/jobs.slice/../x\n".to_owned())),
        ("root", |_| Some("0::/\n".to_owned())),
        ("outside", |_| {
            Some("0::/other.slice/docker-abc.scope\n".to_owned())
        }),
        ("itself", |_| Some("0::/jobs.slice\n".to_owned())),
    ];
    for (name, read) in tables {
        assert_eq!(cgroup_with(in_jobs_slice(), read), None, "{name}");
    }
    assert_eq!(
        cgroup_with(in_jobs_slice(), |_| Some("0::/jobs.slicex/a\n".to_owned())),
        None
    );
}

/// Asserts that a container that is over, or whose look cannot be made, has no control
/// group, rather than a stale one. It catches a lease sampled from the directory of a
/// finished container.
#[test]
fn an_ended_container_has_no_control_group() {
    let world = docker();
    let mut rt = open_with(&world, "linux", in_jobs_slice())
        .with_proc_files(systemd_table, in_another_pid_namespace);
    let id = rt.create_and_start(&spec("r1")).unwrap();
    world.exit("r1", 0);
    assert_eq!(rt.cgroup(&id), None);
    world.vanish("r1");
    assert_eq!(rt.cgroup(&id), None);
    let id = rt.create_and_start(&spec("r2")).unwrap();
    world.fail_pid();
    assert_eq!(rt.cgroup(&id), None);
    world.pid_says("0 running\n");
    assert_eq!(rt.cgroup(&id), None);
    world.pid_says("4000 exited\n");
    assert_eq!(rt.cgroup(&id), None);
}

/// Asserts that `create` carries `--cgroup-parent` with the configured value, and that the
/// start asks for the control group once. It catches a runtime that checks the flag and
/// does not pass it on.
#[test]
fn create_passes_the_cgroup_parent() {
    let world = docker();
    let mut rt = open_with(&world, "linux", in_jobs_slice());
    let id = rt.create_and_start(&spec("r1")).unwrap();
    let args = world.create_of(&id.0);
    let at = args
        .iter()
        .position(|arg| arg == "--cgroup-parent")
        .unwrap();
    assert_eq!(args[at + 1], "jobs.slice");
}

/// Asserts that the direct runner's placement, `<lease>/ctr` below the actions group, is
/// reported as it is (it is a group below the actions group), while a container whose
/// control group only looks like a lease's, with a `..` in it or in a sibling of the actions
/// group, is not. It pins the runtime's rule to the shared one, so a runtime that places its
/// containers as the direct runner does is sampled and a foreign group is still refused.
#[test]
fn a_lease_ctr_group_is_reported_and_a_lookalike_is_not() {
    let found = cgroup_with(in_jobs_slice(), |_| {
        Some("0::/jobs.slice/lease-1-2/ctr\n".to_owned())
    });
    let expected = Path::new(CGROUP_ROOT).join("jobs.slice/lease-1-2/ctr");
    assert_eq!(found, Some(expected));
    let lookalikes: [(&str, ProcRead); 3] = [
        ("sibling name", |_| {
            Some("0::/jobs.slice-old/lease-1-2/ctr\n".to_owned())
        }),
        ("another tree", |_| {
            Some("0::/other.slice/jobs.slice/lease-1-2/ctr\n".to_owned())
        }),
        ("dotdot", |_| {
            Some("0::/jobs.slice/lease-1-2/../../ctr\n".to_owned())
        }),
    ];
    for (name, read) in lookalikes {
        assert_eq!(cgroup_with(in_jobs_slice(), read), None, "{name}");
    }
}
