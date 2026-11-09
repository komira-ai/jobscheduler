//! What the placement engine keeps of the hosts the registry hears and forgets: a reading
//! that stops coming goes stale while the host stays, a host that says hello again starts
//! its readings again, and hosts the registry forgets leave the engine. Everything runs in
//! process on a manual clock, with jobs placed through the pass over the queue; no listener
//! is bound.

use js_registry::HostState;

use super::engine_rig::*;

/// Asserts what beats with no new reading do to a host whose reading was fresh: a beat
/// with no load, one whose load the vet drops and one that repeats the last reading keep
/// the host live in the registry and a worker of the engine, and none refreshes the old
/// reading, which goes stale three windows after it came; the other host, whose readings
/// keep coming, stays fresh. Once stale, a queued job is still placed. It catches a beat
/// that refreshes an old reading, one that ends the host, and staleness spilling from one
/// host to another.
#[test]
fn a_reading_that_stops_coming_goes_stale_and_the_host_stays() {
    let hosts = ["h1", "h2"];
    let r = rig("stale_reading", &pool(2, 8000, ""), &hosts, &[]);
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat("h1", Some(reading(seq, 8_000, GIB)));
        r.beat("h2", Some(reading(seq, 0, GIB)));
    }
    let view = |host: &str| r.placer().load_of(host).unwrap();
    assert!(!view("h1").fallback);
    assert!(view("h1").left_cpu_millis >= 8_000);
    let mut bad = reading(3, 8_000, GIB);
    bad.active_file_bytes = js_agent_wire::limits::MAX_MEMORY_BYTES + 1;
    let beats = [None, Some(bad), Some(reading(2, 8_000, GIB))];
    for (n, beat) in (3..).zip(beats) {
        r.advance(5_000);
        r.beat("h1", beat);
        r.beat("h2", Some(reading(n, 0, GIB)));
    }
    // 15 s after the last reading of h1 came: not yet past three windows.
    assert!(!view("h1").fallback);
    r.advance(5_000);
    r.beat("h1", None);
    r.beat("h2", Some(reading(6, 0, GIB)));
    let stale = view("h1");
    assert!(stale.fallback);
    assert_eq!(stale.left_cpu_millis, 0);
    assert!(!view("h2").fallback);
    let listed = r.agents.registry.hosts();
    assert!(listed.iter().all(|host| host.state == HostState::Live));
    assert!(r.place("job-1", 1_000, 1 << 20).is_some());
}

/// Asserts that a host that says hello again starts its readings again: its new session's
/// readings, numbered from 1 again, are taken (an idle window where the old session was
/// busy), and the host takes a job after. It catches readings of a new session refused as
/// going back.
#[test]
fn a_returning_host_starts_its_readings_again() {
    let hosts = ["h1", "h2"];
    let mut r = rig("returning", &pool(2, 8000, ""), &hosts, &[]);
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat("h1", Some(reading(seq, 8_000, GIB)));
    }
    assert!(r.placer().load_of("h1").unwrap().left_cpu_millis >= 8_000);
    // Silent past the registry's grace, the host is lost and may say hello again.
    r.advance(20_000);
    r.hello(&hello_of("h1", 16_000, 32 * GIB));
    for seq in 1..=2 {
        r.advance(5_000);
        r.beat("h1", Some(reading(seq, 0, GIB)));
    }
    let back = r.placer().load_of("h1").unwrap();
    assert!(!back.fallback);
    assert_eq!(back.measured_cpu_millis, Some(0));
    assert!(r.place("job-1", 1_000, 1 << 20).is_some());
}

/// Asserts that hosts the registry forgets (lost past the pool's retention with nothing
/// booked) are forgotten by the engine, so that hosts coming and going over and over keep
/// no more state than the hosts there are. It catches an engine that keeps every host it
/// ever heard of.
#[test]
fn hosts_the_registry_forgets_leave_the_engine() {
    let text = pool(2, 8000, "lost_retention = \"1m\"");
    let mut r = rig("churn", &text, &[], &[]);
    for round in 0..10u64 {
        let name = format!("h{round}");
        r.hello(&hello_of(&name, 16_000, 32 * GIB));
        assert!(r.placer().load_of(&name).is_some(), "round {round}");
        assert!(r.placer().hosts_kept() <= 2, "round {round}");
        r.advance(15_000 + 60_000 + 1_000);
        let _ = r.agents.registry.hosts();
        assert!(r.placer().load_of(&name).is_none(), "round {round}");
    }
    assert_eq!(r.placer().hosts_kept(), 0);
}
