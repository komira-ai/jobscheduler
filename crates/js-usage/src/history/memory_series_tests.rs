//! The memory series of an ended-allocation row: its stored form after the attempt, a row from
//! before it, and the values a reader refuses.

use super::tests::row;
use super::*;
use crate::testkit::TestStore;

/// The series the rows of these tests carry: four points, one past a byte, one past 16 bits
/// and one at the top of 32 bits, so the varints have one, two, three and five bytes.
fn memory() -> MemorySeries {
    MemorySeries::new(2, vec![0, 128, 70_000, u32::MAX], 12_345_678, true).unwrap()
}

/// The stored form of [`memory`] after its presence byte: the granularity, the run length,
/// the partial flag, the number of points and each point.
const MEMORY_BYTES: [u8; 18] = [
    2, 0xce, 0xc2, 0xf1, 0x05, 1, 4, 0, 0x80, 0x01, 0xf0, 0xa2, 0x04, 0xff, 0xff, 0xff, 0xff, 0x0f,
];

/// The row of [`row`] with `memory_series`.
fn with(memory_series: Option<MemorySeries>) -> EndedRow {
    EndedRow {
        memory_series,
        ..row(9)
    }
}

/// The stored form of [`row`], which ends where a row from before the series ends.
fn base() -> Vec<u8> {
    encode(&row(9))
}

/// Asserts a memory series on a row with no CPU series, no class and no attempt is stored after
/// `used` as a byte 0 for each of those three, then a byte 1, the granularity, the run length,
/// the partial flag, the number of points and each point, the numbers as varints (a point of 32
/// bits whole), and reads back exactly; and that a series that is not partial stores a 0 flag.
/// Catches a series stored before the attempt or in another order, a slot left out, a point cut
/// to 16 bits and a flag inverted.
#[test]
fn a_memory_series_alone_is_stored_after_three_absent_slots() {
    let name = "a_memory_series_alone_is_stored_after_three_absent_slots";
    let bytes = encode(&with(Some(memory())));
    let base = base();
    assert_eq!(bytes[..base.len()], base[..], "{name}: prefix");
    let mut tail = vec![0, 0, 0, 1];
    tail.extend_from_slice(&MEMORY_BYTES);
    assert_eq!(bytes[base.len()..], tail[..], "{name}: tail");
    assert_eq!(decode(&bytes), Ok(with(Some(memory()))), "{name}: back");

    let whole = with(MemorySeries::new(1, Vec::new(), 0, false));
    assert_eq!(
        encode(&whole)[base.len()..],
        [0, 0, 0, 1, 1, 0, 0, 0],
        "{name}: empty"
    );
    assert_eq!(decode(&encode(&whole)), Ok(whole), "{name}: empty back");
}

/// Asserts a memory series round trips beside every mix of a CPU series, a class and an
/// attempt, each present or absent, and that the bytes before it are those a build from before
/// the field reads as the same row without it (the row without the series, with a byte 0 for
/// each absent slot up to the attempt). Catches a memory series that drops or shifts a field
/// before it, one read into the CPU series, and a row an older build would misread.
#[test]
fn a_memory_series_round_trips_beside_every_earlier_field() {
    let name = "a_memory_series_round_trips_beside_every_earlier_field";
    let cpu = CpuSeries::new(1, vec![3, 4], 2_000_000, false);
    for cpu_series in [None, cpu] {
        for class in [None, Some("cell//pkg:name build".to_owned())] {
            for attempt in [None, Some(2)] {
                let without = EndedRow {
                    cpu_series: cpu_series.clone(),
                    class: class.clone(),
                    attempt,
                    ..row(9)
                };
                let full = EndedRow {
                    memory_series: Some(memory()),
                    ..without.clone()
                };
                let bytes = encode(&full);
                assert_eq!(decode(&bytes), Ok(full.clone()), "{name}: {full:?}");
                let mut tail = vec![1];
                tail.extend_from_slice(&MEMORY_BYTES);
                let before = bytes.len() - tail.len();
                assert_eq!(bytes[before..], tail[..], "{name}: tail {full:?}");
                assert_eq!(
                    decode(&bytes[..before]),
                    Ok(without.clone()),
                    "{name}: before {full:?}"
                );
                let mut older = encode(&without);
                if without.cpu_series.is_none() && without.class.is_none() && attempt.is_none() {
                    older.push(0);
                }
                if without.class.is_none() && attempt.is_none() {
                    older.push(0);
                }
                if attempt.is_none() {
                    older.push(0);
                }
                assert_eq!(bytes[..before], older[..], "{name}: slots {full:?}");
            }
        }
    }
}

/// Asserts a row without a memory series is the bytes it was before the field, and that a row
/// that ends after `used`, or after a byte 0 in each slot up to and including the memory
/// series', reads with none. Catches a byte written for an absent series, which would change
/// every row an older build wrote and read, and a reader that refuses a row from before the
/// field.
#[test]
fn a_row_from_before_the_memory_series_reads_with_none() {
    let name = "a_row_from_before_the_memory_series_reads_with_none";
    assert_eq!(encode(&with(None)), base(), "{name}: bytes");
    for slots in 0..=4 {
        let mut bytes = base();
        bytes.resize(bytes.len() + slots, 0);
        assert_eq!(decode(&bytes), Ok(row(9)), "{name}: {slots} slots");
    }
}

/// Asserts a memory series that is not well formed (a granularity that is not a power of two,
/// one that does not fit 16 bits, zero, 129 points) is not written, so the row is the bytes of
/// the row without one, alone and beside an attempt. Catches a row this build writes and then
/// refuses to read.
#[test]
fn a_memory_series_that_is_not_well_formed_is_not_written() {
    let name = "a_memory_series_that_is_not_well_formed_is_not_written";
    for (granularity_secs, points) in [(3, 1), (1 << 16, 1), (0, 1), (1, 129)] {
        let ill = MemorySeries {
            granularity_secs,
            points: vec![1; points],
            run_usec: 1,
            partial: false,
        };
        assert_eq!(
            encode(&with(Some(ill.clone()))),
            base(),
            "{name}: {granularity_secs}"
        );
        let attempted = EndedRow {
            attempt: Some(2),
            ..row(9)
        };
        let ill_attempted = EndedRow {
            memory_series: Some(ill),
            ..attempted.clone()
        };
        assert_eq!(
            encode(&ill_attempted),
            encode(&attempted),
            "{name}: beside an attempt {granularity_secs}"
        );
    }
}

/// Asserts a row cut anywhere inside its memory series is refused as truncated, and a row cut
/// where the series would start is the row without one. Catches a reader that takes a partial
/// series for a whole one.
#[test]
fn a_truncated_memory_series_is_refused_at_every_length() {
    let name = "a_truncated_memory_series_is_refused_at_every_length";
    let bytes = encode(&with(Some(memory())));
    let start = base().len() + 3;
    assert_eq!(decode(&bytes[..start]), Ok(row(9)), "{name}: at the start");
    for cut in start + 1..bytes.len() {
        assert_eq!(
            decode(&bytes[..cut]),
            Err(EndedRowError::Truncated),
            "{name}: cut at {cut}"
        );
    }
}

/// Asserts a granularity that does not fit 32 bits and a point that does not fit 32 are refused
/// as out of range, and that a presence byte that is not 0 or 1, a flag that is not 0 or 1, a
/// granularity that is not a power of two (or is zero, or does not fit 16 bits) and more than
/// 128 points are refused by the field's name, while exactly 128 points, and a point at the top
/// of 32 bits, read. Catches a number cut to fit, a refusal named for the CPU series, and a
/// series no agent writes read as one.
#[test]
fn a_memory_series_that_cannot_be_read_is_refused() {
    let name = "a_memory_series_that_cannot_be_read_is_refused";
    let read = |tail: &[u8]| {
        let mut bytes = base();
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(tail);
        decode(&bytes)
    };
    let field = Err(EndedRowError::Field("memory_series"));
    let range = Err(EndedRowError::Range);
    let cases: [(&str, &[u8], Result<EndedRow, EndedRowError>); 8] = [
        (
            "wide granularity",
            &[1, 0x80, 0x80, 0x80, 0x80, 0x10, 0, 0, 0],
            range.clone(),
        ),
        (
            "wide point",
            &[1, 1, 0, 0, 1, 0x80, 0x80, 0x80, 0x80, 0x10],
            range,
        ),
        ("presence", &[2], field.clone()),
        ("flag", &[1, 1, 0, 2, 0], field.clone()),
        ("granularity 3", &[1, 3, 0, 0, 0], field.clone()),
        ("granularity 0", &[1, 0, 0, 0, 0], field.clone()),
        (
            "granularity 2^16",
            &[1, 0x80, 0x80, 0x04, 0, 0, 0],
            field.clone(),
        ),
        (
            "top point",
            &[1, 1, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0x0f],
            Ok(with(MemorySeries::new(1, vec![u32::MAX], 0, false))),
        ),
    ];
    for (case, tail, expected) in cases {
        assert_eq!(read(tail), expected, "{name}: {case}");
    }
    let mut many = vec![1, 1, 0, 0, 0x81, 0x01];
    many.extend_from_slice(&[0; 129]);
    assert_eq!(read(&many), field, "{name}: 129 points");
    let mut most = vec![1, 1, 0, 0, 0x80, 0x01];
    most.extend_from_slice(&[0; 128]);
    assert_eq!(
        read(&most).map(|row| row.memory_series),
        Ok(MemorySeries::new(1, vec![0; 128], 0, false)),
        "{name}: 128 points"
    );
}

/// Asserts a row with a memory series written through [`EndedHistory`] reads back equal through
/// `range`, next to a row without one that reads back with none. Catches a series lost between
/// the row and the store.
#[test]
fn a_row_with_a_memory_series_reads_back_through_the_history() {
    let name = "a_row_with_a_memory_series_reads_back_through_the_history";
    let store = TestStore::new();
    let history = EndedHistory::new(&*store);
    let held = EndedRow {
        id: AllocationId::parse("7-124").unwrap(),
        ..with(Some(memory()))
    };
    assert_eq!(history.put(&with(None)), Ok(true), "{name}: put without");
    assert_eq!(history.put(&held), Ok(true), "{name}: put with");
    let read = history
        .range(0, u64::MAX, NonZeroUsize::new(10).unwrap())
        .unwrap();
    assert_eq!(read.skipped, 0, "{name}: skipped");
    assert_eq!(read.rows, [with(None), held], "{name}: rows");
}
