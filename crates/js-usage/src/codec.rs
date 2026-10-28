//! The stored form of one key's profile: the ring of runs and the day the key was last
//! used. The fit is not stored; it is computed from the ring.
//!
//! Layout, version 1: a version byte, the last-used day (varint), the run count (one
//! byte), then per run its wall minutes (varint), its flag byte, its bucket count (one
//! byte) and per bucket either a varint 0 (no record) or a varint `cores_milli + 1`
//! followed by a varint of the working set. A varint is little-endian base 128, 7 bits a
//! byte, high bit set on all but the last.

use thiserror::Error;

use crate::ring::{BUCKETS, Bucket, RING_RUNS, Ring, Run};

/// The version byte this build writes and reads.
pub const ROW_VERSION: u8 = 1;

/// Why a stored value is not a profile row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum CodecError {
    /// The version byte is not [`ROW_VERSION`].
    #[error("profile row version {0} is not supported")]
    Version(u8),
    /// The value ends inside a field.
    #[error("profile row is truncated")]
    Truncated,
    /// A number does not fit its field.
    #[error("profile row holds a number out of range")]
    Range,
    /// More runs or buckets than a ring holds.
    #[error("profile row holds more runs or buckets than a ring")]
    TooMany,
    /// Bytes follow the last run.
    #[error("profile row has bytes after its last run")]
    Trailing,
}

/// A decoded profile row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The day (since the Unix epoch) the key was last used.
    pub last_used_day: u64,
    /// The key's runs.
    pub ring: Ring,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let low = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// The stored form of `ring`, last used on `last_used_day`.
#[must_use]
pub fn encode(last_used_day: u64, ring: &Ring) -> Vec<u8> {
    let mut out = vec![ROW_VERSION];
    put_varint(&mut out, last_used_day);
    // The ring holds at most RING_RUNS runs, each of at most BUCKETS buckets.
    out.push(ring.len() as u8);
    for run in ring.runs() {
        put_varint(&mut out, u64::from(run.wall_minutes));
        out.push(run.flags);
        out.push(run.buckets.len() as u8);
        for bucket in &run.buckets {
            match bucket {
                None => put_varint(&mut out, 0),
                Some(b) => {
                    put_varint(&mut out, u64::from(b.cores_milli) + 1);
                    put_varint(&mut out, b.ws_bytes);
                }
            }
        }
    }
    out
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, CodecError> {
        let (&first, rest) = self.rest.split_first().ok_or(CodecError::Truncated)?;
        self.rest = rest;
        Ok(first)
    }

    fn varint(&mut self) -> Result<u64, CodecError> {
        let mut value = 0u64;
        for i in 0..10 {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            // The tenth byte carries only the top bit of a u64.
            if i == 9 && low > 1 {
                return Err(CodecError::Range);
            }
            value |= low << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(CodecError::Range)
    }
}

/// The row stored in `bytes`.
///
/// # Errors
/// [`CodecError`] when `bytes` is not a version-1 row.
pub fn decode(bytes: &[u8]) -> Result<Row, CodecError> {
    let mut r = Reader { rest: bytes };
    let version = r.byte()?;
    if version != ROW_VERSION {
        return Err(CodecError::Version(version));
    }
    let last_used_day = r.varint()?;
    let runs = usize::from(r.byte()?);
    if runs > RING_RUNS {
        return Err(CodecError::TooMany);
    }
    let mut ring = Ring::new();
    for _ in 0..runs {
        let wall_minutes = u32::try_from(r.varint()?).map_err(|_| CodecError::Range)?;
        let flags = r.byte()?;
        let count = usize::from(r.byte()?);
        if count > BUCKETS {
            return Err(CodecError::TooMany);
        }
        let mut buckets = Vec::with_capacity(count);
        for _ in 0..count {
            buckets.push(match r.varint()? {
                0 => None,
                cores => Some(Bucket {
                    cores_milli: u32::try_from(cores - 1).map_err(|_| CodecError::Range)?,
                    ws_bytes: r.varint()?,
                }),
            });
        }
        ring.push_run(Run {
            wall_minutes,
            flags,
            buckets,
        });
    }
    if !r.rest.is_empty() {
        return Err(CodecError::Trailing);
    }
    Ok(Row {
        last_used_day,
        ring,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::RunSeries;
    use crate::ring::tests::series;

    fn sample() -> Ring {
        let mut ring = Ring::new();
        ring.push(&series(&[(100, 1 << 40), (250, 7)]));
        let mut gappy = series(&[(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)]);
        gappy.minutes.retain(|m| m.minute != 2);
        gappy.flags = 0b11;
        ring.push(&gappy);
        ring.push(&series(&[(u32::MAX, u64::MAX)]));
        ring
    }

    /// Catches: a field lost or reordered on the way through, including the unused flag
    /// bits, a hole in a run, and the extremes of the number ranges.
    #[test]
    fn a_row_round_trips_with_its_flags_holes_and_extremes() {
        let ring = sample();
        let bytes = encode(20_000, &ring);
        let row = decode(&bytes).unwrap();
        assert_eq!(row.last_used_day, 20_000);
        assert_eq!(row.ring, ring);
        assert_eq!(row.ring.runs().nth(1).unwrap().flags, 0b11);
        assert_eq!(
            decode(&encode(u64::MAX, &Ring::new()))
                .unwrap()
                .last_used_day,
            u64::MAX
        );
    }

    /// Catches: a changed byte layout (a stored row must stay readable across releases).
    #[test]
    fn the_bytes_are_pinned() {
        let mut ring = Ring::new();
        ring.push(&series(&[(100, 300), (0, 0)]));
        assert_eq!(
            encode(129, &ring),
            vec![
                1, // version
                0x81, 0x01, // day 129
                1,    // one run
                2,    // wall minutes
                0,    // flags
                2,    // buckets
                101, 0xac, 0x02, // minute 0: cores 100 + 1, ws 300
                1, 0, // minute 1: cores 0 + 1, ws 0
            ]
        );
    }

    /// Catches: a row of another version being read as this one.
    #[test]
    fn another_version_is_refused() {
        let mut bytes = encode(1, &sample());
        bytes[0] = 2;
        assert_eq!(decode(&bytes), Err(CodecError::Version(2)));
        assert_eq!(decode(&[0]), Err(CodecError::Version(0)));
    }

    /// Catches: a short value decoding to a partial row, at every cut point.
    #[test]
    fn a_truncated_value_is_refused_at_every_length() {
        let bytes = encode(300, &sample());
        for cut in 0..bytes.len() {
            assert_eq!(
                decode(&bytes[..cut]),
                Err(CodecError::Truncated),
                "cut at {cut}"
            );
        }
        assert!(decode(&bytes).is_ok());
    }

    #[test]
    fn bytes_after_the_last_run_are_refused() {
        let mut bytes = encode(1, &sample());
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(CodecError::Trailing));
    }

    /// Catches: a corrupt count making the decoder build a ring no writer could.
    #[test]
    fn more_runs_or_buckets_than_a_ring_holds_are_refused() {
        assert_eq!(decode(&[1, 0, 33]), Err(CodecError::TooMany));
        let mut ring = Ring::new();
        ring.push(&RunSeries {
            minutes: series(&[(1, 1)]).minutes,
            flags: 0,
        });
        let mut bytes = encode(0, &ring);
        // [version, day, runs, wall, flags, buckets, ...]
        bytes[5] = 65;
        assert_eq!(decode(&bytes), Err(CodecError::TooMany));
        bytes[5] = 64;
        assert_eq!(decode(&bytes), Err(CodecError::Truncated));
    }

    /// Catches: a varint of eleven bytes or with bits past the 64th wrapping silently, and
    /// numbers too large for their field being cut to fit.
    #[test]
    fn numbers_out_of_range_are_refused() {
        // A day that does not end in ten bytes, and one whose tenth byte overflows u64.
        assert_eq!(
            decode(&[
                1, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80
            ]),
            Err(CodecError::Range)
        );
        assert_eq!(
            decode(&[
                1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02
            ]),
            Err(CodecError::Range)
        );
        // Wall minutes past u32.
        assert_eq!(
            decode(&[1, 0, 1, 0x80, 0x80, 0x80, 0x80, 0x10, 0, 0]),
            Err(CodecError::Range)
        );
        // Cores past u32 (varint 2^32 + 1 = cores 2^32).
        assert_eq!(
            decode(&[1, 0, 1, 1, 0, 1, 0x81, 0x80, 0x80, 0x80, 0x10, 0]),
            Err(CodecError::Range)
        );
    }
}
