//! What a container or a cgroup lets the agent use, read from recorded files: a CPU quota,
//! a memory limit, the CPUs the process may run on. The host's totals are the ceiling,
//! these only lower them, and the flags lower them further.

use js_agent::detect;

use crate::support::{FakeFiles, Host, linux_host, mac_host, run_scripted, welcome};

/// A Linux host (8 CPUs, 32 GiB) whose process runs in the cgroup `path` with the given
/// files, each named relative to `/sys/fs/cgroup` or by its `/proc` path.
fn in_cgroup(path: &str, files: &[(&str, &str)]) -> Host {
    let mut host = linux_host();
    let mut recorded: FakeFiles = host
        .files
        .with("/proc/self/cgroup", &format!("0::{path}\n"));
    for (name, text) in files {
        let full = match name.starts_with("/proc") {
            true => (*name).to_owned(),
            false => format!("/sys/fs/cgroup{name}"),
        };
        recorded = recorded.with(&full, text);
    }
    host.files = recorded;
    host
}

fn found(host: &Host) -> (u64, u64) {
    let found = detect(&host.platform, &host.commands, &host.files).unwrap();
    (found.cpu_millis, found.memory_bytes)
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Asserts that a CPU quota and a memory limit of the agent's own cgroup lower what the
/// host offers: `cpu.max` of `200000 100000` is two CPUs, `memory.max` is taken as bytes,
/// and the host totals (8 CPUs, 32 GiB) are what is offered without them. It catches an
/// agent in a container that offers the whole node.
#[test]
fn a_cgroup_quota_and_memory_limit_lower_the_offer() {
    assert_eq!(found(&linux_host()), (8000, 32 * GIB));
    let host = in_cgroup(
        "/",
        &[
            ("/cpu.max", "200000 100000\n"),
            ("/memory.max", "4294967296\n"),
        ],
    );
    assert_eq!(found(&host), (2000, 4 * GIB));
    let fractional = in_cgroup("/", &[("/cpu.max", "150000 100000\n")]);
    assert_eq!(found(&fractional), (1500, 32 * GIB));
}

/// Asserts that a limit of "no limit" (`max`) lowers nothing, and that a limit above the
/// host's totals never raises them.
#[test]
fn no_limit_and_a_limit_above_the_host_change_nothing() {
    let unlimited = in_cgroup(
        "/",
        &[("/cpu.max", "max 100000\n"), ("/memory.max", "max\n")],
    );
    assert_eq!(found(&unlimited), (8000, 32 * GIB));
    let huge = in_cgroup(
        "/",
        &[
            ("/cpu.max", "6400000 100000\n"),
            ("/memory.max", "274877906944\n"),
        ],
    );
    assert_eq!(found(&huge), (8000, 32 * GIB));
}

/// Asserts that a limit set on a parent of the agent's cgroup binds the agent too, and
/// the lowest limit on the way up to the root wins: the container's own cgroup says four
/// CPUs and no memory limit, its parent 1.5 CPUs and 1 GiB, its grandparent 2 GiB.
#[test]
fn the_lowest_limit_on_the_way_to_the_root_wins() {
    let host = in_cgroup(
        "/kube/pod/ctr",
        &[
            ("/kube/pod/ctr/cpu.max", "400000 100000\n"),
            ("/kube/pod/ctr/memory.max", "max\n"),
            ("/kube/pod/cpu.max", "150000 100000\n"),
            ("/kube/pod/memory.max", "1073741824\n"),
            ("/kube/memory.max", "2147483648\n"),
        ],
    );
    assert_eq!(found(&host), (1500, GIB));
}

/// Asserts that files the agent cannot make sense of are not limits: a cgroup v1 line, an
/// unreadable `/proc/self/cgroup`, a zero period, text where a number should be, and an
/// empty file offer the host's totals instead of ending the start.
#[test]
fn unreadable_or_unknown_limits_are_ignored() {
    let mut v1 = linux_host();
    v1.files = v1
        .files
        .with("/proc/self/cgroup", "12:cpu,cpuacct:/docker/abc\n")
        .with("/sys/fs/cgroup/cpu.max", "100000 100000\n");
    assert_eq!(found(&v1), (8000, 32 * GIB));

    assert_eq!(found(&linux_host()), (8000, 32 * GIB));

    let odd = in_cgroup(
        "/",
        &[("/cpu.max", "100000 0\n"), ("/memory.max", "lots\n")],
    );
    assert_eq!(found(&odd), (8000, 32 * GIB));
    let words = in_cgroup("/", &[("/cpu.max", "many few\n")]);
    assert_eq!(found(&words).0, 8000);
    let short = in_cgroup("/", &[("/cpu.max", "100000\n")]);
    assert_eq!(found(&short).0, 8000);
    let empty = in_cgroup("/", &[("/cpu.max", ""), ("/memory.max", "")]);
    assert_eq!(found(&empty), (8000, 32 * GIB));
}

/// The `/proc/self/status` of a process pinned to the CPUs `list`.
fn status(list: &str) -> String {
    format!("Name:\tjs-agent\nCpus_allowed:\tff\nCpus_allowed_list:\t{list}\nMems_allowed:\t1\n")
}

/// Asserts that the CPUs the process may run on (`taskset`, a cpuset) lower the CPU count:
/// ranges and single CPUs are counted, the lower of the affinity and the quota wins, and
/// an affinity above the host's count changes nothing.
#[test]
fn the_allowed_cpu_list_lowers_the_cpu_count() {
    let pin = |list: &str, extra: &[(&str, &str)]| {
        let mut files = vec![("/proc/self/status", status(list))];
        files.extend(extra.iter().map(|(n, t)| (*n, (*t).to_owned())));
        let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
        found(&in_cgroup("/", &files)).0
    };
    assert_eq!(pin("0-1,4", &[]), 3000);
    assert_eq!(pin("5", &[]), 1000);
    assert_eq!(pin("0-7", &[]), 8000);
    assert_eq!(pin("0-63", &[]), 8000);
    assert_eq!(pin("0-3", &[("/cpu.max", "200000 100000")]), 2000);
    assert_eq!(pin("0", &[("/cpu.max", "200000 100000")]), 1000);
}

/// Asserts that an allowed list that is not a list (a reversed range, a word, an empty
/// value, a missing line) is no limit, and that a list of ranges that would overflow the
/// count is read without a panic and lowers nothing.
#[test]
fn a_malformed_allowed_cpu_list_is_ignored() {
    let all = format!("0-{max},0-{max}", max = u64::MAX);
    for list in ["3-1", "x", "", "1-", "0,,2", all.as_str()] {
        let mut host = linux_host();
        host.files = host
            .files
            .with("/proc/self/cgroup", "0::/\n")
            .with("/proc/self/status", &status(list));
        assert_eq!(found(&host).0, 8000, "{list:?}");
    }
    let mut none = linux_host();
    none.files = none.files.with("/proc/self/status", "Name:\tjs-agent\n");
    assert_eq!(found(&none).0, 8000);
}

/// Asserts that a Mac offers its totals whatever Linux files exist: the cgroup files are
/// never read there.
#[test]
fn a_mac_offers_its_totals() {
    let mut host = mac_host();
    host.files = host
        .files
        .with("/proc/self/cgroup", "0::/\n")
        .with("/sys/fs/cgroup/cpu.max", "100000 100000\n")
        .with("/sys/fs/cgroup/memory.max", "1048576\n")
        .with("/proc/self/status", &status("0"));
    assert_eq!(found(&host), (10_000, 16 * GIB));
}

/// Asserts that the flags still only lower: with a two-CPU quota in force, the hello
/// offers 2000 millis, `--cpu-millis 1000` lowers it to 1000, and `--cpu-millis 4000`,
/// which the host could give but the container cannot, ends the start naming the flag and
/// the figure detected inside the container.
#[test]
fn flags_cap_the_container_limit_and_never_raise_it() {
    let host = in_cgroup("/", &[("/cpu.max", "200000 100000\n")]);
    let offered = |extra: &[&str]| {
        let replies = vec![welcome(1)];
        let mut args = vec!["--host-id", "boxed"];
        args.extend_from_slice(extra);
        run_scripted(&args, &host, replies, 1)
    };
    let (ran, sent, _) = offered(&[]);
    assert_eq!(ran.status(), crate::support::code(0), "{}", ran.err);
    assert_eq!(sent.borrow()[0].1["shape"]["cpu_millis"], 2000);
    let (_, sent, _) = offered(&["--cpu-millis", "1000"]);
    assert_eq!(sent.borrow()[0].1["shape"]["cpu_millis"], 1000);
    let (ran, sent, _) = offered(&["--cpu-millis", "4000"]);
    assert_eq!(ran.status(), crate::support::code(2));
    assert!(
        ran.err
            .contains("--cpu-millis: 4000 is above the 2000 detected"),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty());
}
