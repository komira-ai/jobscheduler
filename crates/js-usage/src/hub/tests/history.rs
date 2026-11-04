//! The hub as the log of ended allocations: what it measured of a lease goes into the row,
//! and the rows reach the sink at the next tick, in order, without loss while the sink fails.

use super::*;

/// The row of the lease `1-n`, ending at `end`, with nothing measured.
fn ended_row(n: u64, end: u64) -> EndedRow {
    EndedRow {
        id: AllocationId::of_lease(&lease(n)),
        profile: None,
        host: Some(host("a")),
        booked: None,
        queued_ms: Some(0),
        run_ms: Some(10),
        ended_reason: crate::EndedReason::Released,
        drain_outcome: crate::DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: None,
    }
}

fn written(fake: &Fake) -> Vec<EndedRow> {
    fake.history.lock().unwrap().clone()
}

fn ids(rows: &[EndedRow]) -> Vec<String> {
    rows.iter().map(|row| row.id.to_string()).collect()
}

/// Catches: the row of a lease left without what the hub measured of it, a highest figure
/// that is the last minute's or a sum, a mean that is not the minutes' average, the minutes of another lease mixed
/// in, and a measurement kept after its row took it (a second row of the id would carry
/// it again).
#[test]
fn an_ended_lease_takes_the_highest_minute_measured_of_it() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    send(
        &hub,
        "a",
        read(1, &[(1, 0, 1000, 500), (2, 0, 9000, 9000)]),
        &[],
    );
    send(&hub, "a", read(2, &[(1, 1, 700, 900)]), &[]);
    hub.ended(ended_row(1, 50));
    hub.ended(ended_row(1, 60));
    assert_eq!(hub.write_history(), Some(Ok(2)));
    let rows = written(&fake);
    let used = Used {
        cpu_milli: 1000,
        memory_bytes: 900,
        cpu_mean_milli: 850,
    };
    assert_eq!(rows[0].used, Some(used));
    assert_eq!(rows[1].used, None);
}

/// Catches: what a row's writer measured itself replaced by the hub's figures.
#[test]
fn a_row_that_brings_its_own_use_keeps_it() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    send(&hub, "a", read(1, &[(1, 0, 1000, 500)]), &[]);
    let own = Used {
        cpu_milli: 1,
        memory_bytes: 2,
        cpu_mean_milli: 1,
    };
    hub.ended(EndedRow {
        used: Some(own),
        ..ended_row(1, 50)
    });
    hub.write_history();
    assert_eq!(written(&fake)[0].used, Some(own));
}

/// Catches: minutes the server does not hold for the host that sent them counted as the
/// lease's use (a host could charge another's lease).
#[test]
fn minutes_of_a_lease_the_server_does_not_hold_are_not_measured() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let nobody = |_: &LeaseId, _: &HostId| false;
    send_held(&hub, "a", read(1, &[(1, 0, 1000, 500)]), &[], &nobody);
    hub.ended(ended_row(1, 50));
    hub.write_history();
    assert_eq!(written(&fake)[0].used, None);
}

/// Catches: the measurements growing past [`MAX_TRACKS`] leases (a host listing made-up
/// leases would grow them without bound), and a lease already measured refused when full.
#[test]
fn at_most_max_tracks_leases_are_measured() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let n = MAX_TRACKS as u64;
    let minutes: Vec<(u64, u16, u32, u64)> = (1..=n + 1).map(|i| (i, 0, 5, 5)).collect();
    send(&hub, "a", read(1, &minutes), &[]);
    send(&hub, "a", read(2, &[(1, 1, 9, 9)]), &[]);
    hub.ended(ended_row(1, 1));
    hub.ended(ended_row(n + 1, 1));
    hub.write_history();
    let rows = written(&fake);
    assert_eq!(rows[0].used.map(|used| used.cpu_milli), Some(9));
    assert_eq!(rows[1].used, None);
}

/// Catches: the measurement of a lease no beat has added to for a day kept for ever (a lease
/// whose end was never reported would hold its place).
#[test]
fn a_measurement_idle_for_a_day_is_forgotten() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    send(
        &hub,
        "a",
        read(1, &[(1, 0, 1000, 500), (2, 0, 1000, 500)]),
        &[],
    );
    hub.finish(T0 + TRACK_IDLE_MINUTES);
    hub.ended(ended_row(1, 1));
    hub.finish(T0 + TRACK_IDLE_MINUTES + 1);
    hub.ended(ended_row(2, 1));
    hub.write_history();
    let rows = written(&fake);
    assert!(rows[0].used.is_some());
    assert_eq!(rows[1].used, None);
}

/// Catches: a row written at the hand-off (on the caller's thread, which must not block),
/// rows written out of order, a tick that does not write them, and a tick with nothing
/// queued reported as a write.
#[test]
fn rows_reach_the_sink_at_the_next_tick_oldest_first() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    hub.ended(ended_row(2, 20));
    hub.ended(ended_row(1, 10));
    assert!(written(&fake).is_empty());
    let ticked = hub.tick(T0 + 1);
    assert_eq!(ticked.history, Some(Ok(2)));
    assert_eq!(ids(&written(&fake)), ["1-2", "1-1"]);
    assert_eq!(hub.counters().history_written, 2);
    assert_eq!(hub.tick(T0 + 2).history, None);
}

/// Catches: a row the sink refused lost, rows after it written out of order or lost, a row
/// queued while the sink was failing written ahead of the older ones, and the failure not
/// counted.
#[test]
fn a_row_the_sink_refuses_stays_queued_with_the_ones_after_it() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    *fake.refuse.lock().unwrap() = Some(AllocationId::of_lease(&lease(2)));
    for n in 1..=3 {
        hub.ended(ended_row(n, n));
    }
    let refused = Some(Err(UsageFault("refused".to_string())));
    assert_eq!(hub.write_history(), refused);
    assert_eq!(hub.write_history(), refused);
    hub.ended(ended_row(4, 4));
    *fake.refuse.lock().unwrap() = None;
    assert_eq!(hub.write_history(), Some(Ok(3)));
    assert_eq!(ids(&written(&fake)), ["1-1", "1-2", "1-3", "1-4"]);
    let counts = hub.counters();
    let history = (
        counts.history_written,
        counts.history_failed,
        counts.history_dropped,
    );
    assert_eq!(history, (4, 2, 0));
}

/// Catches: the queue growing without bound while the sink cannot write, and the newest
/// rows dropped in place of the oldest.
#[test]
fn at_most_max_pending_ended_rows_wait_and_the_oldest_are_dropped() {
    let fake = Arc::new(Fake::default());
    let hub = usage(&fake, &[]);
    let total = MAX_PENDING_ENDED as u64 + 2;
    for n in 1..=total {
        hub.ended(ended_row(n, n));
    }
    assert_eq!(hub.counters().history_dropped, 2);
    assert_eq!(hub.write_history(), Some(Ok(MAX_PENDING_ENDED as u64)));
    let rows = written(&fake);
    assert_eq!(rows[0].actual_end, 3);
    assert_eq!(rows.last().map(|row| row.actual_end), Some(total));
}
