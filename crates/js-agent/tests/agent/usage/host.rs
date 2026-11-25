//! The whole host's CPU counters: the busy time of `/proc/stat` and the `some` total of
//! `/proc/pressure/cpu`, which the per-second series reads.

use js_agent::usage::{
    HostCpu, PROC_CPU_PRESSURE, PROC_MEMINFO, PROC_STAT, host_cpu, host_nonfile,
    parse_meminfo_nonfile_bytes, parse_proc_stat_busy_usec,
};

use crate::support::FakeFiles;

/// Asserts the busy time read from `/proc/stat`: user, nice, system, irq, softirq and steal
/// ticks of the first line, at 10 ms a tick; idle, iowait, guest and guest_nice left out; and
/// every malformed form refused. Catches idle or iowait counted as busy, guest counted twice,
/// a per-CPU line read for the host, and a short line read with missing fields as zero.
#[test]
fn proc_stat_busy_table() {
    let cases: [(&str, Option<u64>); 9] = [
        // user 1, nice 2, system 3, idle 400, iowait 50, irq 6, softirq 7, steal 8, guest 90,
        // guest_nice 100: busy 27 ticks.
        (
            "cpu  1 2 3 400 50 6 7 8 90 100\ncpu0 1 1 1 1 1 1 1 1 1 1\n",
            Some(270_000),
        ),
        // A kernel that prints only the first eight fields.
        ("cpu 1 2 3 400 50 6 7 8\n", Some(270_000)),
        ("cpu 1 2 3 400 50 6 7\n", None),
        ("cpu 1 2 3 400 x 6 7 8\n", None),
        ("cpu0 1 2 3 400 50 6 7 8\n", None),
        ("intr 1 2 3 4 5 6 7 8\n", None),
        ("\ncpu 1 2 3 4 5 6 7 8\n", None),
        ("", None),
        ("cpu 18446744073709551615 1 0 0 0 0 0 0\n", Some(u64::MAX)),
    ];
    for (text, want) in cases {
        assert_eq!(parse_proc_stat_busy_usec(text), want, "{text:?}");
    }
}

/// Asserts the host's counters from its files: both when both can be read, busy time alone
/// when `/proc/pressure/cpu` cannot be read or has no `some` total, and none when
/// `/proc/stat` cannot be read or has no `cpu` line, whatever the pressure. Catches a zero
/// invented for an unreadable counter and a missing pressure file that costs the busy time.
#[test]
fn the_hosts_counters_are_its_busy_time_and_its_pressure() {
    let stat = "cpu  100 0 50 9000 7 0 0 0 0 0\n";
    let pressure = "some avg10=0.00 avg60=0.00 avg300=0.00 total=4321\n\
                    full avg10=0.00 avg60=0.00 avg300=0.00 total=9\n";
    let both = FakeFiles::default()
        .with(PROC_STAT, stat)
        .with(PROC_CPU_PRESSURE, pressure);
    assert_eq!(
        host_cpu(&both),
        Some(HostCpu {
            busy_usec: 1_500_000,
            pressure_some_usec: Some(4321),
        })
    );
    let busy_alone = Some(HostCpu {
        busy_usec: 1_500_000,
        pressure_some_usec: None,
    });
    assert_eq!(
        host_cpu(&FakeFiles::default().with(PROC_STAT, stat)),
        busy_alone
    );
    let no_total = FakeFiles::default()
        .with(PROC_STAT, stat)
        .with(PROC_CPU_PRESSURE, "full total=9\n");
    assert_eq!(host_cpu(&no_total), busy_alone);
    assert_eq!(
        host_cpu(&FakeFiles::default().with(PROC_CPU_PRESSURE, pressure)),
        None
    );
    let no_cpu_line = FakeFiles::default()
        .with(PROC_STAT, "intr 1\n")
        .with(PROC_CPU_PRESSURE, pressure);
    assert_eq!(host_cpu(&no_cpu_line), None);
    assert_eq!(
        (PROC_STAT, PROC_CPU_PRESSURE),
        ("/proc/stat", "/proc/pressure/cpu")
    );
}

/// A typical `/proc/meminfo` of a host: 16 GiB total, 4 GiB free, 1 GiB of buffers, 6 GiB cached
/// of which 2 GiB is shared memory. Cache that is not shared is 5 GiB, so the non-file memory is
/// 16 - 4 - 5 = 7 GiB.
const MEMINFO: &str = "MemTotal:       16777216 kB\n\
                       MemFree:         4194304 kB\n\
                       MemAvailable:    9000000 kB\n\
                       Buffers:         1048576 kB\n\
                       Cached:          6291456 kB\n\
                       SwapCached:            0 kB\n\
                       Shmem:           2097152 kB\n";

/// Asserts the non-file memory read from `/proc/meminfo`: total less free less the cache that
/// is not shared memory, in bytes, in any line order; shared memory counted back in; never
/// below 0; a missing line or a non-number refused; and an overflow saturating. Catches shared
/// memory counted as droppable cache, `SwapCached` read as `Cached`, a negative result wrapped
/// round, and a missing line read as 0.
#[test]
fn meminfo_nonfile_table() {
    let gib = 1024_u64 * 1024 * 1024;
    assert_eq!(parse_meminfo_nonfile_bytes(MEMINFO), Some(7 * gib));
    let reversed = MEMINFO.lines().rev().collect::<Vec<_>>().join("\n");
    assert_eq!(parse_meminfo_nonfile_bytes(&reversed), Some(7 * gib));
    // No shared memory: all 7 GiB of cache is droppable.
    let no_shmem = MEMINFO.replace("2097152 kB", "0 kB");
    assert_eq!(parse_meminfo_nonfile_bytes(&no_shmem), Some(5 * gib));
    // More cache than total less free: nothing is left, and it is not negative.
    let cache_heavy = "MemTotal: 10 kB\nMemFree: 4 kB\nBuffers: 5 kB\nCached: 5 kB\nShmem: 0 kB\n";
    assert_eq!(parse_meminfo_nonfile_bytes(cache_heavy), Some(0));
    // Shmem larger than the cache cannot make the cache negative.
    let shmem_heavy = "MemTotal: 10 kB\nMemFree: 4 kB\nBuffers: 1 kB\nCached: 1 kB\nShmem: 9 kB\n";
    assert_eq!(parse_meminfo_nonfile_bytes(shmem_heavy), Some(6 * 1024));
    for key in ["MemTotal:", "MemFree:", "Buffers:", "Cached:", "Shmem:"] {
        let without = MEMINFO
            .lines()
            .filter(|line| !line.starts_with(key))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_meminfo_nonfile_bytes(&without), None, "{key}");
        let garbled = MEMINFO.replace(key, &format!("{key} x"));
        assert_eq!(parse_meminfo_nonfile_bytes(&garbled), None, "{key}");
    }
    assert_eq!(parse_meminfo_nonfile_bytes(""), None);
    let huge = "MemTotal: 18446744073709551615 kB\nMemFree: 0 kB\nBuffers: 0 kB\n\
                Cached: 0 kB\nShmem: 0 kB\n";
    assert_eq!(parse_meminfo_nonfile_bytes(huge), Some(u64::MAX));
}

/// Asserts the host's non-file memory from its files: the number when `/proc/meminfo` reads,
/// and none when it cannot be read or lacks a line, never a zero. Catches an unreadable file
/// that reads as an idle host, and a wrong path.
#[test]
fn the_hosts_nonfile_memory_is_read_from_proc_meminfo() {
    let files = FakeFiles::default().with(PROC_MEMINFO, MEMINFO);
    assert_eq!(host_nonfile(&files), Some(7 * 1024 * 1024 * 1024));
    assert_eq!(host_nonfile(&FakeFiles::default()), None);
    let short = FakeFiles::default().with(PROC_MEMINFO, "MemTotal: 1 kB\n");
    assert_eq!(host_nonfile(&short), None);
    assert_eq!(PROC_MEMINFO, "/proc/meminfo");
}
