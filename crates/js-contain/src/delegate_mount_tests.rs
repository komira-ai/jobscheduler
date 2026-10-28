//! The path-taking entry points ([`delegate`], [`adopt`], [`capacity`], [`limit_cpus`],
//! [`enable_cpuset`]) and the real [`Mount`], against a plain directory standing in for
//! the cgroup v2 mount. The steps themselves are tested in `delegate_tests` against a
//! fake that keeps the kernel's rules; these tests show each entry point hands its
//! arguments to the right step through the mount, and that the mount reads and writes
//! the files the steps name.

use super::*;

/// A directory standing in for the mount, with the root's `cgroup.controllers` (what
/// makes it look like cgroup v2) and a delegated unit that offers the three controllers.
fn host(name: &str) -> crate::test_dir::TestDir {
    let dir = crate::unit_dir(&format!("delegate-mount-{name}"));
    put(&dir, "cgroup.controllers", "cpu memory pids\n");
    std::fs::create_dir_all(dir.join("unit")).expect("mkdir");
    put(&dir.join("unit"), "cgroup.controllers", "cpu memory pids\n");
    put(&dir.join("unit"), "cgroup.procs", "");
    dir
}

fn put(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).expect("write");
}

fn text(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).expect("read")
}

/// Catches the real mount reading, writing or making a cgroup anywhere but under its
/// directory, a root path (`/`) mapped to a path that ends in a slash, and a make of a
/// cgroup that exists reported as done instead of `AlreadyExists` (the steps rely on the
/// kind to treat a second start as a no-op).
#[test]
fn the_mount_reads_writes_and_makes_under_its_directory() {
    let dir = host("mount");
    let mount = Mount(&dir);
    assert_eq!(mount.path("/", ""), dir.to_path_buf());
    assert_eq!(
        mount.path("/", "cgroup.controllers"),
        dir.join("cgroup.controllers")
    );
    assert_eq!(mount.path("/unit/", "x"), dir.join("unit/x"));
    assert_eq!(
        mount.read("/", "cgroup.controllers").expect("read"),
        "cpu memory pids\n"
    );
    mount.mkdir("/unit/child").expect("mkdir");
    assert_eq!(
        mount.mkdir("/unit/child").expect_err("twice").kind(),
        io::ErrorKind::AlreadyExists
    );
    mount
        .write("/unit/child", "memory.max", "4096")
        .expect("write");
    assert_eq!(text(&dir.join("unit/child"), "memory.max"), "4096");
    mount
        .writable("/unit/child")
        .expect("this process may write its own directory");
    assert_eq!(
        mount.writable("/unit/missing").expect_err("absent").kind(),
        io::ErrorKind::NotFound
    );
}

/// Catches `delegate` not passing the memory limits through to the files the kernel
/// reads, the processes of the unit not moved into `supervisor/`, and the controllers
/// not enabled in both the unit and `actions/`.
#[test]
fn delegate_sets_up_supervisor_and_actions_through_the_mount() {
    let dir = host("delegate");
    put(&dir.join("unit"), "cgroup.procs", "11\n12\n");
    let delegation = delegate(&dir, "0::/unit\n", 1 << 20, Some(8 << 30)).expect("delegate");
    assert_eq!(
        delegation,
        Delegation {
            root: "/unit".to_owned(),
            actions: "/unit/actions".to_owned(),
            memory_min_capped: None,
        }
    );
    let unit = dir.join("unit");
    assert_eq!(text(&unit.join("supervisor"), "memory.min"), "1048576");
    // The last process the unit listed was written to the leaf's `cgroup.procs`.
    assert_eq!(text(&unit.join("supervisor"), "cgroup.procs"), "12");
    assert_eq!(text(&unit, "cgroup.subtree_control"), "+cpu +memory +pids");
    assert_eq!(
        text(&unit.join("actions"), "cgroup.subtree_control"),
        "+cpu +memory +pids"
    );
    assert_eq!(
        text(&unit.join("actions"), "memory.max"),
        (8u64 << 30).to_string()
    );
}

/// Catches `delegate` on a directory that is not cgroup v2 (no `cgroup.controllers` at
/// the root) going on to make cgroups in it.
#[test]
fn delegate_refuses_a_mount_that_is_not_cgroup_v2() {
    let dir = crate::unit_dir("delegate-mount-v1");
    let error = delegate(&dir, "0::/unit\n", 0, None).expect_err("not v2");
    assert!(matches!(error, DelegateError::NotV2(_)), "{error}");
    assert!(!dir.join("unit").exists());
}

/// Catches `adopt` changing the cgroup it was told to leave alone, a limit passed to the
/// wrong cgroup, and a cgroup that does not enable the controllers accepted.
#[test]
fn adopt_checks_the_cgroup_and_writes_only_the_limit() {
    let dir = host("adopt");
    let actions = dir.join("unit/actions");
    std::fs::create_dir_all(&actions).expect("mkdir");
    let error = adopt(&dir, "/unit/actions", None).expect_err("no controllers file");
    assert!(matches!(error, DelegateError::Io { .. }), "{error}");
    put(&actions, "cgroup.subtree_control", "cpu memory\n");
    let error = adopt(&dir, "/unit/actions", None).expect_err("pids missing");
    assert!(
        matches!(error, DelegateError::Controllers { .. }),
        "{error}"
    );
    put(&actions, "cgroup.subtree_control", "cpu memory pids\n");
    adopt(&dir, "/unit/actions", Some(1 << 30)).expect("adopt");
    assert_eq!(text(&actions, "memory.max"), (1u64 << 30).to_string());
    assert_eq!(
        text(&actions, "cgroup.subtree_control"),
        "cpu memory pids\n"
    );
}

/// Catches the capacity of a cgroup read from the cgroup itself only (a unit's own
/// `MemoryMax=` and `CPUQuota=` are on the cgroups above it), a quota taken as a CPU
/// count without its period, and `max` taken as a limit.
#[test]
fn capacity_is_the_lowest_limit_on_the_path_up() {
    let dir = host("capacity");
    let actions = dir.join("unit/actions");
    std::fs::create_dir_all(&actions).expect("mkdir");
    assert_eq!(
        capacity(&dir, "/unit/actions").expect("read"),
        Capacity::default()
    );
    put(&dir.join("unit"), "memory.max", "17179869184\n");
    put(&dir.join("unit"), "cpu.max", "400000 100000\n");
    put(&actions, "memory.max", "max\n");
    put(&actions, "cpu.max", "max 100000\n");
    assert_eq!(
        capacity(&dir, "/unit/actions").expect("read"),
        Capacity {
            cpus: Some(4),
            memory_bytes: Some(17_179_869_184),
        }
    );
    put(&actions, "memory.max", "8589934592\n");
    assert_eq!(
        capacity(&dir, "/unit/actions").expect("read").memory_bytes,
        Some(8_589_934_592)
    );
}

/// Catches a CPU limit written in CPUs where the kernel wants a quota in microseconds of
/// a period, and a limit written to the wrong cgroup.
#[test]
fn limit_cpus_writes_the_quota_and_period_to_actions() {
    let dir = host("limit");
    let actions = dir.join("unit/actions");
    std::fs::create_dir_all(&actions).expect("mkdir");
    limit_cpus(&dir, "/unit/actions", 3).expect("limit");
    assert_eq!(text(&actions, "cpu.max"), "300000 100000");
    let error = limit_cpus(&dir, "/unit/missing", 3).expect_err("no such cgroup");
    assert!(matches!(error, DelegateError::Io { .. }), "{error}");
}

/// Catches `enable_cpuset` not enabling `cpuset` in both the parent and `actions/`, and
/// not returning the CPUs `actions/` may use.
#[test]
fn enable_cpuset_enables_it_in_the_parent_and_in_actions_and_reads_the_cpus() {
    let dir = host("cpuset");
    let unit = dir.join("unit");
    let actions = unit.join("actions");
    std::fs::create_dir_all(&actions).expect("mkdir");
    put(&unit, "cgroup.controllers", "cpu cpuset memory pids\n");
    put(&unit, "cgroup.subtree_control", "cpu memory pids\n");
    put(&actions, "cgroup.subtree_control", "cpu memory pids\n");
    put(&actions, "cpuset.cpus.effective", "0-3\n");
    let cpus = enable_cpuset(&dir, "/unit/actions").expect("enable");
    assert_eq!(cpus.expect("cpus").len(), 4);
    assert_eq!(text(&unit, "cgroup.subtree_control"), "+cpuset");
    assert_eq!(text(&actions, "cgroup.subtree_control"), "+cpuset");
}
