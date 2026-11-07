//! The one placement engine of `--placement js-sched`, in process: built once and kept
//! in step with the registry's books, fed the heartbeats' load readings, held to the CPU
//! slack setting and to the pool's memory percent; and the default placement, which none
//! of it reaches. The agent side is assembled from the flags as the server assembles it,
//! on a manual clock; no listener is bound.

use js_agent_wire::NodeLoad as Wire;
use js_agent_wire::limits::MAX_MEMORY_BYTES;
use js_lease::Timestamp;
use js_provider::{Pool, RefusalCode, StartAck};
use js_registry::HostState;
use js_sched::LoadConfig;
use js_types::{LeaseId, Resources};

use super::engine_rig::*;
use super::place::Fallbacks;
use super::wiring_tests::{scratch, sharing_spec, write};
use crate::config::{Cli, Config, CpuSlack};

/// Asserts that 200 starts and 200 ends over 20 hosts build the engine once: the placer
/// counts one construction, no resync and no fallback, and every start is placed. It
/// catches an engine built per start (the design's row 21) and any path that rebuilds it
/// on a normal start or end.
#[test]
fn the_engine_is_built_once_across_many_starts() {
    let hosts = names(20);
    let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let mut r = rig(
        "built_once",
        &pool(20, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    assert_eq!(r.counted(), (NONE, 0, 1));
    for sequence in 1..=200 {
        assert!(
            r.start(sharing_spec(sequence)).is_some(),
            "start {sequence}"
        );
        if sequence > 30 {
            r.provider.stop(&LeaseId::new(1, sequence - 30));
        }
        if sequence % 10 == 0 {
            r.advance(5_000);
            hosts.iter().for_each(|host| r.beat(host, None));
        }
    }
    for sequence in 171..=200 {
        r.provider.stop(&LeaseId::new(1, sequence));
    }
    assert!(r.provider.booked().is_empty());
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts that a lease the registry books and the engine does not hold (put back as a
/// restart does, which the engine is told of, then lost by the engine, as a lease it gives
/// up for a silent host is) makes the next start that meets its host rebuild the engine
/// once, counted as `resynced`, and that the next 50 starts and ends add no rebuild. It
/// catches a disagreement left in place (the engine placing past a booking it does not
/// hold) and a rebuild that does not take the registry's books, which would rebuild again
/// at every start.
#[test]
fn a_disagreement_rebuilds_once_and_is_counted() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "resync_once",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    assert!(r.start(sharing_spec(1)).is_some());
    assert_eq!(r.counted(), (NONE, 0, 1));
    let behind = js_types::HostId::new("h2").unwrap();
    let held = Some(Resources::new(1000, 1 << 20, 0, None).unwrap());
    let booking = js_lease::Booking {
        lease: LeaseId::new(9, 1),
        host: behind.clone(),
        state: js_lease::LeaseState::Held,
        shares: held,
        started_at: Timestamp::from_millis(T0),
        ends_at: Timestamp::from_millis(T0 + 600_000),
    };
    r.provider.seat_booked(&booking, None).unwrap();
    r.placer().engine().ended(LeaseId::new(9, 1));
    assert!(r.start(sharing_spec(2)).is_some());
    assert_eq!(r.counted(), (NONE, 1, 2));
    for sequence in 3..=52 {
        assert!(
            r.start(sharing_spec(sequence)).is_some(),
            "start {sequence}"
        );
        r.provider.stop(&LeaseId::new(1, sequence));
    }
    assert_eq!(r.counted(), (NONE, 1, 2));
}

/// Over two equal hosts with room, the host lease 1 lands on, and the host a second lease
/// lands on after `beats` (each host's two readings, or none) are fed.
fn second_after(name: &str, beats: impl Fn(&str) -> [Option<Wire>; 2]) -> (String, String) {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        name,
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let first = r.start(sharing_spec(1)).unwrap();
    for at in 0..2 {
        r.advance(5_000);
        for host in hosts {
            let reading = beats(&first)[at].clone().filter(|_| host == first);
            let other = beats("")[at].clone().filter(|_| host != first);
            r.beat(host, reading.or(other));
        }
    }
    let second = r.start(sharing_spec(2)).unwrap();
    assert_eq!(r.counted(), (NONE, 0, 1), "{name}");
    (first, second)
}

/// Asserts that the heartbeats' load readings choose among the hosts that fit: the host
/// that carries lease 1 reports all 8 of its cores busy and a 15 GiB working set, the
/// other an idle one, and lease 2 goes to the idle host; with the same hosts and no load
/// in the heartbeats it goes where booking alone sends it, beside lease 1 (the host the
/// lease fits most tightly). It catches readings not fed to the engine, and readings fed
/// to the wrong host (which would send it to the busy one).
#[test]
fn load_readings_choose_among_hosts_that_fit() {
    let busy = |first: &str| {
        if first.is_empty() {
            [Some(reading(1, 0, GIB, &[])), Some(reading(2, 0, GIB, &[]))]
        } else {
            let cover = ["1-1"];
            let ws = 15 * GIB;
            [
                Some(reading(1, 8_000, ws, &cover)),
                Some(reading(2, 8_000, ws, &cover)),
            ]
        }
    };
    let (first, second) = second_after("load_chooses", busy);
    assert_ne!(second, first);
    let (first, second) = second_after("load_absent", |_| [None, None]);
    assert_eq!(second, first);
}

/// Asserts that a heartbeat whose load the vet drops (an active file figure past the most
/// the wire takes) still counts as a beat and feeds no reading: the registry keeps every
/// host live with no reading, and lease 2 goes where booking alone sends it. The dropped
/// readings say all 8 cores of each host are busy with a working set the engine would
/// take, so had they been fed, the engine would place lease 2 nowhere. It catches a beat
/// whose bad load reaches the engine, and a bad load that loses the beat.
#[test]
fn a_heartbeat_with_a_bad_load_still_counts_as_a_beat() {
    let hosts = ["h1", "h2"];
    let mut r = rig(
        "bad_load",
        &pool(2, 8000, ""),
        &hosts,
        &["--placement", "js-sched"],
    );
    let first = r.start(sharing_spec(1)).unwrap();
    for seq in 1..=4 {
        r.advance(5_000);
        for host in hosts {
            let mut bad = reading(seq, 8_000, GIB, &["1-1"]);
            bad.active_file_bytes = MAX_MEMORY_BYTES + 1;
            r.beat(host, Some(bad));
        }
    }
    let listed = r.agents.registry.hosts();
    assert!(listed.iter().all(|host| host.state == HostState::Live));
    assert!(listed.iter().all(|host| host.load.is_none()));
    assert_eq!(r.start(sharing_spec(2)).unwrap(), first);
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// Asserts the CPU slack through the server's configuration: with no `--cpu-slack` (the
/// engine's own default, read from js-sched and not restated) a 10-core host whose leases
/// book 10 cores refuses a start of 1.2 cores and takes one of 1 core, and a start that
/// would pass the host's memory is refused whatever the slack; with `--cpu-slack 1.0` the
/// 1-core start is refused; with `--cpu-slack 1.5` five more cores are taken and a sixth
/// is not, which the engine's own rule decides as much as the registry's book. It catches
/// the setting dropped on either side, a default restated that drifts, and slack applied
/// to memory.
#[test]
fn slack_1_1_lets_cpu_book_ten_percent_over() {
    let sched = ["--placement", "js-sched"];
    let full = |name: &str, extra: &[&str]| {
        let mut r = rig(name, &pool(1, 10_000, ""), &["h1"], extra);
        for sequence in 1..=10 {
            assert_eq!(
                r.start(share_of(sequence, 1_000, GIB)).as_deref(),
                Some("h1")
            );
        }
        r
    };
    let mut r = full("slack_default", &sched);
    assert_eq!(r.start(share_of(11, 1_200, 1)), None);
    assert_eq!(r.start(share_of(12, 1, 7 * GIB)), None);
    assert_eq!(r.start(share_of(13, 1_000, 1)).as_deref(), Some("h1"));
    assert_eq!(r.start(share_of(14, 1, 1)), None);
    let default = LoadConfig::default().s_cpu_permille;
    assert_eq!(r.agents.registry.provider().capacity()[0].leases.len(), 11);
    assert_eq!(CpuSlack::default().permille(), default);
    let mut extra = sched.to_vec();
    extra.extend(["--cpu-slack", "1.0"]);
    let mut r = full("slack_one", &extra);
    assert_eq!(r.start(share_of(11, 1_000, 1)), None);
    let refused = r.provider.start(share_of(12, 1_000, 1));
    assert_eq!(refused, StartAck::Refused(RefusalCode::NoCapacity));
    let mut extra = sched.to_vec();
    extra.extend(["--cpu-slack", "1.5"]);
    let mut r = full("slack_one_half", &extra);
    for sequence in 11..=15 {
        assert_eq!(r.start(share_of(sequence, 1_000, 1)).as_deref(), Some("h1"));
    }
    assert_eq!(r.start(share_of(16, 1_000, 1)), None);
    assert_eq!(r.counted(), (NONE, 0, 1));
}

/// What 90 percent of 16 GiB books, rounded down: 15461882265.6 bytes.
const NINETY: u64 = 16 * GIB / 100 * 90 + 16 * GIB % 100 * 90 / 100;

/// Asserts that under `--placement js-sched` the pool's `memory_schedulable_percent` is
/// the engine's rule: at 90 a host of 16 GiB takes starts totalling 90% of it, and one
/// byte more is placed nowhere (the decline stands and is counted), while the registry
/// books the host's whole 16 GiB and still reports the percent. It catches the registry still cutting the shape (a
/// double cut to 81%, which refuses the third start) and the engine ignoring the key
/// (which places the fourth).
#[test]
fn memory_percent_cuts_bookable_memory_in_the_engine() {
    let percent = "memory_schedulable_percent = 90";
    let sched = ["--placement", "js-sched"];
    let mut r = rig("percent_engine", &pool(2, 8000, percent), &["h1"], &sched);
    assert_eq!(NINETY, 15_461_882_265);
    let third = NINETY - 10 * GIB;
    for (sequence, bytes) in [(1, 5 * GIB), (2, 5 * GIB), (3, third)] {
        assert_eq!(
            r.start(share_of(sequence, 1_000, bytes)).as_deref(),
            Some("h1")
        );
    }
    assert_eq!(r.start(share_of(4, 1_000, 1)), None);
    let declined = Fallbacks {
        unmappable: 0,
        declined: 1,
    };
    assert_eq!(r.counted(), (declined, 0, 1));
    let listed = r.agents.registry.provider().capacity();
    assert_eq!(listed[0].shape.resources().memory_bytes(), 16 * GIB);
    let hosts = r.agents.registry.hosts();
    assert_eq!(hosts[0].memory.percent().get(), 90);
    assert_eq!(hosts[0].memory.bookable_bytes(), NINETY);
}

/// Asserts that a host whose hello claims far more memory than the pool lists (512 GiB)
/// is still held to the pool's percent: a request one byte above 90% of the pool's 16 GiB
/// fits no host (`no-fit`, not a wait), and the engine books what 90% allows on it and not
/// a byte more. It catches a host booked by its claim, and a request above the share left
/// to wait for ever.
#[test]
fn an_agent_claim_never_raises_the_percent() {
    let percent = "memory_schedulable_percent = 90";
    let sched = ["--placement", "js-sched"];
    let mut r = rig("percent_claim", &pool(2, 8000, percent), &[], &sched);
    r.hello(&hello_of("big", 64_000, 512 * GIB));
    let above = r.provider.start(share_of(9, 1_000, NINETY + 1));
    assert_eq!(above, StartAck::Refused(RefusalCode::NoFit));
    assert_eq!(r.start(share_of(1, 1_000, NINETY)).as_deref(), Some("big"));
    assert_eq!(r.start(share_of(2, 1_000, 1)), None);
    assert_eq!(r.placer().fallbacks().declined, 1);
}

/// Asserts that a `memory_schedulable_percent` outside 1 to 100, or not a number, is
/// refused at start under `--placement js-sched` with the pool's text, so no such value
/// reaches the engine. It catches a value let through to the engine unchecked.
#[test]
fn memory_percent_values_outside_1_to_100_are_refused() {
    for value in ["0", "101", "-1", "\"ninety\""] {
        let dir = scratch("percent_refused");
        let text = pool(2, 8000, &format!("memory_schedulable_percent = {value}"));
        let flags = [
            ("--listen", "127.0.0.1:0".into()),
            (
                "--state-file",
                dir.path().join("leases.toml").into_os_string(),
            ),
            ("--boot", "initialise".into()),
            (
                "--token-file",
                write(dir.path(), "token", &"t".repeat(40), 0o600),
            ),
            (
                "--agent-pool-config",
                write(dir.path(), "agent-pool.toml", &text, 0o644),
            ),
            ("--placement", "js-sched".into()),
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
