//! The one placement engine, in process: fed the heartbeats' load readings, held to the CPU
//! slack setting and to the pool's memory percent when it places queued jobs. The agent side
//! is assembled from the flags as the server assembles it, on a manual clock; no listener
//! is bound.

use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_registry::HostState;
use js_sched::LoadConfig;

use super::engine_rig::*;
use super::wiring_tests::{scratch, write};
use crate::config::{Cli, Config, CpuSlack};

/// The host the first job lands on over two equal hosts with room, and the host a second
/// lands on after `beats` (each host's two readings, or none) are fed.
fn second_after(
    name: &str,
    beats: impl Fn(&str) -> [Option<js_agent_wire::NodeLoad>; 2],
) -> (String, String) {
    let hosts = ["h1", "h2"];
    let r = rig(name, &pool(2, 8000, ""), &hosts, &[]);
    let first = r.place("job-1", 1_000, 1 << 20).unwrap();
    for at in 0..2 {
        r.advance(5_000);
        for host in hosts {
            let reading = beats(&first)[at].clone().filter(|_| host == first);
            let other = beats("")[at].clone().filter(|_| host != first);
            r.beat(host, reading.or(other));
        }
    }
    let second = r.place("job-2", 1_000, 1 << 20).unwrap();
    (first, second)
}

/// Asserts that the heartbeats' load readings choose among the hosts that fit: the host
/// that carries job 1 reports all 8 of its cores busy and a 15 GiB working set, the other
/// an idle one, and job 2 goes to the idle host; with the same hosts and no load in the
/// heartbeats it goes where booking alone sends it, beside job 1 (the host the job fits most
/// tightly). It catches readings not fed to the engine, and readings fed to the wrong host
/// (which would send it to the busy one).
#[test]
fn load_readings_choose_among_hosts_that_fit() {
    let busy = |first: &str| {
        if first.is_empty() {
            [Some(reading(1, 0, GIB)), Some(reading(2, 0, GIB))]
        } else {
            let ws = 15 * GIB;
            [Some(reading(1, 8_000, ws)), Some(reading(2, 8_000, ws))]
        }
    };
    let (first, second) = second_after("load_chooses", busy);
    assert_ne!(second, first);
    let (first, second) = second_after("load_absent", |_| [None, None]);
    assert_eq!(second, first);
}

/// Asserts that a heartbeat whose load the vet drops (an active file figure past the most
/// the wire takes) still counts as a beat and feeds no reading: the registry keeps every
/// host live with no reading, and job 2 goes where booking alone sends it. The dropped
/// readings say all 8 cores of each host are busy with a working set the engine would
/// take, so had they been fed, the engine would place job 2 nowhere. It catches a beat
/// whose bad load reaches the engine, and a bad load that loses the beat.
#[test]
fn a_heartbeat_with_a_bad_load_still_counts_as_a_beat() {
    let hosts = ["h1", "h2"];
    let r = rig("bad_load", &pool(2, 8000, ""), &hosts, &[]);
    let first = r.place("job-1", 1_000, 1 << 20).unwrap();
    for seq in 1..=4 {
        r.advance(5_000);
        for host in hosts {
            let mut bad = reading(seq, 8_000, GIB);
            bad.active_file_bytes = MAX_MEMORY_BYTES + 1;
            r.beat(host, Some(bad));
        }
    }
    let listed = r.agents.registry.hosts();
    assert!(listed.iter().all(|host| host.state == HostState::Live));
    assert!(listed.iter().all(|host| host.load.is_none()));
    assert_eq!(r.place("job-2", 1_000, 1 << 20).unwrap(), first);
}

/// Asserts the CPU slack through the server's configuration: with no `--cpu-slack` (the
/// engine's own default, read from js-sched and not restated) a 10-core host whose jobs
/// book 10 cores refuses a job of 1.2 cores and takes one of 1 core, and a job that would
/// pass the host's memory is refused whatever the slack; with `--cpu-slack 1.0` the 1-core
/// job is refused; with `--cpu-slack 1.5` five more cores are taken and a sixth is not,
/// which the engine's own rule decides. It catches the setting dropped on either side, a
/// default restated that drifts, and slack applied to memory.
#[test]
fn slack_1_1_lets_cpu_book_ten_percent_over() {
    let full = |name: &str, extra: &[&str]| {
        let r = rig(name, &pool(1, 10_000, ""), &["h1"], extra);
        for n in 1..=10 {
            assert_eq!(
                r.place(&format!("job-{n}"), 1_000, GIB).as_deref(),
                Some("h1")
            );
        }
        r
    };
    let r = full("slack_default", &[]);
    assert_eq!(r.place("job-11", 1_200, 1), None);
    assert_eq!(r.place("job-12", 1, 7 * GIB), None);
    assert_eq!(r.place("job-13", 1_000, 1).as_deref(), Some("h1"));
    assert_eq!(r.place("job-14", 1, 1), None);
    let default = LoadConfig::default().s_cpu_permille;
    assert_eq!(CpuSlack::default().permille(), default);
    let r = full("slack_one", &["--cpu-slack", "1.0"]);
    assert_eq!(r.place("job-11", 1_000, 1), None);
    let r = full("slack_one_half", &["--cpu-slack", "1.5"]);
    for n in 11..=15 {
        assert_eq!(
            r.place(&format!("job-{n}"), 1_000, 1).as_deref(),
            Some("h1")
        );
    }
    assert_eq!(r.place("job-16", 1_000, 1), None);
}

/// What 90 percent of 16 GiB books, rounded down: 15461882265.6 bytes.
const NINETY: u64 = 16 * GIB / 100 * 90 + 16 * GIB % 100 * 90 / 100;

/// Asserts that the pool's `memory_schedulable_percent` is the engine's rule for queued jobs:
/// at 90 a host of 16 GiB takes jobs totalling 90% of it, and one byte more is placed
/// nowhere, while the registry's view of the host still reports the percent and the memory
/// that may be booked. It catches the engine ignoring the key (which places the fourth job)
/// and a percent that is not the registry's.
#[test]
fn memory_percent_cuts_bookable_memory_in_the_engine() {
    let percent = "memory_schedulable_percent = 90";
    let r = rig("percent_engine", &pool(2, 8000, percent), &["h1"], &[]);
    assert_eq!(NINETY, 15_461_882_265);
    let third = NINETY - 10 * GIB;
    for (n, bytes) in [(1, 5 * GIB), (2, 5 * GIB), (3, third)] {
        assert_eq!(
            r.place(&format!("job-{n}"), 1_000, bytes).as_deref(),
            Some("h1")
        );
    }
    assert_eq!(r.place("job-4", 1_000, 1), None);
    let hosts = r.agents.registry.hosts();
    assert_eq!(hosts[0].memory.percent().get(), 90);
    assert_eq!(hosts[0].memory.bookable_bytes(), NINETY);
}

/// Asserts that a host whose hello claims far more memory than the pool lists (512 GiB) is
/// still held to the pool's percent: a job one byte above 90% of the pool's 16 GiB fits no
/// host, and the engine books what 90% allows on it and not a byte more. It catches a host
/// booked by its claim.
#[test]
fn an_agent_claim_never_raises_the_percent() {
    let percent = "memory_schedulable_percent = 90";
    let mut r = rig("percent_claim", &pool(2, 8000, percent), &[], &[]);
    r.hello(&hello_of("big", 64_000, 512 * GIB));
    assert_eq!(r.place("job-above", 1_000, NINETY + 1), None);
    assert_eq!(r.place("job-fits", 1_000, NINETY).as_deref(), Some("big"));
    assert_eq!(r.place("job-over", 1_000, 1), None);
}

/// Asserts that a `memory_schedulable_percent` outside 1 to 100, or not a number, is
/// refused at start with the pool's text, so no such value reaches the engine. It catches a
/// value let through to the engine unchecked.
#[test]
fn memory_percent_values_outside_1_to_100_are_refused() {
    for value in ["0", "101", "-1", "\"ninety\""] {
        let dir = scratch("percent_refused");
        let text = pool(2, 8000, &format!("memory_schedulable_percent = {value}"));
        let flags = [
            ("--listen", "127.0.0.1:0".into()),
            ("--state-file", dir.path().join("state").into_os_string()),
            ("--boot", "initialise".into()),
            (
                "--token-file",
                write(dir.path(), "token", &"t".repeat(40), 0o600),
            ),
            (
                "--agent-pool-config",
                write(dir.path(), "agent-pool.toml", &text, 0o644),
            ),
        ];
        let mut args: Vec<std::ffi::OsString> = vec!["js-lease-server".into()];
        for (flag, value) in flags {
            args.push(flag.into());
            args.push(value);
        }
        let refused = Config::from_cli(Cli::try_parse_from(args).unwrap())
            .err()
            .unwrap();
        let said = refused.to_string();
        assert!(
            said.contains("memory_schedulable_percent"),
            "{value}: {said}"
        );
        if ["0", "101"].contains(&value) {
            let text = "memory_schedulable_percent must be from 1 to 100";
            assert!(said.contains(text), "{value}: {said}");
        }
    }
}
