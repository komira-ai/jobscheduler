//! The attempt of an ended-allocation row: its stored form after the class, a row from before
//! it, and the values a reader refuses.

use super::tests::row;
use super::*;
use crate::testkit::TestStore;

/// The row of [`row`] on attempt `attempt`.
fn on(attempt: Option<u32>) -> EndedRow {
    EndedRow { attempt, ..row(9) }
}

/// The stored form of [`row`], which ends where a row from before the series ends.
fn base() -> Vec<u8> {
    encode(&row(9))
}

/// A series of two points.
fn series() -> CpuSeries {
    CpuSeries::new(1, vec![3, 4], 2_000_000, false).unwrap()
}

/// Asserts a row of a first attempt (attempt `None`) has exactly the bytes it had before the
/// field: with no series and no class it ends after `used`; with a series it ends after the
/// series; with a class it ends after the class; and each reads back equal. Catches a byte
/// written for an absent attempt, which would change every row of a first attempt.
#[test]
fn a_row_of_a_first_attempt_is_the_bytes_it_was_before_the_field() {
    let name = "a_row_of_a_first_attempt_is_the_bytes_it_was_before_the_field";
    let bare = encode(&on(None));
    assert_eq!(bare, base(), "{name}: bare");
    let with_series = EndedRow {
        cpu_series: Some(series()),
        ..on(None)
    };
    let bytes = encode(&with_series);
    assert_eq!(bytes[..bare.len()], bare[..], "{name}: series prefix");
    assert_eq!(
        bytes[bare.len()..],
        [1, 1, 0x80, 0x89, 0x7a, 0, 2, 3, 4],
        "{name}: series tail"
    );
    assert_eq!(decode(&bytes), Ok(with_series), "{name}: series back");
    let with_class = EndedRow {
        class: Some("c".to_owned()),
        ..on(None)
    };
    let bytes = encode(&with_class);
    assert_eq!(
        bytes[bare.len()..],
        [0, 1, 1, b'c'],
        "{name}: class tail, no series"
    );
    assert_eq!(decode(&bytes), Ok(with_class), "{name}: class back");
    let both = EndedRow {
        cpu_series: Some(series()),
        class: Some("c".to_owned()),
        ..on(None)
    };
    let bytes = encode(&both);
    assert_eq!(bytes[bytes.len() - 3..], [1, 1, b'c'], "{name}: both tail");
    assert_eq!(decode(&bytes), Ok(both), "{name}: both back");
}

/// Asserts an attempt with no series and no class is stored after `used` as a byte 0 (no
/// series), a byte 0 (no class), a byte 1 and the attempt as a varint, for 2 and for `u32::MAX`,
/// and reads back equal. Catches an attempt stored before the series or class slots, a width
/// changed, and a slot left out.
#[test]
fn an_attempt_alone_is_stored_after_two_absent_slots() {
    let name = "an_attempt_alone_is_stored_after_two_absent_slots";
    let base = base();
    for (attempt, varint) in [
        (2u32, vec![2u8]),
        (u32::MAX, vec![0xff, 0xff, 0xff, 0xff, 0x0f]),
    ] {
        let row = on(Some(attempt));
        let bytes = encode(&row);
        assert_eq!(bytes[..base.len()], base[..], "{name}: prefix {attempt}");
        let mut tail = vec![0, 0, 1];
        tail.extend(varint);
        assert_eq!(bytes[base.len()..], tail[..], "{name}: tail {attempt}");
        assert_eq!(decode(&bytes), Ok(row), "{name}: back {attempt}");
    }
}

/// Asserts an attempt round trips with a series, with a class, and with both, with the series
/// and class slots in their places. Catches an attempt that drops or shifts the fields before it.
#[test]
fn an_attempt_round_trips_with_a_series_and_a_class() {
    let name = "an_attempt_round_trips_with_a_series_and_a_class";
    for attempt in [2u32, u32::MAX] {
        for (series, class) in [
            (Some(series()), None),
            (None, Some("a b/c:d".to_owned())),
            (Some(series()), Some("class".to_owned())),
        ] {
            let row = EndedRow {
                cpu_series: series,
                class,
                ..on(Some(attempt))
            };
            assert_eq!(decode(&encode(&row)), Ok(row), "{name}: {attempt}");
        }
    }
}

/// Asserts a row that ends after `used`, after the series, or after the class reads with no
/// attempt, and a row whose class slot is a byte 0 and ends there too. Catches a reader that
/// refuses a row from before the field.
#[test]
fn a_row_from_before_the_attempt_reads_with_none() {
    let name = "a_row_from_before_the_attempt_reads_with_none";
    assert_eq!(decode(&base()), Ok(on(None)), "{name}: after used");
    let with_series = EndedRow {
        cpu_series: Some(series()),
        ..on(None)
    };
    assert_eq!(
        decode(&encode(&with_series)),
        Ok(with_series),
        "{name}: after series"
    );
    let with_class = EndedRow {
        class: Some("c".to_owned()),
        ..on(None)
    };
    assert_eq!(
        decode(&encode(&with_class)),
        Ok(with_class),
        "{name}: after class"
    );
    let mut absent = base();
    absent.extend([0, 0]);
    assert_eq!(decode(&absent), Ok(on(None)), "{name}: absent slots");
}

/// Asserts a stored attempt above `u32::MAX` is [`EndedRowError::Range`], an option byte other
/// than 0 or 1 for it is `Field("attempt")`, a byte 0 for it reads none, and bytes cut inside
/// the varint or before the option byte are `Truncated`. Catches an attempt truncated to 32
/// bits, a bad flag read as present, and a cut read as a value.
#[test]
fn an_attempt_that_cannot_be_read_is_refused() {
    let name = "an_attempt_that_cannot_be_read_is_refused";
    let mut tail = base();
    tail.extend([0, 0]);
    let with = |rest: &[u8]| {
        let mut bytes = tail.clone();
        bytes.extend_from_slice(rest);
        decode(&bytes)
    };
    assert_eq!(
        with(&[1, 0x80, 0x80, 0x80, 0x80, 0x10]),
        Err(EndedRowError::Range),
        "{name}: above u32"
    );
    assert_eq!(
        with(&[1, 0xff, 0xff, 0xff, 0xff, 0x0f]),
        Ok(on(Some(u32::MAX))),
        "{name}: at u32::MAX"
    );
    assert_eq!(
        with(&[2, 2]),
        Err(EndedRowError::Field("attempt")),
        "{name}: bad flag"
    );
    assert_eq!(with(&[0]), Ok(on(None)), "{name}: absent");
    assert_eq!(
        with(&[1, 0x80]),
        Err(EndedRowError::Truncated),
        "{name}: cut in the varint"
    );
    assert_eq!(
        with(&[1]),
        Err(EndedRowError::Truncated),
        "{name}: cut after the flag"
    );
}

/// Asserts a row of attempt 3 written through [`EndedHistory`] reads back equal through
/// `range`, next to a row of a first attempt that reads back with none. Catches an attempt lost
/// between the row and the store.
#[test]
fn a_row_with_an_attempt_reads_back_through_the_history() {
    let name = "a_row_with_an_attempt_reads_back_through_the_history";
    let store = TestStore::new();
    let history = EndedHistory::new(&*store);
    let later = EndedRow {
        id: AllocationId::parse("7-124").unwrap(),
        ..on(Some(3))
    };
    assert_eq!(history.put(&on(None)), Ok(true), "{name}: put first");
    assert_eq!(history.put(&later), Ok(true), "{name}: put later");
    let read = history
        .range(0, u64::MAX, NonZeroUsize::new(10).unwrap())
        .unwrap();
    assert_eq!(read.skipped, 0, "{name}: skipped");
    assert_eq!(read.rows, [on(None), later], "{name}: rows");
}
