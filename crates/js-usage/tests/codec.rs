//! The hour row codec and the errors' texts.

use std::collections::BTreeMap;

use js_state::StoreError;
use js_usage::minute::{ENTRY_LEN, ROW_VERSION, decode_hour, encode_hour};
use js_usage::{CodecError, NodeMinute, UsageError};

fn map(rows: &[NodeMinute]) -> BTreeMap<u64, NodeMinute> {
    rows.iter().map(|r| (r.minute, *r)).collect()
}

fn minute(m: u64) -> NodeMinute {
    NodeMinute {
        minute: m,
        cpu_millis: 1500,
        ws_max_bytes: 0x0102_0304_0506_0708,
        leases: 3,
    }
}

// Asserts the exact bytes of a one-minute row, so a changed field order, width or
// endianness fails; it catches an edit that would make rows written by one build unreadable
// by the next.
#[test]
fn the_row_bytes_are_fixed() {
    let bytes = encode_hour(&map(&[minute(7 * 60 + 5)]));
    let expect: Vec<u8> = vec![
        1, 1, // version, count
        5, // minute within the hour
        0xdc, 0x05, 0, 0, // cpu_millis 1500
        8, 7, 6, 5, 4, 3, 2, 1, // ws_max_bytes
        3, 0, 0, 0, // leases
    ];
    assert_eq!(bytes, expect);
    assert_eq!(ROW_VERSION, 1);
    assert_eq!(bytes.len(), 2 + ENTRY_LEN);
}

// Asserts that a full hour of distinct figures survives the round trip and that the hour
// the reader is told is added back to the minute within it; it catches a swapped field and
// a reader that forgets the hour.
#[test]
fn a_full_hour_round_trips() {
    let hour = 411_000;
    let rows: Vec<NodeMinute> = (0..60)
        .map(|i| NodeMinute {
            minute: hour * 60 + i,
            cpu_millis: u32::MAX - u32::try_from(i).unwrap(),
            ws_max_bytes: u64::MAX - i,
            leases: u32::try_from(i).unwrap() * 3,
        })
        .collect();
    let bytes = encode_hour(&map(&rows));
    assert_eq!(bytes.len(), 2 + 60 * ENTRY_LEN);
    assert_eq!(decode_hour(hour, &bytes).unwrap(), rows);
}

// Asserts each way a stored value can be wrong names its own error; it catches one reader
// check folded into another or dropped.
#[test]
fn a_bad_row_is_refused_for_its_own_reason() {
    let good = encode_hour(&map(&[minute(5), minute(9)]));
    assert_eq!(decode_hour(0, &[]), Err(CodecError::Short));
    assert_eq!(decode_hour(0, &[1]), Err(CodecError::Short));
    let mut wrong = good.clone();
    wrong[0] = 2;
    assert_eq!(decode_hour(0, &wrong), Err(CodecError::Version(2)));
    let truncated = &good[..good.len() - 1];
    assert_eq!(decode_hour(0, truncated), Err(CodecError::Length));
    let mut extra = good.clone();
    extra.push(0);
    assert_eq!(decode_hour(0, &extra), Err(CodecError::Length));
    assert_eq!(decode_hour(0, &[1, 0]), Err(CodecError::Length));
    let mut sixty_one = vec![1, 61];
    sixty_one.resize(2 + 61 * ENTRY_LEN, 0);
    assert_eq!(decode_hour(0, &sixty_one), Err(CodecError::Length));
    let mut late = good.clone();
    late[2] = 60;
    assert_eq!(decode_hour(0, &late), Err(CodecError::Minute));
    let mut repeated = good.clone();
    repeated[2 + ENTRY_LEN] = repeated[2];
    assert_eq!(decode_hour(0, &repeated), Err(CodecError::Order));
    let mut descending = good.clone();
    descending[2] = 10;
    assert_eq!(decode_hour(0, &descending), Err(CodecError::Order));
    assert_eq!(decode_hour(0, &good).unwrap().len(), 2);
}

// Asserts that minute 59 (the last of an hour) is accepted and that a reader given a huge
// hour saturates instead of overflowing.
#[test]
fn the_edges_of_an_hour_read() {
    let last = encode_hour(&map(&[minute(59)]));
    assert_eq!(decode_hour(0, &last).unwrap()[0].minute, 59);
    assert_eq!(decode_hour(u64::MAX, &last).unwrap()[0].minute, u64::MAX);
}

// Asserts every error has a fixed, useful text; it catches a message that echoes a value
// that should not be echoed, or that goes empty.
#[test]
fn errors_say_what_is_wrong() {
    let corrupt = UsageError::Corrupt {
        key: "usage/node/h/0000000001".to_owned(),
        reason: CodecError::Version(9),
    };
    assert_eq!(
        corrupt.to_string(),
        "row usage/node/h/0000000001 is corrupt: the row has version 9, which this build does not read"
    );
    assert_eq!(
        UsageError::MinuteOutOfRange(5).to_string(),
        "wall minute 5 is past the last one a series can key"
    );
    assert_eq!(
        UsageError::Conflict {
            key: "k".to_owned()
        }
        .to_string(),
        "row k changed under the writer; another writer is active"
    );
    let store = UsageError::from(StoreError::Failed("x".to_owned()));
    assert_eq!(store.to_string(), "state store failure: x");
    for (error, text) in [
        (CodecError::Short, "too short"),
        (CodecError::Length, "length"),
        (CodecError::Minute, "within an hour"),
        (CodecError::Order, "ascend"),
    ] {
        assert!(error.to_string().contains(text), "{error}");
    }
}
