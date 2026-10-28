//! Tests of the x2 export reader: the text format, the beliefs built from it and the
//! header that says which inputs are real. Each test names the defect it would catch.
//! None reads a clock or asserts a timing, and no assertion reads a float.

use std::path::{Path, PathBuf};

use crate::engine::export::{
    Arrivals, Export, ExportError, KEY_PROFILE_FILE, KeyProfile, LineError, NODE_MINUTE_FILE,
    NodeMinute, ReadError, parse_key_profile, parse_node_minutes, read_dir, render_key_profile,
    render_node_minutes,
};
use crate::engine::predictor::{COLD_CORES_MILLI, COLD_MEM_MIB, MIN_RING, Tail};

const KEY_A: u64 = 0xa1;
const KEY_B: u64 = 0xb2;
const KEY_C: u64 = 0xc3;
const KEY_D: u64 = 0xd4;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sim_x2_export")
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixture_dir().join(name)).expect("the committed fixture reads")
}

fn fixture() -> Export {
    read_dir(&fixture_dir()).expect("the committed fixture parses")
}

/// A scratch directory of its own, emptied of the files these tests write.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("sim_x2_{name}"));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    for file in [KEY_PROFILE_FILE, NODE_MINUTE_FILE] {
        let _ = std::fs::remove_file(dir.join(file));
    }
    dir
}

/// The lease (index in start order) of the `nth` run of `digest`, also in start order.
fn lease(export: &Export, digest: u64, nth: usize) -> usize {
    let key = export
        .profile
        .keys
        .iter()
        .position(|k| *k == digest)
        .unwrap();
    let rows = export.rows();
    (0..rows.len())
        .filter(|&i| usize::from(rows[i].key.0) == key)
        .nth(nth)
        .unwrap()
}

fn line_error(text: &str) -> ExportError {
    parse_key_profile(text).expect_err("the text is malformed")
}

#[test]
fn fixture_holds_what_its_lines_say() {
    // A reader that drops a line, a run or a node row.
    let e = fixture();
    assert_eq!(e.profile.keys, vec![KEY_A, KEY_B, KEY_C, KEY_D]);
    assert_eq!(e.profile.runs.len(), 13);
    let minutes: usize = e.profile.runs.iter().map(|r| r.cores_milli.len()).sum();
    assert_eq!(minutes, 32);
    assert_eq!(e.nodes.len(), 8);
    assert_eq!(e.arrivals(), Arrivals::Recorded);
    assert_eq!(fixture_text(KEY_PROFILE_FILE).lines().count(), 32);
    assert_eq!(fixture_text(NODE_MINUTE_FILE).lines().count(), 8);
}

#[test]
fn text_round_trips() {
    // A render that loses or reorders a field, or a parser that reads a field another way.
    let kp = fixture_text(KEY_PROFILE_FILE);
    let nm = fixture_text(NODE_MINUTE_FILE);
    let profile = parse_key_profile(&kp).unwrap();
    assert_eq!(render_key_profile(&profile), kp);
    assert_eq!(
        parse_key_profile(&render_key_profile(&profile)).unwrap(),
        profile
    );
    let nodes = parse_node_minutes(&nm).unwrap();
    assert_eq!(render_node_minutes(&nodes), nm);
    assert_eq!(
        parse_node_minutes(&render_node_minutes(&nodes)).unwrap(),
        nodes
    );
    // The export without starts takes the same road.
    let mut bare = profile;
    for run in &mut bare.runs {
        run.start_ms = None;
    }
    let text = render_key_profile(&bare);
    assert!(text.lines().all(|l| l.split(' ').nth(3) == Some("-")));
    assert_eq!(parse_key_profile(&text).unwrap(), bare);
}

#[test]
fn comments_and_blank_lines_are_skipped_but_counted() {
    // A numbering that ignores skipped lines would name the wrong line.
    let text = "# header\n\nkp 00000000000000a1 linux-x64 0 1000 0 1 2\n\nkp zz\n";
    let err = line_error(text);
    assert_eq!(err.line, 5);
    assert_eq!(err.kind, LineError::FieldCount { want: 8, got: 2 });
    assert_eq!(err.to_string(), "line 5: wants 8 fields, has 2");
}

const OK: &str = "kp 00000000000000a1 linux-x64 0 70000 0 1000 4096\n\
                  kp 00000000000000a1 linux-x64 0 70000 1 2000 8192\n";

#[test]
fn each_malformed_key_profile_line_is_refused_with_its_number() {
    // Every case: the text, the line that is wrong, and why. A parser that accepts the
    // line, blames another one, or names another reason fails its case.
    let k = "00000000000000a1";
    let cases: Vec<(String, usize, LineError)> = vec![
        ("nm 1 2 3 4 5\n".into(), 1, LineError::UnknownTag),
        (
            format!("{OK}kp {k} linux-x64 0 70000 0 1\n"),
            3,
            LineError::FieldCount { want: 8, got: 7 },
        ),
        (
            format!("kp {k}  linux-x64 0 70000 0 1 2\n"),
            1,
            LineError::FieldCount { want: 8, got: 9 },
        ),
        (
            "kp 00000000000000A1 linux-x64 0 70000 0 1 2\n".into(),
            1,
            LineError::BadKey,
        ),
        (
            "kp a1 linux-x64 0 70000 0 1 2\n".into(),
            1,
            LineError::BadKey,
        ),
        (
            format!("kp {k} Linux 0 70000 0 1 2\n"),
            1,
            LineError::BadPlatform,
        ),
        (
            format!("kp {k} {} 0 70000 0 1 2\n", "x".repeat(33)),
            1,
            LineError::BadPlatform,
        ),
        (
            format!("kp {k} linux-x64 +5 70000 0 1 2\n"),
            1,
            LineError::BadNumber { field: "start_ms" },
        ),
        (
            format!("kp {k} linux-x64 0 7e4 0 1 2\n"),
            1,
            LineError::BadNumber { field: "wall_ms" },
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 -1 2\n"),
            1,
            LineError::BadNumber {
                field: "cores_milli",
            },
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 4294967296 2\n"),
            1,
            LineError::BadNumber {
                field: "cores_milli",
            },
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 1 \n"),
            1,
            LineError::BadNumber { field: "ws_bytes" },
        ),
        (
            format!("kp {k} linux-x64 0 0 0 1 2\n"),
            1,
            LineError::ZeroWall,
        ),
        (
            format!("{OK}kp {k} linux-x64 - 70000 0 1 2\nkp {k} linux-x64 - 70000 1 1 2\n"),
            3,
            LineError::MixedStart,
        ),
        (
            format!("kp {k} linux-x64 0 70000 1 1 2\n"),
            1,
            LineError::MinuteOrder { want: 0, got: 1 },
        ),
        (
            format!("kp {k} linux-x64 0 130000 0 1 2\nkp {k} linux-x64 0 130000 2 1 2\n"),
            2,
            LineError::MinuteOrder { want: 1, got: 2 },
        ),
        (
            format!("kp {k} linux-x64 0 130000 0 1 2\nkp {k} linux-x64 0 130000 0 1 2\n"),
            1,
            LineError::ShortRun { have: 1, want: 3 },
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 1 2\nkp {k} linux-x64 1 70000 1 1 2\n"),
            2,
            LineError::RunChanged,
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 1 2\nkp {k} linux-x64 0 70001 1 1 2\n"),
            2,
            LineError::RunChanged,
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 1 2\nkp {k} darwin 0 70000 1 1 2\n"),
            2,
            LineError::RunChanged,
        ),
        (
            format!(
                "kp {k} linux-x64 0 70000 0 1 2\nkp 00000000000000b2 linux-x64 0 70000 1 1 2\n"
            ),
            2,
            LineError::RunChanged,
        ),
        (
            format!("{OK}kp {k} linux-x64 0 70000 2 1 2\n"),
            3,
            LineError::PastWall { last: 1 },
        ),
        (
            format!("kp {k} linux-x64 0 70000 0 1 2\n"),
            1,
            LineError::ShortRun { have: 1, want: 2 },
        ),
    ];
    for (text, line, kind) in cases {
        let err = line_error(&text);
        assert_eq!((err.line, &err.kind), (line, &kind), "text:\n{text}");
        assert!(err.to_string().starts_with(&format!("line {line}: ")));
    }
}

#[test]
fn too_many_keys_is_refused_at_the_first_line_past_the_index() {
    // A key index that wraps and merges two keys.
    let mut text = String::new();
    for k in 0..=65_536u64 {
        text.push_str(&format!("kp {k:016x} p 0 1 0 1 2\n"));
    }
    let err = line_error(&text);
    assert_eq!(err.line, 65_537);
    assert_eq!(err.kind, LineError::TooManyKeys);
}

#[test]
fn each_malformed_node_line_is_refused_with_its_number() {
    // The same, for the node file.
    let cases: Vec<(&str, usize, LineError)> = vec![
        ("kp 1 2 3 4 5\n", 1, LineError::UnknownTag),
        (
            "nm 1 0 5 6 7\n# c\nnm 1 1 5 6\n",
            3,
            LineError::FieldCount { want: 6, got: 5 },
        ),
        (
            "nm host1 0 5 6 7\n",
            1,
            LineError::BadNumber { field: "node" },
        ),
        (
            "nm 1 0 5 6 7\nnm 1 1 5 -6 7\n",
            2,
            LineError::BadNumber {
                field: "ws_max_bytes",
            },
        ),
        (
            "nm 1 0 5 6 7\nnm 2 0 5 6 7\nnm 1 0 5 6 7\n",
            3,
            LineError::NodeMinuteOrder { after: 0 },
        ),
        (
            "nm 1 4 5 6 7\nnm 1 3 5 6 7\n",
            2,
            LineError::NodeMinuteOrder { after: 4 },
        ),
    ];
    for (text, line, kind) in cases {
        let err = parse_node_minutes(text).expect_err("malformed");
        assert_eq!((err.line, &err.kind), (line, &kind), "text:\n{text}");
    }
}

/// Every run's series moved, to see what a later edit reaches.
fn perturbed(export: &Export, run: usize) -> Export {
    let mut e = export.clone();
    for c in &mut e.profile.runs[run].cores_milli {
        *c += 7_777;
    }
    for w in &mut e.profile.runs[run].ws_bytes {
        *w += 1 << 30;
    }
    e
}

fn belief_bytes(export: &Export, lease: usize) -> Vec<u8> {
    let mut out = Vec::new();
    export.beliefs().belief(lease).encode(&mut out);
    out
}

#[test]
fn editing_a_run_changes_no_belief_of_a_lease_that_starts_no_later() {
    // A learner that reads a run it should not yet have: a look-ahead.
    let base = fixture();
    let rows = base.rows();
    let order = base.order();
    let mut reached_later = 0;
    for edited in 0..base.profile.runs.len() {
        let edited_lease = order.iter().position(|&r| r == edited).unwrap();
        let start = rows[edited_lease].start_ms;
        let after = perturbed(&base, edited);
        assert_eq!(after.order(), order);
        for l in 0..rows.len() {
            let same = belief_bytes(&base, l) == belief_bytes(&after, l);
            if rows[l].start_ms <= start {
                assert!(same, "lease {l} saw run {edited}, which starts no earlier");
            } else if !same {
                reached_later += 1;
                // Only the edited run's own key can learn from it.
                assert_eq!(rows[l].key, rows[edited_lease].key);
            }
        }
    }
    // The edits do reach later runs of the key, or the test above is empty.
    assert!(reached_later > 0);
}

#[test]
fn a_run_ending_after_a_lease_starts_is_not_read_by_it() {
    // A learner that admits a run at its start instead of its end.
    let base = fixture();
    // Key B's runs are 150 s long and start 200 s apart, so none overlaps; pull the
    // starts closer so that each run ends after the next one starts.
    let mut e = base.clone();
    let b = e.profile.keys.iter().position(|k| *k == KEY_B).unwrap();
    for run in e
        .profile
        .runs
        .iter_mut()
        .filter(|r| usize::from(r.key.0) == b)
    {
        run.start_ms = run.start_ms.map(|s| s * 2 / 3);
    }
    let rows = e.rows();
    let beliefs = e.beliefs();
    for l in 0..rows.len() {
        let seen = rows
            .iter()
            .filter(|r| r.key == rows[l].key && r.start_ms + r.wall_ms < rows[l].start_ms)
            .count();
        assert_eq!(beliefs.belief(l).is_cold(), seen < MIN_RING, "lease {l}");
    }
}

#[test]
fn a_key_is_flat_until_three_earlier_runs_exist() {
    // A belief that warms early, or never; and one that is not the flat cold charge.
    let e = fixture();
    let beliefs = e.beliefs();
    for (digest, runs) in [(KEY_A, 5), (KEY_B, 4), (KEY_C, 2), (KEY_D, 2)] {
        for nth in 0..runs {
            let b = beliefs.belief(lease(&e, digest, nth));
            assert_eq!(b.is_cold(), nth < MIN_RING, "key {digest:x} run {nth}");
            if nth < MIN_RING {
                assert_eq!(b.curve_len(), 1);
                assert_eq!(b.life_minutes(), 1);
                for minute in [0, 1, 7] {
                    assert_eq!(b.cores_milli_at(minute, Tail::Held), COLD_CORES_MILLI);
                    assert_eq!(b.mem_mib_at(minute, Tail::Held), COLD_MEM_MIB);
                }
            }
        }
    }
}

#[test]
fn a_warm_belief_is_the_p90_of_the_earlier_runs_series() {
    // Wrong units (bytes for MiB, cores for millicores) or a wrong rank on the first
    // warm lease of key A, whose three earlier runs are the first three.
    let e = fixture();
    let b = e.beliefs();
    let b = b.belief(lease(&e, KEY_A, 3));
    assert!(!b.is_cold());
    assert_eq!(b.cores_milli_at(0, Tail::Held), 1_200);
    assert_eq!(b.cores_milli_at(1, Tail::Held), 3_200);
    assert_eq!(b.mem_mib_at(0, Tail::Held), 640);
    assert_eq!(b.mem_mib_at(1, Tail::Held), 1_152);
    assert_eq!(b.life_minutes(), 2);
}

#[test]
fn rows_carry_the_series_totals() {
    // A cpu_ms that counts a partial last minute as whole, or a peak in the wrong unit.
    let e = fixture();
    let rows = e.rows();
    let first = rows[lease(&e, KEY_A, 0)];
    // 1.0 core for 60 s and 3.0 cores for the last 30 s.
    assert_eq!(first.cpu_ms, 60_000 + 90_000);
    assert_eq!(first.wall_ms, 90_000);
    assert_eq!(first.peak_mem_mib, 1_024);
    assert!(
        rows.iter()
            .all(|r| r.outcome == crate::engine::fixture::Outcome::Ok)
    );
    assert!(rows.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
}

#[test]
fn without_starts_the_runs_follow_one_another_and_the_header_says_so() {
    // Invented arrivals that overlap (so a run reads its successor) or go unlabelled.
    let mut e = fixture();
    for run in &mut e.profile.runs {
        run.start_ms = None;
    }
    assert_eq!(e.arrivals(), Arrivals::Invented);
    let rows = e.rows();
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.key, e.profile.runs[i].key, "file order is kept");
        if i > 0 {
            assert!(rows[i - 1].start_ms + rows[i - 1].wall_ms < row.start_ms);
        }
    }
    let beliefs = e.beliefs();
    let header = e.header(&beliefs);
    assert!(header.contains("invented: arrivals"));
    assert!(!header.contains("real: arrivals"));
    // Key A's runs are the first five in file order: the fourth is the first warm one.
    assert!(beliefs.belief(2).is_cold());
    assert!(!beliefs.belief(3).is_cold());
}

#[test]
fn the_header_names_what_is_real_and_what_is_invented() {
    // A header that calls an invented input real, or hides a missing one.
    let e = fixture();
    let beliefs = e.beliefs();
    let header = e.header(&beliefs);
    let lines: Vec<&str> = header.lines().collect();
    assert_eq!(
        lines,
        vec![
            "x2 inputs (report only; nothing here is asserted)",
            "real: per-run minute series (cores, working set): 13 runs, 4 keys, 32 run-minutes",
            "real: per-node minute means: 8 node-minutes over 2 nodes",
            "real: arrivals (recorded starts)",
            "invented: host pool and capacities",
            "dropped: historical placement, kill metrics, within-minute bursts",
            "beliefs: from the key's earlier runs only; cold below 3 runs (10 of 13 leases cold)",
        ]
    );
    let mut bare = e.clone();
    bare.nodes.clear();
    let header = bare.header(&beliefs);
    assert!(header.contains("absent: per-node minute means"));
    assert!(!header.contains("real: per-node"));
}

#[test]
fn read_dir_reports_missing_and_bad_files() {
    // A missing required file read as empty, a missing optional one as an error, and a
    // parse error that does not say which file.
    let dir = scratch("read_dir");
    assert!(matches!(read_dir(&dir), Err(ReadError::Io { .. })));

    std::fs::write(dir.join(KEY_PROFILE_FILE), OK).unwrap();
    let e = read_dir(&dir).expect("node_minute.txt is optional");
    assert!(e.nodes.is_empty());
    assert_eq!(e.profile.runs.len(), 1);

    std::fs::write(dir.join(NODE_MINUTE_FILE), "nm 1 0 5 6 7\nnm 1 0 5 6 7\n").unwrap();
    let err = read_dir(&dir).expect_err("duplicate node minute");
    assert_eq!(
        err.to_string(),
        "node_minute.txt: line 2: node minute does not increase past 0"
    );

    std::fs::write(dir.join(KEY_PROFILE_FILE), "kp x\n").unwrap();
    let err = read_dir(&dir).expect_err("bad kp line");
    assert!(
        err.to_string().starts_with("key_profile.txt: line 1: "),
        "{err}"
    );

    // A node file that exists but cannot be read as text is an error, not an absence.
    std::fs::write(dir.join(KEY_PROFILE_FILE), OK).unwrap();
    std::fs::write(dir.join(NODE_MINUTE_FILE), [0xff, 0xfe]).unwrap();
    assert!(matches!(read_dir(&dir), Err(ReadError::Io { .. })));
}

#[test]
fn an_empty_profile_is_an_empty_export() {
    // A panic on the degenerate input.
    let e = Export {
        profile: KeyProfile::default(),
        nodes: Vec::<NodeMinute>::new(),
    };
    assert!(e.rows().is_empty());
    assert!(e.beliefs().is_empty());
    assert!(e.header(&e.beliefs()).contains("0 runs, 0 keys"));
}
