//! The host ports of an allocation row (tag 13): they round-trip beside a later field, a row
//! from before them reads as holding none, and a list no writer produces is refused by tag.

use super::*;

// Catches host ports dropped or reordered by a rewrite, or written for an allocation that holds
// none, which would change every row written before ports were booked.
#[test]
fn a_ported_allocation_row_round_trips_with_a_later_field_beside_it() {
    let ported = AllocationRecord {
        host_ports: vec![1, 80, 8080, u16::MAX],
        ..allocation(shape())
    };
    let row = allocation_row(&ported);
    assert_eq!(decode_allocation(&row), Ok(ported.clone()));
    assert_eq!(
        field_of(&allocation_row(&allocation(shape())), 13),
        None,
        "an allocation that holds none writes no field 13"
    );
    let read = decode_allocation(&with(&row, 902, b"later")).expect("reads");
    assert_eq!(read.host_ports, ported.host_ports);
    let rewritten = allocation_row(&read);
    assert_eq!(field_of(&rewritten, 902), Some(b"later".to_vec()));
    assert_eq!(field_of(&rewritten, 13), field_of(&row, 13));
}

// Catches a row from before host ports that is refused, or read as holding some, when it
// should read as holding none.
#[test]
fn an_allocation_row_from_before_host_ports_reads_as_holding_none() {
    let row = allocation_row(&AllocationRecord {
        host_ports: vec![80],
        ..allocation(shape())
    });
    assert_eq!(
        decode_allocation(&without(&row, 13)),
        Ok(allocation(shape()))
    );
}

// Catches host ports that are read from a value that is not a whole number of ports, or that
// is not strictly ascending with no zero, which no writer produces.
#[test]
fn a_host_port_list_that_is_not_valid_is_refused_by_tag() {
    let row = allocation_row(&allocation(shape()));
    let cases: [(&[u8], DecodeError); 5] = [
        (&[0, 80, 0], DecodeError::BadLength { tag: 13 }),
        (&[0, 0], DecodeError::BadValue { tag: 13 }),
        (&[0, 80, 0, 0], DecodeError::BadValue { tag: 13 }),
        (&[0x1f, 0x90, 0, 80], DecodeError::BadValue { tag: 13 }),
        (&[0, 80, 0, 80], DecodeError::BadValue { tag: 13 }),
    ];
    for (value, error) in cases {
        assert_eq!(
            decode_allocation(&with(&row, 13, value)),
            Err(error),
            "{value:?}"
        );
    }
}
