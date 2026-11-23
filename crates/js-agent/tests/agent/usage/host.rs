//! The whole host's CPU counters: the busy time of `/proc/stat` and the `some` total of
//! `/proc/pressure/cpu`, which the per-second series reads; and the whole host's memory in use,
//! from `/proc/meminfo`.

use js_agent::usage::{
    HostCpu, PROC_CPU_PRESSURE, PROC_MEMINFO, PROC_STAT, host_cpu, host_memory_used,
    parse_meminfo_used_bytes, parse_proc_stat_busy_usec,
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

/// Asserts the memory in use read from `/proc/meminfo`: `MemTotal` less `MemAvailable`, in
/// kibibytes, as bytes, wherever the two lines are; 0 when `MemAvailable` is the larger; and
/// none when either line is missing or not a number. Catches `MemFree` read for `MemAvailable`
/// (which counts the page cache the kernel can drop as in use), kibibytes taken for bytes, a
/// raw subtraction that wraps, and a missing line read as zero.
#[test]
fn meminfo_used_table() {
    let cases: [(&str, Option<u64>); 8] = [
        (
            "MemTotal:       263303168 kB\nMemFree:         1048576 kB\n\
             MemAvailable:   174063616 kB\nBuffers:          2048 kB\n",
            Some((263_303_168 - 174_063_616) * 1024),
        ),
        (
            "MemAvailable: 1000 kB\nMemTotal: 3000 kB\n",
            Some(2000 * 1024),
        ),
        ("MemTotal: 1000 kB\nMemAvailable: 3000 kB\n", Some(0)),
        ("MemTotal: 1000 kB\nMemFree: 10 kB\n", None),
        ("MemFree: 10 kB\nMemAvailable: 10 kB\n", None),
        ("MemTotal: x kB\nMemAvailable: 10 kB\n", None),
        ("", None),
        (
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 0 kB\n",
            Some(u64::MAX),
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse_meminfo_used_bytes(text), want, "{text:?}");
    }
}

/// Asserts the host's memory in use from its files: read from `/proc/meminfo` when it can be
/// read, and none when it cannot. Catches a zero invented for an unreadable file.
#[test]
fn the_hosts_memory_in_use_is_read_from_meminfo() {
    let files = FakeFiles::default().with(
        PROC_MEMINFO,
        "MemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n",
    );
    assert_eq!(host_memory_used(&files), Some(8 << 30));
    assert_eq!(host_memory_used(&FakeFiles::default()), None);
    assert_eq!(PROC_MEMINFO, "/proc/meminfo");
}
