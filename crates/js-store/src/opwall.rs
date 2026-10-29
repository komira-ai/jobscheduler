//! The value of an `opwall/<operation>` row: the longest the operation may run, which the
//! scheduler's whole-node option reads ([`js_sched::Scheduler::set_max_wall`]), a version
//! byte (1), the seconds (8 bytes, big-endian) and the nanoseconds within the second (4
//! bytes, big-endian, below a billion). A row of another version is refused, not guessed at.
//! The bytes are those the farm server writes, so either reads the other's rows.

use std::time::Duration;

const VERSION: u8 = 1;

/// `wall` as a row value.
pub(crate) fn encode(wall: Duration) -> Vec<u8> {
    let mut row = vec![VERSION];
    row.extend(wall.as_secs().to_be_bytes());
    row.extend(wall.subsec_nanos().to_be_bytes());
    row
}

/// A row value as a wall time, or what is wrong with it.
pub(crate) fn decode(row: &[u8]) -> Result<Duration, &'static str> {
    let [VERSION, rest @ ..] = row else {
        return Err("not a version 1 wall time");
    };
    let Some((secs, rest)) = rest.split_first_chunk::<8>() else {
        return Err("a wall time is not 12 bytes");
    };
    let Some((nanos, [])) = rest.split_first_chunk::<4>() else {
        return Err("a wall time is not 12 bytes");
    };
    let nanos = u32::from_be_bytes(*nanos);
    if nanos >= 1_000_000_000 {
        return Err("a wall time has a billion nanoseconds or more");
    }
    Ok(Duration::new(u64::from_be_bytes(*secs), nanos))
}
