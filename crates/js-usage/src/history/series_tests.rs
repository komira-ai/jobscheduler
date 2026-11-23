//! The CPU series of an ended-allocation row: its stored form after `used`, a row from before
//! it, and the values a reader refuses.

use super::tests::row;
use super::*;

/// The series the rows of these tests carry: four points, one past a byte and one at the top
/// of 16 bits, so the varints have one, two and three bytes.
fn series() -> CpuSeries {
    CpuSeries::new(2, vec![0, 16, 300, 65535], 12_345_678, true).unwrap()
}

/// The row of [`row`] with [`series`].
fn with_series() -> EndedRow {
    EndedRow {
        cpu_series: Some(series()),
        ..row(9)
    }
}

/// The stored form of [`row`], which ends where a row from before the series ends.
fn before_the_series() -> Vec<u8> {
    encode(&row(9))
}

/// Asserts a series is stored after `used` as a byte 1, the granularity, the run length, the
/// partial flag, the number of points and each point, the numbers as varints, and reads back
/// exactly; that a series that is not partial stores a 0 flag; and that the bytes before it are
/// those of the same row without one. Catches a field stored before `used` or in another order,
/// a width changed, and a flag inverted.
#[test]
fn a_series_is_stored_after_used_and_reads_back() {
    let bytes = encode(&with_series());
    let base = before_the_series();
    assert_eq!(bytes[..base.len()], base[..]);
    assert_eq!(
        bytes[base.len()..],
        [
            1, 2, 0xce, 0xc2, 0xf1, 0x05, 1, 4, 0, 16, 0xac, 0x02, 0xff, 0xff, 0x03
        ]
    );
    assert_eq!(decode(&bytes), Ok(with_series()));

    let whole = EndedRow {
        cpu_series: CpuSeries::new(1, Vec::new(), 0, false),
        ..row(9)
    };
    assert_eq!(encode(&whole)[base.len()..], [1, 1, 0, 0, 0]);
    assert_eq!(decode(&encode(&whole)), Ok(whole));
}

/// Asserts a row without a series is the bytes it was before the field (it ends after
/// `used`), that such a row reads with no series, and that a byte 0 where a series would start
/// reads as none too. Catches a byte written for an absent series, which would change every row
/// an older build wrote and read, and a reader that refuses a row from before the field.
#[test]
fn a_row_from_before_the_series_reads_with_none() {
    let base = before_the_series();
    assert_eq!(decode(&base), Ok(row(9)));
    let mut absent = base;
    absent.push(0);
    assert_eq!(decode(&absent), Ok(row(9)));
}

/// Asserts a series that is not well formed (a granularity that is not a power of two, one that
/// does not fit 16 bits, 129 points) is not written, so the row is the bytes of the row without
/// one. Catches a row this build writes and then refuses to read.
#[test]
fn a_series_that_is_not_well_formed_is_not_written() {
    for (granularity_secs, points) in [(3, 1), (1 << 16, 1), (0, 1), (1, 129)] {
        let ill = EndedRow {
            cpu_series: Some(CpuSeries {
                granularity_secs,
                points: vec![1; points],
                run_usec: 1,
                partial: false,
            }),
            ..row(9)
        };
        assert_eq!(encode(&ill), before_the_series(), "{granularity_secs}");
    }
}

/// Asserts a row cut anywhere inside its series is refused as truncated, and a row cut where
/// the series would start is the row without one. Catches a reader that takes a partial series
/// for a whole one.
#[test]
fn a_truncated_series_is_refused_at_every_length() {
    let bytes = encode(&with_series());
    let base = before_the_series().len();
    assert_eq!(decode(&bytes[..base]), Ok(row(9)));
    for cut in base + 1..bytes.len() {
        assert_eq!(
            decode(&bytes[..cut]),
            Err(EndedRowError::Truncated),
            "cut at {cut}"
        );
    }
}

/// Asserts a granularity that does not fit 32 bits and a point that does not fit 16 are refused
/// as out of range, and that a presence byte that is not 0 or 1, a flag that is not 0 or 1, a
/// granularity that is not a power of two (or does not fit 16 bits) and more than 128 points are
/// refused by the field's name, while exactly 128 points read. Catches a number cut to fit, and a
/// series no agent writes read as one.
#[test]
fn a_series_that_cannot_be_read_is_refused() {
    let read = |tail: &[u8]| {
        let mut bytes = before_the_series();
        bytes.extend_from_slice(tail);
        decode(&bytes)
    };
    let field = EndedRowError::Field("cpu_series");
    let range = EndedRowError::Range;
    assert_eq!(
        read(&[1, 0x80, 0x80, 0x80, 0x80, 0x10, 0, 0, 0]),
        Err(range)
    );
    assert_eq!(read(&[1, 1, 0, 0, 1, 0x80, 0x80, 0x04]), Err(range));
    assert_eq!(read(&[2]), Err(field));
    assert_eq!(read(&[1, 1, 0, 2, 0]), Err(field));
    assert_eq!(read(&[1, 3, 0, 0, 0]), Err(field));
    assert_eq!(read(&[1, 0, 0, 0, 0]), Err(field));
    assert_eq!(read(&[1, 0x80, 0x80, 0x04, 0, 0, 0]), Err(field));
    let mut many = vec![1, 1, 0, 0, 0x81, 0x01];
    many.extend_from_slice(&[0; 129]);
    assert_eq!(read(&many), Err(field));
    let mut most = vec![1, 1, 0, 0, 0x80, 0x01];
    most.extend_from_slice(&[0; 128]);
    assert_eq!(
        read(&most).unwrap().cpu_series,
        CpuSeries::new(1, vec![0; 128], 0, false)
    );
}
