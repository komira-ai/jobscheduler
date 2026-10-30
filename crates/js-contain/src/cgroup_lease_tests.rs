//! Tests of [`LeaseCgroup`] against a plain directory standing in for cgroupfs, of the
//! swap watch's sampling loop, and of [`memory_max`] against the copies it replaces.

use super::*;

/// A fresh directory standing in for the cgroup mount.
fn mount(name: &str) -> crate::test_dir::TestDir {
    crate::unit_dir(&format!("cgroup-lease-{name}"))
}

/// A lease cgroup `/actions/lease` under `root`, with its directory made.
fn lease_in(root: &Path) -> LeaseCgroup {
    let lease = LeaseCgroup::at(root, "/actions", "lease");
    std::fs::create_dir_all(lease.dir()).expect("mkdir");
    lease
}

fn put(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).expect("write");
}

/// What the kernel writes to `memory.events` for the given counters.
fn events_text(max: u64, oom: u64, oom_kill: u64) -> String {
    format!("low 0\nhigh 0\nmax {max}\noom {oom}\noom_kill {oom_kill}\n")
}

/// Catches the name and directory accessors disagreeing about the parent: the bundle's
/// `cgroupsPath` is made from the name, the files are read from the directory, and a
/// trailing slash on the parent must not double.
#[test]
fn the_name_is_relative_to_the_mount_and_the_dir_is_under_it() {
    let root = mount("name");
    let lease = LeaseCgroup::at(&root, "/actions/", "lease");
    assert_eq!(lease.name(), "/actions/lease");
    assert_eq!(lease.dir(), root.join("actions/lease"));
}

/// Catches `kill` writing to the wrong file or a value the kernel does not take, and a
/// write error that does not name the file (the operator has to find which write failed).
#[test]
fn kill_writes_one_to_cgroup_kill_and_a_failed_write_names_the_file() {
    let root = mount("kill");
    let lease = lease_in(&root);
    lease.kill().expect("kill");
    assert_eq!(
        std::fs::read_to_string(lease.dir().join("cgroup.kill")).expect("read"),
        "1"
    );
    let gone = LeaseCgroup::at(&root, "/actions", "never-made");
    let error = gone.kill().expect_err("no such cgroup");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("write cgroup.kill"), "{error}");
}

/// Catches `events` reading another file, an unreadable file passed off as zero counts,
/// and a missing file's error losing its kind.
#[test]
fn events_are_read_from_memory_events_or_fail_naming_it() {
    let root = mount("events");
    let lease = lease_in(&root);
    let error = lease.events().expect_err("no file");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("read memory.events"), "{error}");
    put(lease.dir(), "memory.events", &events_text(4, 2, 1));
    assert_eq!(
        lease.events().expect("read"),
        MemoryEvents {
            max: 4,
            oom: 2,
            oom_kill: 1
        }
    );
}

/// Catches the peak read from another file, a garbled peak taken for zero, and the
/// peak's error losing its kind.
#[test]
fn the_peak_is_memory_peak_and_garbage_is_invalid_data() {
    let root = mount("peak");
    let lease = lease_in(&root);
    assert_eq!(
        lease.peak().expect_err("no file").kind(),
        io::ErrorKind::NotFound
    );
    put(lease.dir(), "memory.peak", "lots\n");
    let error = lease.peak().expect_err("garbled");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("memory.peak"), "{error}");
    put(lease.dir(), "memory.peak", "123456\n");
    assert_eq!(lease.peak().expect("read"), 123_456);
}

/// Catches the backstop count read from the lease's own `memory.events.local` instead of
/// its parent's (which would count the lease's own cap), the `oom` key confused with
/// `oom_kill`, and a missing count taken for zero.
#[test]
fn the_backstop_count_is_the_parents_local_oom_count() {
    let root = mount("parent");
    let lease = lease_in(&root);
    let parent = lease.dir().parent().expect("parent").to_owned();
    assert_eq!(
        lease.parent_ooms().expect_err("no file").kind(),
        io::ErrorKind::NotFound
    );
    put(&parent, "memory.events.local", "low 0\nmax 0\n");
    let error = lease.parent_ooms().expect_err("no oom line");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    put(
        &parent,
        "memory.events.local",
        "low 0\nhigh 0\nmax 5\noom 3\noom_kill 9\n",
    );
    put(lease.dir(), "memory.events.local", "oom 100\n");
    assert_eq!(lease.parent_ooms().expect("read"), 3);
}

/// Catches a lease that killed nothing reported as killed, and a kill reported when the
/// events cannot be read (the error must reach the caller, not be read as "no kill").
#[test]
fn no_oom_kill_is_no_outcome_and_unreadable_events_are_an_error() {
    let root = mount("outcome-none");
    let lease = lease_in(&root);
    assert!(lease.oom_outcome(Some(1 << 30), None).is_err());
    put(lease.dir(), "memory.events", &events_text(7, 0, 0));
    assert_eq!(lease.oom_outcome(Some(1 << 30), None).expect("read"), None);
}

/// Catches a kill at the lease's own cap reported with the wrong limit (the booking
/// instead of the cap), `used` taken from anything but the peak, and an unreadable peak
/// reported as zero use (the cap is the least the lease used).
#[test]
fn a_kill_at_the_leases_own_cap_reports_the_cap_and_the_peak() {
    let root = mount("outcome-own");
    let lease = lease_in(&root);
    put(lease.dir(), "memory.events", &events_text(3, 1, 2));
    let cap = 3 << 30;
    assert_eq!(
        lease.oom_outcome(Some(cap), None).expect("read"),
        Some(OomOutcome::OwnLimit {
            used: cap,
            limit: cap
        })
    );
    put(lease.dir(), "memory.peak", &(cap + 4096).to_string());
    assert_eq!(
        lease.oom_outcome(Some(cap), None).expect("read"),
        Some(OomOutcome::OwnLimit {
            used: cap + 4096,
            limit: cap
        })
    );
    assert_eq!(
        lease.oom_outcome(None, None).expect("read"),
        Some(OomOutcome::OwnLimit {
            used: cap + 4096,
            limit: 0
        })
    );
}

/// Catches a busy-node kill whose message does not say whether the parent's own limit
/// ran the OOM killer (the three cases differ in what the operator should raise), and
/// one that drops the lease's counters.
#[test]
fn a_busy_node_kill_says_what_the_parents_backstop_did() {
    let root = mount("outcome-busy");
    let lease = lease_in(&root);
    let parent = lease.dir().parent().expect("parent").to_owned();
    // Processes were killed, but the lease's own cap never ran the OOM killer.
    put(lease.dir(), "memory.events", &events_text(0, 0, 4));
    let busy = |lease: &LeaseCgroup, before: Option<u64>| match lease
        .oom_outcome(Some(1 << 30), before)
        .expect("read")
    {
        Some(OomOutcome::BusyNode(text)) => text,
        other => panic!("not a busy-node kill: {other:?}"),
    };

    let unreadable = busy(&lease, Some(2));
    assert!(
        unreadable.contains("memory.events.local could not be read"),
        "{unreadable}"
    );
    assert!(
        unreadable.contains("killed 4 process(es)") && unreadable.contains("oom 0, max 0"),
        "{unreadable}"
    );

    put(&parent, "memory.events.local", "oom 5\n");
    let grew = busy(&lease, Some(2));
    assert!(grew.contains("ran 3 time(s) for the actions/"), "{grew}");

    let same = busy(&lease, Some(5));
    assert!(same.contains("did not run for the actions/"), "{same}");

    let unknown = busy(&lease, None);
    assert!(
        unknown.contains("memory.events.local could not be read"),
        "{unknown}"
    );
}

/// Catches `remove` leaving the lease's directory behind, and removal that does not take
/// the cgroups below it (the container's own cgroup is a child of the lease's).
#[test]
fn remove_takes_the_lease_and_the_cgroups_below_it() {
    let root = mount("remove");
    let lease = lease_in(&root);
    std::fs::create_dir_all(lease.dir().join("container/inner")).expect("mkdir");
    lease.remove().expect("remove");
    assert!(!lease.dir().exists());
    // Already gone is fine.
    lease.remove().expect("remove again");
}

/// Catches the sampling loop returning for a lease that has no cap (it has nothing to
/// press against), and a loop that stops sampling at a sample it cannot read.
#[tokio::test(start_paused = true)]
async fn a_lease_without_a_cap_is_never_reported() {
    let root = mount("watch-uncapped");
    let lease = lease_in(&root);
    put(lease.dir(), "memory.events", &events_text(9, 0, 0));
    put(lease.dir(), "memory.current", "1\n");
    put(lease.dir(), "memory.swap.current", "999999999999\n");
    let every = Duration::from_secs(1);
    tokio::select! {
        _ = presses_into_swap(&lease, None, 1, every) => panic!("reported a lease with no cap"),
        () = tokio::time::sleep(every * 20) => {}
    }
}

/// Catches the loop giving up at an unreadable sample, a kill at a sample where the
/// lease was below its cap, and a report that carries any sample but the one that
/// crossed the threshold. Time is paused: the loop sleeps `every` between samples and
/// the writer changes the files between them.
#[tokio::test(start_paused = true)]
async fn the_watch_skips_what_it_cannot_read_and_reports_the_sample_past_the_threshold() {
    let root = mount("watch");
    let lease = lease_in(&root);
    let every = Duration::from_secs(1);
    let threshold = 100;
    let writer = async {
        // Nothing to read at the first sample (t = 1 s): skipped.
        tokio::time::sleep(every + every / 2).await;
        // t = 2 s: below the cap, a little swap that host pressure put there.
        put(lease.dir(), "memory.events", &events_text(0, 0, 0));
        put(lease.dir(), "memory.current", "4096\n");
        put(lease.dir(), "memory.swap.current", "50\n");
        tokio::time::sleep(every).await;
        // t = 3 s: at the cap, and 500 more bytes went to swap.
        put(lease.dir(), "memory.events", &events_text(1, 0, 0));
        put(lease.dir(), "memory.swap.current", "550\n");
        // Far past the sample that crosses the threshold: a watch that has not
        // reported by now never will.
        tokio::time::sleep(every * 20).await;
    };
    let (pressure, own) = tokio::select! {
        done = presses_into_swap(&lease, Some(1 << 30), threshold, every) => done,
        () = writer => panic!("the watch did not report the sample past the threshold"),
    };
    assert_eq!(
        pressure,
        Pressure {
            max: 1,
            current: 4096,
            swap: 550
        }
    );
    // The first 50 bytes were not the lease's own: they came without a `max` event.
    assert_eq!(own, 500);
}

/// Catches [`memory_max`] drifting from the two other places that computed the same cap
/// before there was one function: the native driver's `MemoryPolicy::DEFAULT.limit`
/// (150 % of the booking in 128-bit arithmetic, plus 512 MiB) and the estimator's
/// `memory_max` (the booking plus half of it plus 512 MiB), both written out here as
/// they were. All three agree on every booking, odd ones and ones that overflow
/// included, so a single function changes no cap.
#[test]
fn memory_max_agrees_with_the_copies_it_replaced_on_every_booking() {
    const MIB: u64 = 1 << 20;
    // The native driver's copy: `percent` 150, `headroom_bytes` 512 MiB.
    let native = |memory_bytes: u64| -> Option<u64> {
        (memory_bytes > 0).then(|| {
            u64::try_from(u128::from(memory_bytes) * u128::from(150u64) / 100)
                .unwrap_or(u64::MAX)
                .saturating_add(512 << 20)
        })
    };
    // The estimator's copy.
    let estimator = |booked: u64| -> Option<u64> {
        (booked > 0).then(|| booked.saturating_add(booked / 2).saturating_add(512 * MIB))
    };
    let mut bookings = vec![
        0,
        1,
        2,
        3,
        4095,
        4096,
        MIB,
        MIB + 1,
        1 << 30,
        (1 << 30) + 1,
        4 << 30,
        64 << 30,
        (1 << 62) + 1,
        u64::MAX / 3 * 2,
        u64::MAX / 3 * 2 + 1,
        u64::MAX - 1,
        u64::MAX,
    ];
    // The bookings around the first one whose cap saturates.
    let edge = u64::MAX - (512 << 20);
    bookings.extend([edge / 3 * 2 - 1, edge / 3 * 2, edge / 3 * 2 + 1, edge]);
    for booked in bookings {
        assert_eq!(
            memory_max(booked),
            native(booked),
            "native, booked {booked}"
        );
        assert_eq!(
            memory_max(booked),
            estimator(booked),
            "estimator, booked {booked}"
        );
    }
    // The cases each copy's own test pinned.
    assert_eq!(memory_max(1 << 30), Some((3 << 29) + (512 << 20)));
    assert_eq!(memory_max(4 << 30), Some((6 << 30) + (1 << 29)));
    assert_eq!(memory_max(0), None);
    assert_eq!(memory_max(u64::MAX), Some(u64::MAX));
}

/// A booking of `cpu_millis` and `memory_bytes`, no GPUs.
fn booking(cpu_millis: u64, memory_bytes: u64) -> Resources {
    Resources::new(cpu_millis, memory_bytes, 0, None).expect("a booking without GPUs")
}

/// What a lease cgroup made under `policy` for `resources` holds, file by file.
fn made(
    name: &str,
    resources: &Resources,
    policy: LeasePolicy,
) -> impl Fn(&str) -> Option<String> + use<> {
    let root = mount(name);
    std::fs::create_dir(root.join("actions")).expect("mkdir");
    let lease = LeaseCgroup::at(&root, "/actions", "lease-1-1");
    lease.create_with(resources, None, policy).expect("created");
    let dir = lease.dir().to_owned();
    // The directory is removed with the guard; read every file the test asks for now.
    let files: Vec<(String, Option<String>)> = [
        "memory.max",
        "memory.swap.max",
        "memory.high",
        "memory.oom.group",
        "cpu.max",
        "cpu.weight",
        "cgroup.subtree_control",
    ]
    .iter()
    .map(|f| ((*f).to_owned(), std::fs::read_to_string(dir.join(f)).ok()))
    .collect();
    drop(root);
    move |file: &str| {
        files
            .iter()
            .find(|(f, _)| f == file)
            .and_then(|(_, text)| text.clone())
    }
}

/// Catches a Hard lease whose memory cap is not the booking (the booking times 1.5 plus
/// 512 MiB lets a job use what it did not book, which podman's `--memory` never did), or
/// whose swap is not 0 (a job pressed at its cap would be moved to swap, which podman's
/// `--memory-swap` equal to `--memory` forbids), or that lost the OOM group.
#[test]
fn a_hard_lease_caps_memory_at_the_booking_and_forbids_swap() {
    let read = made("hard-memory", &booking(2000, 3 << 30), LeasePolicy::Hard);
    assert_eq!(read("memory.max"), Some((3u64 << 30).to_string()));
    assert_eq!(read("memory.swap.max").as_deref(), Some("0"));
    assert_eq!(read("memory.oom.group").as_deref(), Some("1"));
    assert_eq!(read("memory.high"), None);
}

/// Catches a Hard lease without a hard CPU cap (a `cpu.weight` alone lets a job use idle
/// cores, which `--cpus` never did) and a cap that is not `cpu_millis` thousandths of a
/// core per 100 ms period: 2500 booked is `250000 100000`, as `--cpus 2.500` makes.
#[test]
fn a_hard_lease_caps_cpu_at_the_booked_cores() {
    let read = made("hard-cpu", &booking(2500, 1 << 30), LeasePolicy::Hard);
    assert_eq!(read("cpu.max").as_deref(), Some("250000 100000"));
    assert_eq!(cpu_max(1500).as_deref(), Some("150000 100000"));
    assert_eq!(
        cpu_max(1).as_deref(),
        Some("1000 100000"),
        "the kernel's least quota"
    );
    assert_eq!(cpu_max(0), None);
    assert_eq!(hard_memory_max(1 << 30), Some(1 << 30));
    assert_eq!(hard_memory_max(0), None);
}

/// Catches a Hard lease that booked nothing getting a limit of 0 (a `memory.max` of 0
/// kills every process at once) and a swap limit with no memory limit beside it.
#[test]
fn a_hard_lease_that_booked_nothing_has_no_limits() {
    let read = made("hard-none", &Resources::default(), LeasePolicy::Hard);
    for file in ["memory.max", "memory.swap.max", "cpu.max", "cpu.weight"] {
        assert_eq!(read(file), None, "{file}");
    }
    assert_eq!(read("memory.oom.group").as_deref(), Some("1"));
}

/// Catches the build-host policy (kbf's) changing with the new one: `create` and
/// `create_with(BuildHost)` write 1.5 x + 512 MiB, a CPU weight and no swap or `cpu.max`.
#[test]
fn the_build_host_policy_is_unchanged_and_the_default() {
    assert_eq!(LeasePolicy::default(), LeasePolicy::BuildHost);
    let read = made(
        "build-host",
        &booking(2000, 1 << 30),
        LeasePolicy::BuildHost,
    );
    assert_eq!(
        read("memory.max"),
        Some(((3u64 << 29) + (512 << 20)).to_string())
    );
    assert_eq!(read("cpu.weight").as_deref(), Some("200"));
    assert_eq!(read("memory.swap.max"), None);
    assert_eq!(read("cpu.max"), None);
}
