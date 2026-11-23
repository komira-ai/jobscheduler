//! The whole host's CPU counters: the busy time of `/proc/stat` (read by the load's own reader,
//! whose cases are in `proc_stat.rs`) and the `some` total of `/proc/pressure/cpu`, which the
//! per-second series reads.

use js_agent::usage::{HostCpu, PROC_CPU_PRESSURE, PROC_STAT, host_cpu};

use crate::support::FakeFiles;

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
