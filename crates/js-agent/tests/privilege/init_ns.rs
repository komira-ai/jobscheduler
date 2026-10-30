//! Duty 1: the agent reads `/proc/1/ns/{pid,mnt}` at start to prove it sees the host
//! (`integrity::require_host_namespaces`). The kernel allows that read to a process that
//! may ptrace pid 1: root, or a process holding `CAP_SYS_PTRACE`.

use js_agent::integrity::{self, HostView, ProcHost};
use js_agent::{FileReader, SystemFiles};

use crate::harness::{Kv, cap, kv, require_root, run_agent, verdict};

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "init_ns" => {
            let own = std::fs::read_link("/proc/self/ns/pid").is_ok()
                && std::fs::read_link("/proc/self/ns/mnt").is_ok();
            let init =
                integrity::require_host_namespaces(&|path| SystemFiles.link(path), &|path| {
                    SystemFiles.read(path)
                });
            Some(vec![
                kv("own_ns_readable", own),
                kv("init_check", verdict(&init)),
            ])
        }
        "host_inputs" => {
            let host = ProcHost::system();
            Some(vec![
                kv("processes", verdict(&host.processes())),
                kv("mounts", verdict(&host.mounts())),
                kv("users", verdict(&host.users())),
                kv("groups", verdict(&host.groups())),
                kv("modules", verdict(&host.modules())),
                kv(
                    "pid_2_and_init_cgroup",
                    verdict(
                        &SystemFiles
                            .read("/proc/2/stat")
                            .and_then(|_| SystemFiles.read("/proc/1/cgroup")),
                    ),
                ),
            ])
        }
        _ => None,
    }
}

/// A: an agent that is not root and holds no capability cannot read the namespace links of
/// pid 1 (its own it can), so the start check refuses with "cannot tell whether the agent
/// sees the host". It pins why shape A needs something for this duty. It catches a start
/// check that quietly passes when the read fails.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_without_capabilities_cannot_read_the_init_namespace_links() {
    let report = run_agent(&[], "init_ns", &[]);
    report.expect("own_ns_readable", "true");
    report.expect_prefix(
        "init_check",
        "err:cannot tell whether the agent sees the host",
    );
}

/// A: the single capability `CAP_SYS_PTRACE`, held as an ambient capability and nothing
/// else, is enough for the whole start check (both namespace links, pid 2, pid 1's control
/// group). It catches a check that needs more than the one capability the design names.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_with_only_ambient_cap_sys_ptrace_passes_the_init_namespace_check() {
    let report = run_agent(&[cap::SYS_PTRACE], "init_ns", &[]);
    report.expect("init_check", "ok");
}

/// A: the other capabilities a root-less agent might be given for its other duties do not
/// stand in for `CAP_SYS_PTRACE`: `CAP_DAC_READ_SEARCH`, `CAP_DAC_OVERRIDE`, `CAP_SYS_ADMIN`,
/// `CAP_CHOWN` and `CAP_FOWNER` together still fail the check. It catches a design that
/// counts on one of them for this duty.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn no_other_capability_stands_in_for_cap_sys_ptrace_on_the_init_namespace_check() {
    let others = [
        cap::DAC_READ_SEARCH,
        cap::DAC_OVERRIDE,
        cap::SYS_ADMIN,
        cap::CHOWN,
        cap::FOWNER,
    ];
    let report = run_agent(&others, "init_ns", &[]);
    report.expect_prefix(
        "init_check",
        "err:cannot tell whether the agent sees the host",
    );
}

/// A: every other input of the post-job checks (the process table, the mount table, the
/// user and group databases, the module list, pid 2 and pid 1's control group) reads as an
/// ordinary user with no capability, so `CAP_SYS_PTRACE` is the only privilege the checks
/// need besides the watched directories. It catches a duty missing from the table of the
/// design.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_reads_every_other_host_input_without_a_capability() {
    let report = run_agent(&[], "host_inputs", &[]);
    for input in [
        "processes",
        "mounts",
        "users",
        "groups",
        "modules",
        "pid_2_and_init_cgroup",
    ] {
        report.expect(input, "ok");
    }
}

/// B: a root agent passes the start check as it is, with no capability named.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_passes_the_init_namespace_check() {
    require_root();
    let check = integrity::require_host_namespaces(&|path| SystemFiles.link(path), &|path| {
        SystemFiles.read(path)
    });
    assert_eq!(check, Ok(()));
}
