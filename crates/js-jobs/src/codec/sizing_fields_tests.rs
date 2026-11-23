//! The sizing history of an allocation row (tag 16): it round-trips, an older row without it
//! reads as none, a value this build does not understand reads as none and never fails the row,
//! and the layout is pinned byte for byte.

use crate::codec::tests::{allocation, shape, with, without};
use crate::codec::{decode_allocation, encode_allocation, field_of};
use crate::record::{AllocationRecord, SizedFrom};

const SIZED_FROM: u16 = 16;

fn allocation_row(allocation: &AllocationRecord) -> Vec<u8> {
    encode_allocation(allocation).expect("encodes")
}

fn sized(from: Option<SizedFrom>) -> AllocationRecord {
    AllocationRecord {
        sized_from: from,
        ..allocation(shape())
    }
}

/// Every history reads back as written, as one byte of its own (1 class, 2 fallback, 3 cold,
/// 4 over-cap), and none writes no field. It catches two histories sharing a byte, a history
/// lost on the way through the row, and a none written as a value.
#[test]
fn a_sized_from_round_trips_as_one_byte_per_history() {
    let cases = [
        (SizedFrom::Class, 1),
        (SizedFrom::Fallback, 2),
        (SizedFrom::Cold, 3),
        (SizedFrom::OverCap, 4),
    ];
    for (from, byte) in cases {
        let row = allocation_row(&sized(Some(from)));
        assert_eq!(field_of(&row, SIZED_FROM), Some(vec![byte]));
        assert_eq!(decode_allocation(&row), Ok(sized(Some(from))));
    }
    let row = allocation_row(&sized(None));
    assert_eq!(field_of(&row, SIZED_FROM), None);
    assert_eq!(decode_allocation(&row), Ok(sized(None)));
}

/// A row an older build wrote, with no sizing tag, reads with none. It catches a row from
/// before the field refused or read as having a history.
#[test]
fn an_allocation_row_without_the_sized_from_tag_reads_as_none() {
    let row = allocation_row(&sized(Some(SizedFrom::Class)));
    let older = without(&row, SIZED_FROM);
    assert_eq!(decode_allocation(&older), Ok(sized(None)));
}

/// A byte this build does not know, or a value of another length, reads as none and the row
/// still decodes. It catches a newer build's history failing an older reader's row.
#[test]
fn an_unknown_sized_from_value_reads_as_none() {
    let row = allocation_row(&sized(Some(SizedFrom::Class)));
    for value in [&[0u8][..], &[5], &[255], &[], &[1, 1]] {
        let odd = with(&row, SIZED_FROM, value);
        assert_eq!(decode_allocation(&odd), Ok(sized(None)), "{value:?}");
    }
}
