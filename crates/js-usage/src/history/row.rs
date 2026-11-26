//! The stored form of an [`EndedRow`].
//!
//! Layout, version 1, in this order: the version byte; the id (text); the profile key
//! (optional text); the host (optional text); what was booked (optional: a byte 1 for the
//! whole host or 0, then the CPU and the memory as varints); the queue wait, the run time
//! (each an optional varint); the ended reason and the drain outcome (each its name as text);
//! the give-ups, the estimated end (each an optional varint); the actual end (a varint); the
//! estimate error (an optional zigzag varint); what was used (optional: the highest CPU, the
//! highest memory and the mean CPU as varints); then, only when the row has one, the CPU
//! series: a byte 1, the granularity in seconds, the run length in microseconds (each a varint),
//! the partial flag (a byte, 0 or 1), the number of points (a varint) and each point (a
//! varint), written as a byte 0 when the row has none but has a class; then, only when the row
//! has one, the class (a byte 1 and the text, any UTF-8, kept as it is). A row without a series
//! or a class ends after `used`, the bytes it was before those fields; a reader that finds bytes
//! after `used` reads a series, and a byte 0 there is an absent one, and one that finds bytes
//! after the series reads a class.
//!
//! A text is a varint length and that many bytes of UTF-8. An optional field is a byte 0
//! (absent) or 1 followed by the field. A varint is little-endian base 128, 7 bits a byte, high
//! bit set on all but the last; a zigzag varint is the varint of `(n << 1) ^ (n >> 63)`.
//!
//! A field is added at the end, under the same version; the reader ignores the bytes after the
//! fields it knows, so an older build reads a newer row. The version changes only when a
//! field already written changes meaning, and a row of a version this build does not read is
//! refused with [`EndedRowError::Version`].

use js_types::{CpuSeries, HostId};

use super::{AllocationId, Booked, DrainOutcome, EndedReason, EndedRow, Used};
use crate::codec::put_varint;
use crate::key::ProfileKey;

/// The version byte this build writes and reads.
pub const ENDED_ROW_VERSION: u8 = 1;

/// Why stored bytes are not an ended-allocation row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EndedRowError {
    /// The version byte is not [`ENDED_ROW_VERSION`].
    #[error("ended-allocation row version {0} is not read by this build")]
    Version(u8),
    /// The bytes end inside a field.
    #[error("ended-allocation row is truncated")]
    Truncated,
    /// A number does not fit its field.
    #[error("ended-allocation row holds a number out of range")]
    Range,
    /// The named field holds a value this build does not read.
    #[error("ended-allocation row holds a bad {0}")]
    Field(&'static str),
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

fn put_opt<T>(out: &mut Vec<u8>, value: Option<T>, put: impl FnOnce(&mut Vec<u8>, T)) {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            put(out, value);
        }
    }
}

fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn unzigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// The stored form of `row`.
#[must_use]
pub(crate) fn encode(row: &EndedRow) -> Vec<u8> {
    let mut out = vec![ENDED_ROW_VERSION];
    put_text(&mut out, row.id.as_str());
    put_opt(&mut out, row.profile.as_ref(), |out, key| {
        put_text(out, key.as_str());
    });
    put_opt(&mut out, row.host.as_ref(), |out, host| {
        put_text(out, host.as_str());
    });
    put_opt(&mut out, row.booked, |out, booked| {
        out.push(u8::from(booked.whole_host));
        put_varint(out, booked.cpu_milli);
        put_varint(out, booked.memory_bytes);
    });
    put_opt(&mut out, row.queued_ms, put_varint);
    put_opt(&mut out, row.run_ms, put_varint);
    put_text(&mut out, row.ended_reason.name());
    put_text(&mut out, row.drain_outcome.name());
    put_opt(&mut out, row.give_up_outcome.map(u64::from), put_varint);
    put_opt(&mut out, row.estimated_end, put_varint);
    put_varint(&mut out, row.actual_end);
    put_opt(&mut out, row.error_ms.map(zigzag), put_varint);
    put_opt(&mut out, row.used, |out, used| {
        put_varint(out, used.cpu_milli);
        put_varint(out, used.memory_bytes);
        put_varint(out, used.cpu_mean_milli);
    });
    let series = row
        .cpu_series
        .as_ref()
        .filter(|series| series.is_well_formed());
    if series.is_some() || row.class.is_some() {
        put_opt(&mut out, series, |out, series| {
            put_varint(out, u64::from(series.granularity_secs));
            put_varint(out, series.run_usec);
            out.push(u8::from(series.partial));
            put_varint(out, series.points.len() as u64);
            for point in &series.points {
                put_varint(out, u64::from(*point));
            }
        });
    }
    if let Some(class) = &row.class {
        put_opt(&mut out, Some(class), |out, class| put_text(out, class));
    }
    out
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, EndedRowError> {
        let (&first, rest) = self.rest.split_first().ok_or(EndedRowError::Truncated)?;
        self.rest = rest;
        Ok(first)
    }

    fn varint(&mut self) -> Result<u64, EndedRowError> {
        let mut value = 0u64;
        for i in 0..10 {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            // The tenth byte carries only the top bit of a u64.
            if i == 9 && low > 1 {
                return Err(EndedRowError::Range);
            }
            value |= low << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(EndedRowError::Range)
    }

    fn text(&mut self, field: &'static str) -> Result<&'a str, EndedRowError> {
        let len = self.varint()?;
        if len > self.rest.len() as u64 {
            return Err(EndedRowError::Truncated);
        }
        let (text, rest) = self.rest.split_at(len as usize);
        self.rest = rest;
        std::str::from_utf8(text).map_err(|_| EndedRowError::Field(field))
    }

    /// An optional field: absent, or read by `read`.
    fn opt<T>(
        &mut self,
        field: &'static str,
        read: impl FnOnce(&mut Self) -> Result<T, EndedRowError>,
    ) -> Result<Option<T>, EndedRowError> {
        match self.byte()? {
            0 => Ok(None),
            1 => read(self).map(Some),
            _ => Err(EndedRowError::Field(field)),
        }
    }
}

/// The row stored in `bytes`.
///
/// # Errors
/// [`EndedRowError`] when `bytes` is not a version-1 row.
pub(crate) fn decode(bytes: &[u8]) -> Result<EndedRow, EndedRowError> {
    let mut r = Reader { rest: bytes };
    let version = r.byte()?;
    if version != ENDED_ROW_VERSION {
        return Err(EndedRowError::Version(version));
    }
    let bad = EndedRowError::Field;
    let id = AllocationId::parse(r.text("id")?).map_err(|_| bad("id"))?;
    let profile = r.opt("profile", |r| {
        ProfileKey::parse(r.text("profile")?).map_err(|_| bad("profile"))
    })?;
    let host = r.opt("host", |r| {
        HostId::new(r.text("host")?).map_err(|_| bad("host"))
    })?;
    let booked = r.opt("booked", |r| {
        let whole_host = match r.byte()? {
            0 => false,
            1 => true,
            _ => return Err(bad("booked")),
        };
        Ok(Booked {
            whole_host,
            cpu_milli: r.varint()?,
            memory_bytes: r.varint()?,
        })
    })?;
    let queued_ms = r.opt("queued_ms", Reader::varint)?;
    let run_ms = r.opt("run_ms", Reader::varint)?;
    let reason = r.text("ended_reason")?;
    let ended_reason = EndedReason::from_name(reason).ok_or(bad("ended_reason"))?;
    let drain = r.text("drain_outcome")?;
    let drain_outcome = DrainOutcome::from_name(drain).ok_or(bad("drain_outcome"))?;
    let give_up_outcome = r.opt("give_up_outcome", |r| {
        u32::try_from(r.varint()?).map_err(|_| EndedRowError::Range)
    })?;
    let estimated_end = r.opt("estimated_end", Reader::varint)?;
    let actual_end = r.varint()?;
    let error_ms = r.opt("error_ms", Reader::varint)?.map(unzigzag);
    let used = r.opt("used", |r| {
        Ok(Used {
            cpu_milli: r.varint()?,
            memory_bytes: r.varint()?,
            cpu_mean_milli: r.varint()?,
        })
    })?;
    // A row from before the series ends here; bytes after the series are fields a later build
    // added.
    let cpu_series = if r.rest.is_empty() {
        None
    } else {
        r.opt("cpu_series", series)?
    };
    // A row from before the class ends here.
    let class = if r.rest.is_empty() {
        None
    } else {
        r.opt("class", |r| r.text("class").map(str::to_owned))?
    };
    Ok(EndedRow {
        id,
        profile,
        host,
        booked,
        queued_ms,
        run_ms,
        ended_reason,
        drain_outcome,
        give_up_outcome,
        estimated_end,
        actual_end,
        error_ms,
        used,
        cpu_series,
        class,
    })
}

/// The series at the reader: granularity, run length, partial flag, then the points.
fn series(r: &mut Reader) -> Result<CpuSeries, EndedRowError> {
    let granularity = u32::try_from(r.varint()?).map_err(|_| EndedRowError::Range)?;
    let run_usec = r.varint()?;
    let partial = match r.byte()? {
        0 => false,
        1 => true,
        _ => return Err(EndedRowError::Field("cpu_series")),
    };
    let count = r.varint()?;
    let points = (0..count)
        .map(|_| u16::try_from(r.varint()?).map_err(|_| EndedRowError::Range))
        .collect::<Result<Vec<_>, _>>()?;
    CpuSeries::new(granularity, points, run_usec, partial).ok_or(EndedRowError::Field("cpu_series"))
}
