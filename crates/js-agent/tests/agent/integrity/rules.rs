//! The rules of the checks that a change to the code of one rule could break unseen: what
//! counts as a change in the user, group and module lists, what a kernel thread is, what
//! the baseline keeps out, and which namespaces the agent must share with `init`.

use js_agent::integrity::{FakeHost, Integrity, Mount, code};

use super::{JOB_UID, after, beside, process, said, started};

/// Asserts that a user line removed from the database and one changed in place (the same
/// name with another shell) are each a finding under the user code, and so are the same two
/// in the group database under the group code. It catches a comparison that sees only what
/// was added (an account deleted, or a group's members replaced, unseen).
#[test]
fn a_user_or_group_line_removed_or_changed_is_a_finding() {
    const ROOT: &str = "root:x:0:0:root:/root:/bin/sh";
    const GROUP: &str = "root:x:0:";
    for (name, change, expected) in [
        (
            "user-removed",
            (|host: &FakeHost| host.remove_user(ROOT)) as fn(&FakeHost),
            (code::USER_CHANGED, "user-changed: root"),
        ),
        (
            "user-changed",
            |host| {
                host.remove_user(ROOT);
                host.add_user("root:x:0:0:root:/root:/bin/evil");
            },
            (code::USER_CHANGED, "user-changed: root"),
        ),
        (
            "group-removed",
            |host| host.remove_group(GROUP),
            (code::GROUP_CHANGED, "group-changed: root"),
        ),
        (
            "group-changed",
            |host| {
                host.remove_group(GROUP);
                host.add_group("root:x:0:mallory");
            },
            (code::GROUP_CHANGED, "group-changed: root"),
        ),
    ] {
        let host = FakeHost::quiet();
        let mut checks = started(&host, vec![]);
        change(&host);
        let found = after(&mut checks, "r1").unwrap_err();
        assert_eq!(
            said(&found),
            (true, expected.0, expected.1.to_owned()),
            "{name}"
        );
    }
}

/// Asserts that a kernel module that was loaded at the baseline and is gone after the job
/// is a finding, as unloaded, that a module loaded is one as loaded, and that both are
/// named when one job does both. An unload is a finding on purpose: a job that cannot load
/// a module (it holds no capability) has no business seeing one go, and a host where an
/// operator unloads modules while jobs run is one the agent cannot call clean. It catches a
/// comparison that sees only what was loaded.
#[test]
fn a_module_unloaded_is_a_finding() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.unload_module("ext4");
    let found = after(&mut checks, "r1").unwrap_err();
    assert_eq!(
        said(&found),
        (
            true,
            code::MODULE_CHANGED,
            "kernel-module-changed: unloaded ext4".to_owned()
        )
    );

    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.unload_module("ext4");
    host.load_module("evil");
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(
        found.detail(),
        "kernel-module-changed: loaded evil, unloaded ext4"
    );
}

/// Asserts that a kernel thread that started during the job (its parent is `kthreadd`, pid
/// 2, and it belongs to root) is not a finding, while a process whose parent is `init` is.
/// A host starts kernel threads all the time, so a rule that took every parent of low pid
/// for a daemonised child would taint nearly every host after nearly every job. It catches
/// that rule.
#[test]
fn a_new_kernel_thread_is_not_a_daemonised_child() {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.start_process(process(900, 2, 0, 5000, "0::/"));
    assert!(
        after(&mut checks, "r1").is_ok(),
        "a thread of kthreadd is the kernel's"
    );
    host.start_process(process(901, 1, 0, 5001, "0::/system.slice/x.service"));
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::DAEMONISED_CHILD);
    assert!(found.detail().contains("pid 901"), "{found:?}");
    assert!(!found.detail().contains("pid 900"), "{found:?}");
}

/// A mount of `root`, `point` and `source` appears while the container `c2` is on the host
/// beside the finished job `r1`: it is no finding then, and it is a new mount once the
/// container is gone and the mount is not, because the baseline of the clean check of `r1`
/// kept it out.
fn neighbours_mount_stays_out_of_the_baseline(root: &str, point: &str, source: &str) {
    let host = FakeHost::quiet();
    let mut checks = started(&host, vec![]);
    host.mount(Mount {
        root: root.to_owned(),
        point: point.to_owned(),
        fstype: "ext4".to_owned(),
        source: source.to_owned(),
    });
    assert!(beside(&mut checks, "r1", &["c2"]).is_ok());
    let found = after(&mut checks, "r2").unwrap_err();
    assert_eq!(found.code(), code::NEW_MOUNT, "left behind");
}

/// Asserts that a mount whose root names a container that was still on the host beside a
/// finished job is no finding for that job and is kept out of the baseline its clean check
/// takes, so that what is left of it is found once the container is gone. It catches a
/// baseline filter without the root arm, which would absorb the leftover into the baseline
/// for good.
#[test]
fn a_neighbours_mount_is_kept_out_of_the_baseline_by_its_root() {
    neighbours_mount_stays_out_of_the_baseline("/c2/x", "/m", "src");
}

/// The same for the mount point: the filter without the point arm absorbs the leftover.
#[test]
fn a_neighbours_mount_is_kept_out_of_the_baseline_by_its_point() {
    neighbours_mount_stays_out_of_the_baseline("/", "/run/c2", "src");
}

/// The same for the source: the filter without the source arm absorbs the leftover.
#[test]
fn a_neighbours_mount_is_kept_out_of_the_baseline_by_its_source() {
    neighbours_mount_stays_out_of_the_baseline("/", "/m", "/dev/c2");
}

/// Asserts that the checks do not start on an agent that does not share `init`'s pid
/// namespace or its mount namespace (the text names which, and both namespaces), whichever
/// of the agent's and `init`'s differs, and do not start when a link cannot be read to tell;
/// and that they start when the namespaces are the same. It catches an agent that cannot see
/// its jobs' processes or mounts and calls the host clean.
#[test]
fn the_checks_need_the_pid_and_mount_namespaces_of_init() {
    let host = FakeHost::quiet();
    assert!(Integrity::start(Box::new(host.clone()), JOB_UID, vec![]).is_ok());

    for (who, kind) in [("self", "pid"), ("1", "pid"), ("self", "mnt"), ("1", "mnt")] {
        let host = FakeHost::quiet();
        host.move_to_namespace(who, kind, &format!("{kind}:[4026532999]"));
        let why = Integrity::start(Box::new(host), JOB_UID, vec![])
            .err()
            .unwrap();
        assert!(
            why.contains(&format!("the agent is in a {kind} namespace of its own"))
                && why.contains("4026532999")
                && why.contains("4026531836"),
            "{who} {kind}: {why}"
        );

        let host = FakeHost::quiet();
        host.remove_link(&format!("/proc/{who}/ns/{kind}"));
        let why = Integrity::start(Box::new(host), JOB_UID, vec![])
            .err()
            .unwrap();
        assert!(
            why.starts_with("cannot tell whether the agent sees the host:")
                && why.contains(&format!("/proc/{who}/ns/{kind}")),
            "{who} {kind}: {why}"
        );
    }
}
