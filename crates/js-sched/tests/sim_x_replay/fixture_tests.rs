//! Tests of the committed trace and of the loader that reads it.
//!
//! Each test names the defect it would catch. None reads a clock or asserts a timing.

use std::collections::BTreeSet;

use crate::engine::fixture::{
    EMBEDDED, Fixture, FixtureError, KeyIx, MEM_CLAMP_MIB, MEM_ROUND_ABOVE_MIB, MEM_ROUND_MIB,
    Outcome, Row, SIG_FIGS_ABOVE_MS, coarse_mem_mib, coarse_ms, encode, fnv1a, seal,
};

/// The file size the fixture may not exceed: 2.0 MB.
const SIZE_BOUND: usize = 2_000_000;

fn row(start_ms: u64, key: u16, wall_ms: u64, cpu_ms: u64, mem: u64, outcome: Outcome) -> Row {
    Row {
        start_ms,
        key: KeyIx(key),
        wall_ms,
        cpu_ms,
        peak_mem_mib: mem,
        outcome,
    }
}

/// Three rows over two keys, one of every shape the format has to carry.
fn small() -> Vec<Row> {
    vec![
        row(0, 0, 1, 0, 0, Outcome::Ok),
        row(0, 1, 250, 1_200, 512, Outcome::KilledMemory),
        row(30_000, 0, 123_000, 9_990_000, 65_536, Outcome::TimedOut),
    ]
}

fn small_bytes() -> Vec<u8> {
    encode(&small())
}

fn embedded() -> Fixture {
    Fixture::embedded().expect("the committed fixture loads")
}

/// The header's two counts, read straight from the bytes.
fn header_counts(bytes: &[u8]) -> (u32, u32) {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    (word(4), word(8))
}

#[test]
fn round_trip_of_hand_rows_and_every_outcome() {
    // A byte of the format that drifts (field order, width, varint form) changes what
    // comes back or what is written.
    let mut rows = small();
    rows.push(row(
        1 << 40,
        1,
        u64::MAX / 2,
        u64::MAX / 3,
        1 << 20,
        Outcome::Evicted,
    ));
    let bytes = encode(&rows);
    let loaded = Fixture::load(&bytes).expect("loads");
    assert_eq!(loaded.rows, rows);
    assert_eq!(loaded.n_keys, 2);
    assert_eq!(loaded.hash, fnv1a(&bytes[..bytes.len() - 8]));
    let seen: BTreeSet<u8> = loaded.rows.iter().map(|r| r.outcome.code()).collect();
    assert_eq!(seen, Outcome::ALL.iter().map(|o| o.code()).collect());
    assert_eq!(Outcome::from_code(4), None);
}

#[test]
fn round_trip_of_the_committed_file_is_byte_exact() {
    // Re-encoding what was loaded gives the committed bytes back: the file is in the
    // canonical form, and no field was widened, narrowed or dropped on the way.
    let loaded = embedded();
    assert_eq!(encode(&loaded.rows), EMBEDDED);
}

#[test]
fn committed_file_is_within_the_size_bound() {
    // A text field leaking into a record, or a lost coarsening, grows the file.
    assert!(
        EMBEDDED.len() <= SIZE_BOUND,
        "{} bytes, bound {SIZE_BOUND}",
        EMBEDDED.len()
    );
}

#[test]
fn counts_read_back_equal_the_header() {
    // A silent row drop (or a key lost with it) leaves the records fewer than the header says.
    let loaded = embedded();
    let (n_rows, n_keys) = header_counts(EMBEDDED);
    assert_eq!(loaded.rows.len(), n_rows as usize);
    assert_eq!(loaded.n_keys, n_keys);
    let keys: BTreeSet<u16> = loaded.rows.iter().map(|r| r.key.0).collect();
    assert_eq!(keys.len(), n_keys as usize);
    assert_eq!(keys.last().map(|k| u32::from(*k) + 1), Some(n_keys));
    assert!(n_rows > 0 && n_keys > 0);
}

#[test]
fn rows_start_at_zero_in_order_with_walls_of_at_least_a_millisecond() {
    let loaded = embedded();
    assert_eq!(loaded.rows[0].start_ms, 0);
    assert!(
        loaded
            .rows
            .windows(2)
            .all(|w| w[0].start_ms <= w[1].start_ms)
    );
    assert!(loaded.rows.iter().all(|r| r.wall_ms >= 1));
}

#[test]
fn header_counts_that_disagree_with_the_records_are_refused() {
    let base = small_bytes();
    // One row more than the records hold, and one fewer, each with a valid hash.
    for n_rows in [4u32, 2] {
        let mut b = base.clone();
        b[4..8].copy_from_slice(&n_rows.to_le_bytes());
        seal(&mut b);
        assert!(Fixture::load(&b).is_err(), "n_rows {n_rows} was accepted");
    }
    // More keys than the records use.
    let mut b = base.clone();
    b[8..12].copy_from_slice(&3u32.to_le_bytes());
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::KeyCount { header: 3, seen: 2 })
    );
    // More keys than a u16 index can hold.
    let mut b = base;
    b[8..12].copy_from_slice(&65_537u32.to_le_bytes());
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::TooManyKeys { n_keys: 65_537 })
    );
}

#[test]
fn refuses_bad_magic() {
    let mut b = small_bytes();
    b[0] = b'X';
    seal(&mut b);
    assert_eq!(Fixture::load(&b), Err(FixtureError::BadMagic));
    assert_eq!(Fixture::load(b"JSX2"), Err(FixtureError::BadMagic));
}

#[test]
fn refuses_trailing_bytes() {
    let mut b = small_bytes();
    b.push(0);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::TrailingBytes { extra: 1 })
    );
    let mut b = embedded_bytes();
    b.extend_from_slice(&[1, 2, 3]);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::TrailingBytes { extra: 3 })
    );
}

fn embedded_bytes() -> Vec<u8> {
    EMBEDDED.to_vec()
}

#[test]
fn refuses_a_bad_hash() {
    // Any flipped bit in the hash, and any flipped bit of a value the hash covers.
    let good = small_bytes();
    let mut b = good.clone();
    *b.last_mut().expect("non-empty") ^= 1;
    assert!(matches!(
        Fixture::load(&b),
        Err(FixtureError::BadHash { .. })
    ));
    let mut b = good;
    b[16] ^= 1; // the first record's cpu
    assert!(Fixture::load(&b).is_err());
    let mut b = embedded_bytes();
    let mid = b.len() / 2;
    b[mid] ^= 0x10;
    assert!(Fixture::load(&b).is_err());
}

#[test]
fn refuses_a_key_ix_at_or_past_n_keys() {
    let mut b = small_bytes();
    b[8..12].copy_from_slice(&1u32.to_le_bytes());
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::KeyOutOfRange {
            row: 1,
            key: 1,
            n_keys: 1
        })
    );
}

#[test]
fn refuses_keys_that_do_not_appear_in_order() {
    // Key 2 before key 1 has appeared: the numbering is not first-appearance dense.
    let rows = vec![
        row(0, 0, 5, 5, 1, Outcome::Ok),
        row(1, 2, 5, 5, 1, Outcome::Ok),
        row(2, 1, 5, 5, 1, Outcome::Ok),
    ];
    assert_eq!(
        Fixture::load(&encode(&rows)),
        Err(FixtureError::KeyOrder {
            row: 1,
            key: 2,
            expected: 1
        })
    );
}

#[test]
fn refuses_a_zero_wall() {
    let mut rows = small();
    rows[1].wall_ms = 0;
    assert_eq!(
        Fixture::load(&encode(&rows)),
        Err(FixtureError::ZeroWall { row: 1 })
    );
}

#[test]
fn refuses_every_truncation() {
    // Cut at each byte of a small file and at a spread of points of the committed one:
    // always a refusal, and always for running out of bytes.
    let small = small_bytes();
    for len in 0..small.len() {
        assert!(
            matches!(
                Fixture::load(&small[..len]),
                Err(FixtureError::Truncated { .. })
            ),
            "cut at {len}"
        );
    }
    for len in (0..EMBEDDED.len()).step_by(EMBEDDED.len() / 23 + 1) {
        assert!(
            matches!(
                Fixture::load(&EMBEDDED[..len]),
                Err(FixtureError::Truncated { .. })
            ),
            "cut at {len}"
        );
    }
    let last = &EMBEDDED[..EMBEDDED.len() - 1];
    assert!(matches!(
        Fixture::load(last),
        Err(FixtureError::Truncated { .. })
    ));
}

#[test]
fn refuses_malformed_fields() {
    // An outcome byte outside the four.
    let mut b = small_bytes();
    let end = b.len() - 8;
    b[end - 1] = 9;
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::BadOutcome { row: 2, code: 9 })
    );

    // A first start that is not 0.
    let mut b = small_bytes();
    b[12] = 7;
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::FirstStartNotZero { delta: 7 })
    );

    // A varint with a redundant zero group, and one wider than 64 bits.
    let mut long = small_bytes();
    long.splice(12..13, [0x80, 0x00]);
    seal(&mut long);
    assert_eq!(
        Fixture::load(&long),
        Err(FixtureError::BadVarint { at: 12 })
    );
    let mut wide = small_bytes();
    wide.splice(12..13, [0xff; 9].into_iter().chain([0x02]));
    seal(&mut wide);
    assert_eq!(
        Fixture::load(&wide),
        Err(FixtureError::BadVarint { at: 12 })
    );
    let mut endless = small_bytes();
    endless.splice(12..13, [0x80; 10]);
    seal(&mut endless);
    assert_eq!(
        Fixture::load(&endless),
        Err(FixtureError::BadVarint { at: 12 })
    );

    // Starts that add past 64 bits.
    let rows = vec![
        row(0, 0, 1, 1, 1, Outcome::Ok),
        row(u64::MAX, 0, 1, 1, 1, Outcome::Ok),
        row(u64::MAX, 0, 1, 1, 1, Outcome::Ok),
    ];
    let mut b = encode(&rows);
    // The last record is seven bytes, its start delta the first; make the delta 1.
    let at = b.len() - 8 - 7;
    b[at] = 1;
    seal(&mut b);
    assert_eq!(
        Fixture::load(&b),
        Err(FixtureError::StartOverflow { row: 2 })
    );
}

#[test]
fn the_empty_fixture_loads() {
    let b = encode(&[]);
    let f = Fixture::load(&b).expect("loads");
    assert!(f.rows.is_empty());
    assert_eq!(f.n_keys, 0);
}

#[test]
fn errors_say_what_is_wrong() {
    // Every refusal prints a distinct, non-empty line (the test log is how a red CI is read).
    let errs = [
        FixtureError::BadMagic,
        FixtureError::Truncated { at: 1 },
        FixtureError::BadVarint { at: 2 },
        FixtureError::TooManyKeys { n_keys: 3 },
        FixtureError::FirstStartNotZero { delta: 4 },
        FixtureError::StartOverflow { row: 5 },
        FixtureError::BadOutcome { row: 6, code: 7 },
        FixtureError::ZeroWall { row: 8 },
        FixtureError::KeyOutOfRange {
            row: 9,
            key: 10,
            n_keys: 11,
        },
        FixtureError::KeyOrder {
            row: 12,
            key: 13,
            expected: 14,
        },
        FixtureError::KeyCount {
            header: 15,
            seen: 16,
        },
        FixtureError::BadHash {
            stored: 17,
            computed: 18,
        },
        FixtureError::TrailingBytes { extra: 19 },
    ];
    let lines: BTreeSet<String> = errs.iter().map(|e| e.to_string()).collect();
    assert_eq!(lines.len(), errs.len());
    assert!(lines.iter().all(|l| !l.is_empty()));
}

/// Digits of `v` in decimal, counted from its text rather than from the code under test.
fn digits(v: u64) -> u32 {
    v.to_string().len() as u32
}

#[test]
fn coarsening_vectors() {
    // Unchanged up to the threshold, three significant figures (half up) above it.
    for (input, want) in [
        (0, 0),
        (9_999, 9_999),
        (10_000, 10_000),
        (10_004, 10_000),
        (10_050, 10_100),
        (123_456, 123_000),
        (123_500, 124_000),
        (999_999, 1_000_000),
        (45_678_901, 45_700_000),
    ] {
        assert_eq!(coarse_ms(input), want, "coarse_ms({input})");
    }
    for (input, want) in [
        (0, 0),
        (1_024, 1_024),
        (1_025, 1_028),
        (1_028, 1_028),
        (65_535, 65_536),
        (65_536, 65_536),
        (1 << 30, 65_536),
    ] {
        assert_eq!(coarse_mem_mib(input), want, "coarse_mem_mib({input})");
    }
}

#[test]
fn committed_values_respect_the_coarsening_bounds() {
    // A fingerprintable outlier (a precise wall, an unrounded or unclamped memory)
    // is a value that is not its own coarsening.
    let loaded = embedded();
    let mut over_threshold = 0usize;
    let mut rounded_mem = 0usize;
    for (i, r) in loaded.rows.iter().enumerate() {
        for ms in [r.wall_ms, r.cpu_ms] {
            if ms > SIG_FIGS_ABOVE_MS {
                over_threshold += 1;
                let unit = 10u64.pow(digits(ms) - 3);
                assert_eq!(ms % unit, 0, "row {i}: {ms} ms has more than three figures");
            }
            assert_eq!(coarse_ms(ms), ms, "row {i}");
        }
        assert!(
            r.peak_mem_mib <= MEM_CLAMP_MIB,
            "row {i}: {} MiB",
            r.peak_mem_mib
        );
        if r.peak_mem_mib > MEM_ROUND_ABOVE_MIB {
            rounded_mem += 1;
            assert_eq!(r.peak_mem_mib % MEM_ROUND_MIB, 0, "row {i}");
        }
        assert_eq!(coarse_mem_mib(r.peak_mem_mib), r.peak_mem_mib, "row {i}");
    }
    // The bounds were exercised: the trace has values on both sides of each threshold.
    assert!(over_threshold > 0 && rounded_mem > 0);
    assert!(loaded.rows.iter().any(|r| r.wall_ms <= SIG_FIGS_ABOVE_MS));
    assert!(
        loaded
            .rows
            .iter()
            .any(|r| r.peak_mem_mib <= MEM_ROUND_ABOVE_MIB)
    );
}

#[test]
fn committed_outcomes_are_mostly_ok_and_all_known() {
    // Not a count of the trace: only that the non-ok outcomes are the minority they
    // are in a real day, and that a replay has some of each kind to replay as given.
    let loaded = embedded();
    let not_ok = loaded
        .rows
        .iter()
        .filter(|r| r.outcome != Outcome::Ok)
        .count();
    assert!(not_ok > 0 && not_ok * 20 < loaded.rows.len());
}
