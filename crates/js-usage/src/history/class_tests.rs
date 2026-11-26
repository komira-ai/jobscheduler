//! The class of an ended-allocation row: its stored form after the series, a row from before
//! it, and the values a reader refuses.

use super::tests::row;
use super::*;

/// A build target's full name with the kind of its step: 200 bytes, with capitals, slashes, a
/// colon and spaces, none of which a profile key takes.
fn target() -> String {
    let name = format!("Komira//{}json:json mojo_compile build", "src/".repeat(40));
    let name = format!("{name}{}", " x".repeat((200 - name.len()) / 2));
    let name = format!("{name}{}", "y".repeat(200 - name.len()));
    assert_eq!(name.len(), 200);
    name
}

/// The row of [`row`] with the class `class` and the series `series`.
fn with(class: &str, series: Option<CpuSeries>) -> EndedRow {
    EndedRow {
        class: Some(class.to_owned()),
        cpu_series: series,
        ..row(9)
    }
}

/// Asserts a class is stored after the series as a byte 1, its length and its bytes, and reads
/// back byte for byte; that a row with no series writes a byte 0 for it first; and that the
/// bytes before the class are those of the same row without one, so a build from before the
/// field, which ignores the bytes after the fields it knows, reads the row as it was. Catches a
/// class stored before a field an older build reads, a class cut or lowered, and a row with a
/// class and no series that reads its class as a series.
#[test]
fn a_class_is_stored_after_the_series_and_reads_back_byte_for_byte() {
    let class = target();
    let bare = encode(&row(9));
    let bytes = encode(&with(&class, None));
    assert_eq!(bytes[..bare.len()], bare[..]);
    let mut tail = vec![0, 1, 200, 1];
    tail.extend_from_slice(class.as_bytes());
    assert_eq!(bytes[bare.len()..], tail[..]);
    assert_eq!(decode(&bytes), Ok(with(&class, None)));

    let series = CpuSeries::new(1, Vec::new(), 0, false);
    let without = EndedRow {
        cpu_series: series.clone(),
        ..row(9)
    };
    let before = encode(&without);
    let bytes = encode(&with("compile", series.clone()));
    assert_eq!(bytes[..before.len()], before[..]);
    assert_eq!(bytes[before.len()..], *b"\x01\x07compile");
    assert_eq!(decode(&bytes), Ok(with("compile", series)));
}

/// Asserts a row without a class is the bytes it was before the field (it ends after `used`
/// or after its series), and that such a row, and one with a byte 0 where the class would
/// start, read with none. Catches a byte written for an absent class, which would change every
/// row an older build wrote, and a reader that refuses a row from before the field.
#[test]
fn a_row_from_before_the_class_reads_with_none() {
    let bare = encode(&row(9));
    assert_eq!(decode(&bare), Ok(row(9)));
    let mut absent = bare;
    absent.extend_from_slice(&[0, 0]);
    assert_eq!(decode(&absent), Ok(row(9)));
}

/// Asserts a class cut inside its length or its bytes is refused as truncated, a presence
/// byte that is not 0 or 1 and a class that is not UTF-8 by the field's name. Catches a reader
/// that takes part of a class for the whole of it.
#[test]
fn a_class_that_cannot_be_read_is_refused() {
    let bytes = encode(&with("compile", None));
    let bare = encode(&row(9)).len();
    for cut in bare + 2..bytes.len() {
        assert_eq!(
            decode(&bytes[..cut]),
            Err(EndedRowError::Truncated),
            "cut at {cut}"
        );
    }
    let read = |tail: &[u8]| {
        let mut bytes = encode(&row(9));
        bytes.extend_from_slice(tail);
        decode(&bytes)
    };
    assert_eq!(read(&[0, 2]), Err(EndedRowError::Field("class")));
    assert_eq!(
        read(&[0, 1, 2, 0xff, 0xfe]),
        Err(EndedRowError::Field("class"))
    );
}
