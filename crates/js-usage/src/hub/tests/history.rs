//! The hub as the log of ended allocations: what it measured of a lease goes into the row,
//! and the rows reach the sink at the next tick, in order, without loss while the sink fails.

use super::*;
use crate::{NoKeys, Recorder, TAIL_MINUTE};

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

/// A sink whose readings go through the real [`Recorder`], that keeps the records of ended
/// allocations it is given, can hold the first such write until released, and can panic.
#[derive(Default)]
struct Recording {
    recorder: Mutex<Recorder>,
    written: Mutex<Vec<EndedRow>>,
    /// Signals when the held write has begun, and waits for the release.
    gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    panics: AtomicBool,
}

impl UsageSink for Recording {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: Limits,
        now: u64,
    ) -> Observation {
        let mut recorder = self.recorder.lock().unwrap();
        let (observed, fresh) = recorder.observe(host, epoch, load, limits, now);
        Observation { observed, fresh }
    }

    fn run_ended(&self, _: &RunEnd, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        Ok(Vec::new())
    }

    fn prune(&self, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn node_rows(
        &self,
        _: &HostId,
        _: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageFault> {
        Ok(Vec::new())
    }

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        Ok(None)
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters::default()
    }

    fn allocation_ended(&self, row: &EndedRow) -> Result<(), UsageFault> {
        assert!(!self.panics.load(Ordering::SeqCst), "the sink is broken");
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        self.written.lock().unwrap().push(row.clone());
        Ok(())
    }
}

impl Recording {
    fn written(&self) -> Vec<u64> {
        let rows = self.written.lock().unwrap();
        rows.iter().map(|row| row.actual_end).collect()
    }

    /// Holds the next write until the returned sender is sent to; the receiver hears when
    /// that write has begun.
    fn hold_next(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, began) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        *self.gate.lock().unwrap() = Some((entered, held));
        (began, release)
    }
}

/// The hub over `sink`.
fn hub_on(sink: &Arc<Recording>) -> Arc<UsageHub> {
    let limits = Limits {
        threads: 8,
        memory_bytes: 1 << 34,
    };
    Arc::new(UsageHub::new(sink.clone(), Arc::new(NoKeys), limits, T0))
}

/// A reading with `minutes` of lease `1-n` as `(index, mean cores, working set)`: the
/// `seq`th of a host that runs one core.
fn reading(seq: u64, n: u64, minutes: &[(u16, u32, u64)]) -> BeatLoad {
    let minute = |(index, mean, ws): &(u16, u32, u64)| MinuteIn {
        lease: lease(n),
        minute: *index,
        mean_cores_milli: *mean,
        peak_window_milli: *mean,
        working_set_max_bytes: *ws,
    };
    BeatLoad::Read {
        load: LoadIn {
            seq,
            mono_usec: seq * 60_000_000,
            cpu_usage_usec: seq * 60_000_000,
            working_set_bytes: 1,
            actions_cpu_limited: false,
            covered: vec![lease(n)],
            minutes: minutes.iter().map(minute).collect(),
            clocks: Vec::new(),
        },
        records_dropped: 0,
    }
}

/// Catches: the mean counting the folded tail minute once more each time the agent sends
/// it again as it grows (through the real recorder, which hands the raised tail on as
/// fresh): minute 0 at one core and the tail at two give a mean of 1.5 cores however often
/// the tail comes again, and whichever came first (another index between two sends of the
/// tail does not make it two minutes); with the tail risen to three, 2 cores.
#[test]
fn the_folded_tail_minute_is_one_minute_of_the_mean() {
    let sink = Arc::new(Recording::default());
    let hub = hub_on(&sink);
    let first = [(0, 1000, 10), (TAIL_MINUTE, 2000, 20)];
    send(&hub, "a", reading(1, 1, &first), &[]);
    send(&hub, "a", reading(2, 1, &[(TAIL_MINUTE, 2000, 50)]), &[]);
    send(&hub, "a", reading(3, 2, &first), &[]);
    send(&hub, "a", reading(4, 2, &[(TAIL_MINUTE, 3000, 20)]), &[]);
    // The tail first and minute 0 after it; then the tail again, raised in working set.
    let tail_first = [(TAIL_MINUTE, 2000, 20), (0, 1000, 10)];
    send(&hub, "a", reading(5, 3, &tail_first), &[]);
    send(&hub, "a", reading(6, 3, &[(TAIL_MINUTE, 2000, 50)]), &[]);
    hub.ended(ended_row(1, 1));
    hub.ended(ended_row(2, 2));
    hub.ended(ended_row(3, 3));
    hub.write_history();
    let rows = sink.written.lock().unwrap().clone();
    let expected = Used {
        cpu_milli: 2000,
        memory_bytes: 50,
        cpu_mean_milli: 1500,
    };
    assert_eq!(rows[0].used, Some(expected));
    assert_eq!(rows[1].used.map(|used| used.cpu_mean_milli), Some(2000));
    assert_eq!(rows[2].used, Some(expected));
}

/// Catches: a second write that runs beside one held in the sink and writes a newer row
/// first, and a row the first write leaves for it to write twice. The second write waits
/// for the first, which writes every row in order; the second then has nothing to write.
#[test]
fn writes_take_turns_and_keep_the_order() {
    let sink = Arc::new(Recording::default());
    let hub = hub_on(&sink);
    hub.ended(ended_row(1, 1));
    let (began, release) = sink.hold_next();
    let first = std::thread::spawn({
        let hub = hub.clone();
        move || hub.write_history()
    });
    began.recv().unwrap();
    hub.ended(ended_row(2, 2));
    let second = std::thread::spawn({
        let hub = hub.clone();
        move || hub.write_history()
    });
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap(), Some(Ok(2)));
    assert_eq!(second.join().unwrap(), None);
    assert_eq!(sink.written(), [1, 2]);
}

/// Catches: the row being written dropped twice when the queue overflows meanwhile (the
/// write would then remove a newer row), and rows queued while a write is held lost. The
/// queue is filled past [`MAX_PENDING_ENDED`] while the first row is held in the sink: the
/// oldest two queued go (the held one among them, which the sink still takes once), and
/// the write finishes with the rest.
#[test]
fn rows_arriving_past_the_limit_during_a_held_write_drop_the_oldest_once() {
    let sink = Arc::new(Recording::default());
    let hub = hub_on(&sink);
    hub.ended(ended_row(1, 1));
    let (began, release) = sink.hold_next();
    let writer = std::thread::spawn({
        let hub = hub.clone();
        move || hub.write_history()
    });
    began.recv().unwrap();
    let total = MAX_PENDING_ENDED as u64 + 1;
    for n in 2..=total + 1 {
        hub.ended(ended_row(n, n));
    }
    release.send(()).unwrap();
    let written = writer.join().unwrap();
    assert_eq!(written, Some(Ok(MAX_PENDING_ENDED as u64 + 1)));
    let ends = sink.written();
    assert_eq!(ends.len(), MAX_PENDING_ENDED + 1);
    assert_eq!((ends[0], ends[1]), (1, 3));
    assert_eq!(hub.counters().history_dropped, 2);
}

/// Catches: a sink that panics on a record taking the queue with it, the panic reaching the
/// caller (the tick), and the failure not counted: the row stays queued and is written once
/// the sink works again.
#[test]
fn a_sink_that_panics_on_a_record_leaves_it_queued_and_counts_the_failure() {
    let sink = Arc::new(Recording::default());
    let hub = hub_on(&sink);
    hub.ended(ended_row(1, 1));
    hub.ended(ended_row(2, 2));
    sink.panics.store(true, Ordering::SeqCst);
    let panicked = UsageFault("the sink panicked on a record".to_string());
    assert_eq!(hub.write_history(), Some(Err(panicked)));
    assert_eq!(hub.counters().history_failed, 1);
    sink.panics.store(false, Ordering::SeqCst);
    assert_eq!(hub.write_history(), Some(Ok(2)));
    assert_eq!(sink.written(), [1, 2]);
}
