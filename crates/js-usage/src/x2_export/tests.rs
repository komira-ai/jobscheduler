//! The x2 export: which history rows are runs, the minute means of a series, the start and
//! the zero, the node rows of the runs' hosts, the pages of the history and a node hour that
//! does not read.

use std::num::NonZeroUsize;
use std::sync::Arc;

use js_state::fake::{Fake, Flaw};
use js_state::{Cond, ManualClock, Txn};
use js_types::{CpuSeries, HostId};

use super::*;
use crate::history::{AllocationId, Booked, DrainOutcome, Used};
use crate::key::ProfileKey;
use crate::minute::NodeMinute;
use crate::store::node_key;

const MIN: u64 = 60_000;

fn store() -> Fake {
    Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None)
}

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A run of the export: released on `host`, of class `class`, `run_ms` long, ending at
/// `end`, with `points` at `granularity` seconds a point and a 1 GiB peak.
fn run_row(id: &str, end: u64, run_ms: u64, granularity: u32, points: Vec<u16>) -> EndedRow {
    EndedRow {
        id: AllocationId::parse(id).unwrap(),
        profile: Some(ProfileKey::parse("build.fast").unwrap()),
        host: Some(host("h1")),
        booked: Some(Booked {
            whole_host: false,
            cpu_milli: 4_000,
            memory_bytes: 8 << 30,
        }),
        queued_ms: Some(0),
        run_ms: Some(run_ms),
        ended_reason: EndedReason::Released,
        drain_outcome: DrainOutcome::None,
        give_up_outcome: None,
        estimated_end: None,
        actual_end: end,
        error_ms: None,
        used: Some(Used {
            cpu_milli: 2_000,
            memory_bytes: 1 << 30,
            cpu_mean_milli: 1_000,
        }),
        cpu_series: CpuSeries::new(granularity, points, run_ms * 1_000, false),
        class: Some("build:a".to_string()),
        attempt: None,
    }
}

fn put(store: &dyn Store, rows: &[EndedRow]) {
    let history = EndedHistory::new(store);
    for row in rows {
        assert!(history.put(row).unwrap());
    }
}

fn digest(text: &str) -> String {
    format!("{:016x}", fnv1a(text.as_bytes()))
}

/// Asserts the digest is FNV-1a 64 over the text's bytes, pinned on the empty text and on
/// `a`. Catches another hash (or another offset or prime), which would rename every key of
/// an export written before.
#[test]
fn the_key_digest_is_fnv1a() {
    assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
}

/// Asserts a run of 150 s at one second a point writes three minutes: the mean of the
/// points of each minute's seconds in millicores, the seconds past the last point at the
/// last point, the start from the minute of the earliest start, the class's digest, the
/// platform token, the wall and the peak working set at every minute. Catches a mean over
/// the points rather than the seconds, the tail of the run dropped, a start that is the end,
/// the zero taken from the end, and the profile key read before the class.
#[test]
fn a_run_is_written_minute_by_minute() {
    let s = store();
    let mut points = vec![16; 60];
    points.extend([32; 60]);
    points.extend([8; 8]);
    // Starts 7 s into minute 10.
    put(
        &s,
        &[run_row(
            "7-1",
            10 * MIN + 7_000 + 150_000,
            150_000,
            1,
            points,
        )],
    );
    let export = x2_export(&s, 0, u64::MAX).unwrap();
    let key = digest("build:a");
    assert_eq!(
        export.key_profile,
        format!(
            "kp {key} any 7000 150000 0 1000 1073741824\n\
             kp {key} any 7000 150000 1 2000 1073741824\n\
             kp {key} any 7000 150000 2 500 1073741824\n"
        )
    );
    assert_eq!(
        export.counts,
        X2Counts {
            runs: 1,
            run_minutes: 3,
            node_minutes: 0,
            nodes: 1,
            skipped: X2Skipped::default(),
        }
    );
    assert_eq!(export.node_minute, "");
}

/// Asserts a run of exactly two minutes at two seconds a point writes three minutes (the
/// wall's `wall / 60 s + 1`), the third, which no second of the run is in, at the last
/// point, and that a row with no class is keyed by its profile key. Catches a division by
/// the empty minute, a minute count of `ceil(wall / 60 s)`, and a run with no class dropped.
#[test]
fn a_whole_number_of_minutes_ends_on_the_last_point() {
    let s = store();
    let mut points = vec![16; 30];
    points.extend([48; 30]);
    let mut row = run_row("7-1", 5 * MIN + 120_000, 120_000, 2, points);
    row.class = None;
    put(&s, &[row]);
    let export = x2_export(&s, 0, u64::MAX).unwrap();
    let key = digest("build.fast");
    assert_eq!(
        export.key_profile,
        format!(
            "kp {key} any 0 120000 0 1000 1073741824\n\
             kp {key} any 0 120000 1 3000 1073741824\n\
             kp {key} any 0 120000 2 3000 1073741824\n"
        )
    );
}

/// Asserts each row that is not a run is counted under the first thing it misses, in the
/// order reason, key, host, run time (unknown, then zero), usage, series, a series that
/// misses the start, a series with no point, and that only the run is written. Catches a
/// row with a cause written anyway (a killed or expired run in the curves, a run of no
/// length), a cause counted under another, and a row counted twice.
#[test]
fn rows_that_are_not_runs_are_counted_by_cause() {
    let s = store();
    let good = run_row("1-0", 10 * MIN, 30_000, 1, vec![16]);
    let mut rows = vec![good.clone()];
    let mut add = |id: &str, change: &dyn Fn(&mut EndedRow)| {
        let mut row = run_row(id, 10 * MIN, 30_000, 1, vec![16]);
        change(&mut row);
        rows.push(row);
    };
    add("1-1", &|r| {
        r.ended_reason = EndedReason::Expired;
        r.class = None;
    });
    add("1-2", &|r| {
        r.class = None;
        r.profile = None;
        r.host = None;
    });
    add("1-3", &|r| {
        r.host = None;
        r.run_ms = None;
    });
    add("1-4", &|r| {
        r.run_ms = None;
        r.used = None;
    });
    add("1-5", &|r| r.run_ms = Some(0));
    add("1-6", &|r| {
        r.used = None;
        r.cpu_series = None;
    });
    add("1-7", &|r| r.cpu_series = None);
    add("1-8", &|r| {
        r.cpu_series = CpuSeries::new(1, vec![16], 1, true)
    });
    add("1-9", &|r| {
        r.cpu_series = CpuSeries::new(1, Vec::new(), 1, false)
    });
    put(&s, &rows);
    let export = x2_export(&s, 0, u64::MAX).unwrap();
    assert_eq!(
        export.counts.skipped,
        X2Skipped {
            not_released: 1,
            no_key: 1,
            no_host: 1,
            no_run_time: 2,
            no_usage: 1,
            no_series: 1,
            partial_series: 2,
        }
    );
    assert_eq!(export.counts.runs, 1);
    let key = digest("build:a");
    assert_eq!(
        export.key_profile,
        format!("kp {key} any 0 30000 0 1000 1073741824\n")
    );
}

/// Asserts the runs are written in start order whatever order they ended in, each start
/// from the export's zero, and that the node minutes are those of the runs' hosts only,
/// numbered by host id, from the zero to the last minute before `before_ms`, each minute
/// counted from the zero. Catches runs in end order, a start or a node minute on the
/// epoch's clock, a host that ran nothing exported, node minutes outside the window, and
/// hosts numbered in the order they were met.
#[test]
fn starts_and_node_minutes_count_from_the_zero() {
    let s = store();
    // A long run on h2 that started first (minute 3) and ended last; a short one on h1.
    let mut long = run_row("2-1", 3 * MIN + 600_000, 600_000, 8, vec![16]);
    long.host = Some(host("h2"));
    long.class = Some("long".to_string());
    let short = run_row("2-2", 5 * MIN + 30_000, 30_000, 1, vec![32]);
    put(&s, &[long, short]);
    let minute = |minute: u64, cpu_millis: u32| NodeMinute {
        minute,
        cpu_millis,
        ws_max_bytes: 5,
        leases: 1,
    };
    let series = NodeSeries::new(&s);
    series
        .append(&host("h1"), &[minute(2, 1), minute(4, 2), minute(20, 3)])
        .unwrap();
    series.append(&host("h2"), &[minute(3, 7)]).unwrap();
    series.append(&host("h0"), &[minute(4, 9)]).unwrap();
    let export = x2_export(&s, 0, 20 * MIN).unwrap();
    let lines: Vec<&str> = export.key_profile.lines().collect();
    assert_eq!(lines.len(), 12);
    assert!(lines[0].starts_with(&format!("kp {} any 0 600000 0 ", digest("long"))));
    assert_eq!(
        lines[11],
        format!(
            "kp {} any 120000 30000 0 2000 1073741824",
            digest("build:a")
        )
    );
    assert_eq!(export.node_minute, "nm 0 1 2 5 1\nnm 1 0 7 5 1\n");
    assert_eq!((export.counts.node_minutes, export.counts.nodes), (2, 2));
}

/// Asserts only the rows that ended in `from_ms..before_ms` are read, and that the history is
/// read to its end a page at a time. Catches an inclusive upper bound, a lower bound read as
/// the start, and an export that stops after the first page.
#[test]
fn the_window_is_of_ends_and_every_page_is_read() {
    let s = store();
    put(
        &s,
        &[
            run_row("3-1", 10 * MIN - 1, 1_000, 1, vec![16]),
            run_row("3-2", 10 * MIN, 1_000, 1, vec![16]),
            run_row("3-3", 11 * MIN, 1_000, 1, vec![16]),
            run_row("3-4", 12 * MIN - 1, 1_000, 1, vec![16]),
            run_row("3-5", 12 * MIN, 1_000, 1, vec![16]),
        ],
    );
    let export = paged(&s, 10 * MIN, 12 * MIN, NonZeroUsize::MIN).unwrap();
    assert_eq!(export.counts.runs, 3);
    assert_eq!(export, x2_export(&s, 10 * MIN, 12 * MIN).unwrap());
}

/// Asserts an empty store exports two empty files and no counts. Catches a zero taken from
/// no run (a panic) and a header line in an empty file.
#[test]
fn an_empty_store_exports_nothing() {
    assert_eq!(
        x2_export(&store(), 0, u64::MAX).unwrap(),
        X2Export::default()
    );
}

/// Asserts a node hour that does not read is an error, not a shorter export. Catches a
/// failure read as a host with no minutes.
#[test]
fn a_node_hour_that_does_not_read_is_an_error() {
    let s = store();
    put(&s, &[run_row("4-1", MIN, 1_000, 1, vec![16])]);
    let txn = Txn::new().put(node_key(&host("h1"), 0), vec![0xff], Cond::Absent);
    s.txn(txn).unwrap().unwrap();
    assert!(matches!(
        x2_export(&s, 0, u64::MAX),
        Err(UsageError::Corrupt { .. })
    ));
}
