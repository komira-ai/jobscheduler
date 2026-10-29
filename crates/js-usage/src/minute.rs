//! The node minute row and the codec of an hour of them.
//!
//! An hour row holds up to 60 minutes of one node. Layout (all integers little-endian):
//! a version byte ([`ROW_VERSION`]), a count byte (1 to 60), then `count` entries of
//! [`ENTRY_LEN`] bytes: the minute within the hour (1 byte, below 60, strictly ascending),
//! `cpu_millis` (4), `ws_max_bytes` (8), `leases` (4).

use std::collections::BTreeMap;

use crate::MINUTES_PER_HOUR;
use crate::error::CodecError;

/// The version byte of an hour row.
pub const ROW_VERSION: u8 = 1;

/// The bytes of one entry of an hour row.
pub const ENTRY_LEN: usize = 17;

/// One wall minute of one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeMinute {
    /// Wall minutes since the Unix epoch.
    pub minute: u64,
    /// Mean CPU the node used over the readings in the minute, in millicores.
    pub cpu_millis: u32,
    /// The largest working set read in the minute, in bytes.
    pub ws_max_bytes: u64,
    /// The most leases running at once in any reading of the minute.
    pub leases: u32,
}

/// The bytes of one hour row for `rows`, which are keyed by their wall minute and all lie
/// in one hour (the caller groups them with [`hour_of`]).
#[must_use]
pub fn encode_hour(rows: &BTreeMap<u64, NodeMinute>) -> Vec<u8> {
    let mut out = vec![ROW_VERSION, u8::try_from(rows.len()).unwrap_or(u8::MAX)];
    for row in rows.values() {
        out.push(u8::try_from(row.minute % MINUTES_PER_HOUR).unwrap_or_default());
        out.extend_from_slice(&row.cpu_millis.to_le_bytes());
        out.extend_from_slice(&row.ws_max_bytes.to_le_bytes());
        out.extend_from_slice(&row.leases.to_le_bytes());
    }
    out
}

/// The minutes of the hour row `bytes`, which is hour `hour` since the epoch.
///
/// # Errors
/// [`CodecError`] when the version is unknown, the length does not match the count, or the
/// minutes are out of range or not ascending.
pub fn decode_hour(hour: u64, bytes: &[u8]) -> Result<Vec<NodeMinute>, CodecError> {
    let [version, count, body @ ..] = bytes else {
        return Err(CodecError::Short);
    };
    if *version != ROW_VERSION {
        return Err(CodecError::Version(*version));
    }
    let count = usize::from(*count);
    if count == 0 || count > 60 || body.len() != count * ENTRY_LEN {
        return Err(CodecError::Length);
    }
    let mut rows = Vec::with_capacity(count);
    let mut previous: Option<u8> = None;
    for entry in body.chunks_exact(ENTRY_LEN) {
        let within = entry[0];
        if u64::from(within) >= MINUTES_PER_HOUR {
            return Err(CodecError::Minute);
        }
        if previous.is_some_and(|before| within <= before) {
            return Err(CodecError::Order);
        }
        previous = Some(within);
        rows.push(NodeMinute {
            minute: hour
                .saturating_mul(MINUTES_PER_HOUR)
                .saturating_add(u64::from(within)),
            cpu_millis: u32::from_le_bytes([entry[1], entry[2], entry[3], entry[4]]),
            ws_max_bytes: u64::from_le_bytes([
                entry[5], entry[6], entry[7], entry[8], entry[9], entry[10], entry[11], entry[12],
            ]),
            leases: u32::from_le_bytes([entry[13], entry[14], entry[15], entry[16]]),
        });
    }
    Ok(rows)
}
