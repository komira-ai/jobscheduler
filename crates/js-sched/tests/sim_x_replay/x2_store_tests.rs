//! The text `js_usage::x2_export` writes reads back as the input of the x2 run: the writer's
//! lines are the format the reader takes, the starts are recorded and each run keeps its
//! series. js-sched may not depend on the usage crate (js-lease-server's layering test), so
//! the text is a copy: `js-usage`'s `five_runs_export_to_the_text_the_reader_is_tested_on`
//! asserts its writer produces exactly these two texts from five history rows.

use crate::engine::export::{Arrivals, Export, NodeMinute, parse_key_profile, parse_node_minutes};
use crate::engine::x2::{X2Config, run};

const MIN: u64 = 60_000;

const KEY_PROFILE: &str = concat!(
    "kp af63dc4c8601ec8c any 0 150000 0 1000 536870912\n",
    "kp af63dc4c8601ec8c any 0 150000 1 1000 536870912\n",
    "kp af63dc4c8601ec8c any 0 150000 2 1000 536870912\n",
    "kp af63dc4c8601ec8c any 120000 150000 0 2000 536870912\n",
    "kp af63dc4c8601ec8c any 120000 150000 1 2000 536870912\n",
    "kp af63dc4c8601ec8c any 120000 150000 2 2000 536870912\n",
    "kp af63dc4c8601ec8c any 240000 150000 0 500 536870912\n",
    "kp af63dc4c8601ec8c any 240000 150000 1 500 536870912\n",
    "kp af63dc4c8601ec8c any 240000 150000 2 500 536870912\n",
    "kp af63df4c8601f1a5 any 360000 30000 0 250 104857600\n",
    "kp af63dc4c8601ec8c any 480000 150000 0 1000 536870912\n",
    "kp af63dc4c8601ec8c any 480000 150000 1 1000 536870912\n",
    "kp af63dc4c8601ec8c any 480000 150000 2 1000 536870912\n",
);

const NODE_MINUTE: &str = concat!(
    "nm 0 0 900 1073741824 1\n",
    "nm 0 1 900 1073741824 1\n",
    "nm 1 3 900 1073741824 1\n",
);

/// Asserts the writer's text is read without a refused line: every run, in start order,
/// with its recorded start from the export's zero, its wall, its minute means and its peak
/// working set; the node minutes of its hosts; warm and cold beliefs; and that the x2 run
/// takes it whole. Catches a writer and a reader that disagree on a field's place, its unit,
/// the minute count of a run or the start's clock.
#[test]
fn a_usage_store_export_reads_back_as_x2_input() {
    let export = Export {
        profile: parse_key_profile(KEY_PROFILE).expect("the writer's runs read"),
        nodes: parse_node_minutes(NODE_MINUTE).expect("the writer's nodes read"),
    };
    assert_eq!(export.arrivals(), Arrivals::Recorded);
    assert_eq!(
        export.profile.keys,
        [0xaf63_dc4c_8601_ec8c, 0xaf63_df4c_8601_f1a5]
    );
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
    let node = |node, minute| NodeMinute {
        node,
        minute,
        mean_cores_milli: 900,
        ws_max_bytes: 1 << 30,
        leases: 1,
    };
    assert_eq!(export.nodes, [node(0, 0), node(0, 1), node(1, 3)]);
    // The fourth run of class a (the fifth lease) is the first with three earlier runs of
    // its key; the third is cold.
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
