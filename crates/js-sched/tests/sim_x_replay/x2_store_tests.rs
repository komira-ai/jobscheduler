//! An export written from a usage store (`js_usage::x2_export`) reads back as the input of
//! the x2 run: the writer's lines are the format the reader takes, the starts are recorded
//! and each run keeps its series.

use std::sync::Arc;

use js_state::ManualClock;
use js_state::fake::{Fake, Flaw};
use js_types::{CpuSeries, HostId};
use js_usage::{
    AllocationId, Booked, DrainOutcome, EndedHistory, EndedReason, EndedRow, NodeMinute,
    NodeSeries, Used, x2_export,
};

use crate::engine::export::{
    Arrivals, Export, NodeMinute as ReadMinute, parse_key_profile, parse_node_minutes,
};
use crate::engine::x2::{X2Config, run};

const MIN: u64 = 60_000;

fn host(name: &str) -> HostId {
    HostId::new(name).unwrap()
}

/// A run of class `class` on `on`, `run_ms` long, ending at `end`, at a constant
/// `sixteenths` of a core and a peak of `mib` MiB.
fn row(
    id: &str,
    class: &str,
    on: &str,
    end: u64,
    run_ms: u64,
    sixteenths: u16,
    mib: u64,
) -> EndedRow {
    EndedRow {
        id: AllocationId::parse(id).unwrap(),
        profile: None,
        host: Some(host(on)),
        booked: Some(Booked {
            whole_host: false,
            cpu_milli: 2_000,
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
            cpu_milli: 1_000,
            memory_bytes: mib << 20,
            cpu_mean_milli: 1_000,
        }),
        cpu_series: CpuSeries::new(8, vec![sixteenths; 16], run_ms * 1_000, false),
        class: Some(class.to_string()),
        attempt: None,
    }
}

/// Asserts the text the usage store's export writes is read by the x2 reader without a
/// refused line: every run, in start order, with its recorded start from the export's zero,
/// its wall, its minute means and its peak working set; the node minutes of its hosts; and
/// that the x2 run takes it whole. Catches a writer and a reader that disagree on a field's
/// place, its unit, the minute count of a run or the start's clock.
#[test]
fn a_usage_store_export_reads_back_as_x2_input() {
    let store = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    let history = EndedHistory::new(&store);
    // Four runs of class a on h1 and h2 and one of class b, starts 2 minutes apart.
    let rows = [
        row("1-1", "a", "h1", 100 * MIN + 150_000, 150_000, 16, 512),
        row("1-2", "a", "h2", 102 * MIN + 150_000, 150_000, 32, 512),
        row("1-3", "a", "h1", 104 * MIN + 150_000, 150_000, 8, 512),
        row("1-4", "b", "h2", 106 * MIN + 30_000, 30_000, 4, 100),
        row("1-5", "a", "h1", 108 * MIN + 150_000, 150_000, 16, 512),
    ];
    for r in &rows {
        assert!(history.put(r).unwrap());
    }
    let nodes = NodeSeries::new(&store);
    let minute = |minute| NodeMinute {
        minute,
        cpu_millis: 900,
        ws_max_bytes: 1 << 30,
        leases: 1,
    };
    nodes
        .append(&host("h1"), &[minute(100), minute(101)])
        .unwrap();
    nodes.append(&host("h2"), &[minute(103)]).unwrap();

    let written = x2_export(&store, 0, 200 * MIN).unwrap();
    let export = Export {
        profile: parse_key_profile(&written.key_profile).expect("the writer's runs read"),
        nodes: parse_node_minutes(&written.node_minute).expect("the writer's nodes read"),
    };
    assert_eq!(export.arrivals(), Arrivals::Recorded);
    assert_eq!(export.profile.keys.len(), 2);
    let starts: Vec<Option<u64>> = export.profile.runs.iter().map(|r| r.start_ms).collect();
    assert_eq!(starts, [0, 2 * MIN, 4 * MIN, 6 * MIN, 8 * MIN].map(Some));
    let first = &export.profile.runs[0];
    assert_eq!(first.wall_ms, 150_000);
    assert_eq!(first.cores_milli, [1_000, 1_000, 1_000]);
    assert_eq!(first.ws_bytes, [512 << 20; 3]);
    assert_eq!(export.profile.runs[1].cores_milli, [2_000; 3]);
    assert_eq!(export.profile.runs[3].cores_milli, [250]);
    let rows = export.rows();
    assert_eq!(rows[0].cpu_ms, 150_000);
    assert_eq!(rows[0].peak_mem_mib, 512);
    assert_eq!(
        export.nodes,
        [
            ReadMinute {
                node: 0,
                minute: 0,
                mean_cores_milli: 900,
                ws_max_bytes: 1 << 30,
                leases: 1
            },
            ReadMinute {
                node: 0,
                minute: 1,
                mean_cores_milli: 900,
                ws_max_bytes: 1 << 30,
                leases: 1
            },
            ReadMinute {
                node: 1,
                minute: 3,
                mean_cores_milli: 900,
                ws_max_bytes: 1 << 30,
                leases: 1
            },
        ]
    );
    // The fourth run of class a (the fifth lease) is the first with three earlier runs of
    // its key, and the run takes every lease.
    let beliefs = export.beliefs();
    assert!(beliefs.belief(2).is_cold());
    assert!(!beliefs.belief(4).is_cold());
    let report = run(
        &export,
        &X2Config {
            warm_up_min: 0,
            ..X2Config::default()
        },
    );
    for r in &report.runs {
        assert_eq!(r.replay.metrics.counters.completed, 5, "{}", r.label);
        assert_eq!(r.books(), Ok(()), "{}", r.label);
    }
}
