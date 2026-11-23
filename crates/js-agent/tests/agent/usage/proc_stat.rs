//! The reader of the host's CPU busy time from `/proc/stat` text.

use js_agent::usage::{USEC_PER_TICK, parse_host_busy_usec};

/// A `/proc/stat` as a 6.x kernel writes it, for two CPUs.
const REAL: &str = "cpu  1000 20 300 90000 500 40 60 8 700 9\n\
cpu0 500 10 150 45000 250 20 30 4 350 4\n\
cpu1 500 10 150 45000 250 20 30 4 350 5\n\
intr 123456 0 9 0\nctxt 987654\nbtime 1700000000\nprocesses 4321\n\
procs_running 3\nprocs_blocked 0\nsoftirq 5555 0 1 2 3 4 5 6 7 8 9\n";

/// Asserts the busy time of a real `/proc/stat`: user, nice, system, irq, softirq and steal
/// of the `cpu` line (1000 + 20 + 300 + 40 + 60 + 8 = 1428 ticks), at 10 ms a tick, with idle
/// and iowait left out and guest and guest_nice not added again (they are in user and nice).
/// It catches a sum that counts idle or iowait, adds the guest times twice, reads a `cpu0`
/// line, or takes the ticks for microseconds.
#[test]
fn the_busy_time_is_the_cpu_lines_busy_ticks_in_microseconds() {
    assert_eq!(USEC_PER_TICK, 10_000);
    assert_eq!(parse_host_busy_usec(REAL), Some(1428 * 10_000));
    // The per-CPU lines are not the host's, wherever they stand.
    let late = "cpu0 1 1 1 1 1 1 1 1\ncpu  2 0 0 9 9 0 0 0\n";
    assert_eq!(parse_host_busy_usec(late), Some(20_000));
    // Exactly the eight fields of kernels since 2.6.11 are enough.
    assert_eq!(
        parse_host_busy_usec("cpu 1 2 3 4 5 6 7 8\n"),
        Some(27 * 10_000)
    );
}

/// Asserts every form that gives no busy time: no `cpu` line, a line of seven numbers (a
/// kernel before `steal`), a sign, a non-number, a number past `u64`, a sum or a product
/// past `u64`, and an empty text. It catches a missing field read as 0, a lenient number
/// parse and an overflow that wraps.
#[test]
fn a_malformed_proc_stat_gives_no_busy_time() {
    let max = u64::MAX;
    let cases = [
        "intr 1 2\nctxt 3\n".to_owned(),
        "cpu0 1 2 3 4 5 6 7 8\n".to_owned(),
        "cpu 1 2 3 4 5 6 7\n".to_owned(),
        "cpu +1 2 3 4 5 6 7 8\n".to_owned(),
        "cpu 1 2 x 4 5 6 7 8\n".to_owned(),
        "cpu 1 2 3 4 5 6 7 18446744073709551616\n".to_owned(),
        format!("cpu {max} 1 0 0 0 0 0 0\n"),
        format!("cpu {} 0 0 0 0 0 0 0\n", max / 10_000 + 1),
        String::new(),
    ];
    for text in cases {
        assert_eq!(parse_host_busy_usec(&text), None, "{text:?}");
    }
}
