//! Any bytes as an allocation row: the decode must not panic, must hold no more than a small
//! multiple of the row's length at once, and a row that decodes must be written and read back
//! as the same record. The bound is a limit on the allocator during the decode: exceeding it
//! aborts the process, which the fuzzer reports as a crash.

#![no_main]

#[path = "../counting_alloc.rs"]
mod counting_alloc;

use js_jobs::rows::{decode_allocation_row, encode_allocation_row};
use libfuzzer_sys::fuzz_target;

/// The declared bound on the live bytes of one decode is [`PER_ROW_BYTE`] times the row's
/// length plus these bytes; a decode that holds more aborts the process (see
/// `counting_alloc`). They are the constant part of the record (its maps and lists, which exist when empty) and the allocator
/// bookkeeping of a short row.
const FIXED_BYTES: usize = 16 * 1024;
/// The bytes held per byte of row. Every allocation copies a value the row holds: a field's
/// value is copied once into the field map, with its map entry. A row of one-byte fields
/// under distinct tags is the worst found, at about 9 bytes held per byte of row; 32 leaves room
/// above it and stays far below what a length field of `u32::MAX` would allocate if the
/// decoder trusted it.
const PER_ROW_BYTE: usize = 32;

fuzz_target!(|row: &[u8]| {
    let decoded = counting_alloc::bounded(PER_ROW_BYTE * row.len() + FIXED_BYTES, || {
        decode_allocation_row(row)
    });
    if let Ok(record) = decoded {
        let again = encode_allocation_row(&record).expect("a decoded record encodes");
        assert_eq!(decode_allocation_row(&again), Ok(record));
    }
});
