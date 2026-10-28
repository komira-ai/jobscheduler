//! The replay fixture: one record per executed action, scrubbed to what a placement
//! replay needs, and the loader, encoder and types the rest of the engine builds on.
//!
//! # Format
//!
//! Little endian, no compression, records sorted by start (ties keep the order in
//! which the actions completed).
//!
//! ```text
//! magic "JSX1" | u32 n_rows | u32 n_keys | records... | u64 FNV-1a of every preceding byte
//! record: varint start_delta_ms | u16 key_ix | varint wall_ms | varint cpu_ms
//!         | varint peak_mem_mib | u8 outcome
//! ```
//!
//! A varint is LEB128 (seven bits per byte, low group first) in its shortest form.
//! `start_delta_ms` is the gap to the previous record's start; the first record starts
//! at 0. `key_ix` numbers the distinct work keys densely from 0 in order of first
//! appearance, so the original digests are not kept. `outcome` is an [`Outcome`] code.
//! Nothing else about an action is in the file: no host, target, lease, operation or
//! absolute time.
//!
//! # Coarsening
//!
//! A value precise to the millisecond or the byte could single out one action, so the
//! file holds fixed points of [`coarse_ms`] (wall and cpu) and [`coarse_mem_mib`] (peak
//! memory). Wall is at least 1 ms. The functions are public so a test can check that
//! the committed file is coarse and a converter can be held to them.
//!
//! # What the loader checks
//!
//! That the file is consistent with itself: magic, record framing, outcome codes,
//! wall at least 1 ms, a first start of 0, keys in range and first appearing in order,
//! the header's key count equal to the keys seen, the hash, and no bytes after it. It also
//! refuses a wall, cpu or peak memory that is not its own coarsening, so a file written
//! without coarsening does not load. The row and key counts of any particular file are
//! data, never constants of the code.

use std::fmt;

/// The four bytes that open a fixture.
pub const MAGIC: [u8; 4] = *b"JSX1";

/// Bytes before the first record: the magic and the two counts.
const HEADER_LEN: usize = 12;

/// Bytes of the trailing hash.
const HASH_LEN: usize = 8;

/// Peak memory is clamped to this many MiB (64 GiB).
pub const MEM_CLAMP_MIB: u64 = 65_536;

/// Above this many MiB (1 GiB) peak memory is rounded.
pub const MEM_ROUND_ABOVE_MIB: u64 = 1_024;

/// Peak memory above [`MEM_ROUND_ABOVE_MIB`] is a multiple of this many MiB.
pub const MEM_ROUND_MIB: u64 = 4;

/// Wall and cpu above this many ms keep three significant figures.
pub const SIG_FIGS_ABOVE_MS: u64 = 10_000;

/// The committed fixture, compiled into every unit that includes the engine.
pub const EMBEDDED: &[u8] = include_bytes!("../../fixtures/sim_x_trace.bin");

/// A work key: the dense index of one distinct key in a fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyIx(pub u16);

/// How an action ended. The code is the byte in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// Ran to completion.
    Ok,
    /// Killed for exceeding its memory.
    KilledMemory,
    /// Stopped at its time limit.
    TimedOut,
    /// Removed from its host under pressure.
    Evicted,
}

impl Outcome {
    /// Every outcome, in code order.
    pub const ALL: [Outcome; 4] = [
        Outcome::Ok,
        Outcome::KilledMemory,
        Outcome::TimedOut,
        Outcome::Evicted,
    ];

    /// The byte that stands for this outcome in the file.
    pub fn code(self) -> u8 {
        match self {
            Outcome::Ok => 0,
            Outcome::KilledMemory => 1,
            Outcome::TimedOut => 2,
            Outcome::Evicted => 3,
        }
    }

    /// The outcome a file byte stands for.
    pub fn from_code(code: u8) -> Option<Outcome> {
        Outcome::ALL.into_iter().find(|o| o.code() == code)
    }
}

/// One executed action.
///
/// The cores it used on average are `cpu_ms / wall_ms`; `wall_ms` is never 0 in a loaded
/// fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// When it started, in ms from the earliest start in the fixture.
    pub start_ms: u64,
    /// Its work key.
    pub key: KeyIx,
    /// Wall time, ms.
    pub wall_ms: u64,
    /// CPU time over the run, ms.
    pub cpu_ms: u64,
    /// Peak resident memory, MiB.
    pub peak_mem_mib: u64,
    /// How it ended.
    pub outcome: Outcome,
}

/// A loaded fixture: rows in start order and what the file said about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    /// The records, sorted by `start_ms`.
    pub rows: Vec<Row>,
    /// The header's key count: keys are `KeyIx(0)` to `KeyIx(n_keys - 1)`.
    pub n_keys: u32,
    /// The file's FNV-1a hash (its last eight bytes), which identifies the trace.
    pub hash: u64,
}

impl Fixture {
    /// Reads the committed fixture.
    pub fn embedded() -> Result<Fixture, FixtureError> {
        Fixture::load(EMBEDDED)
    }

    /// Reads and checks a fixture; see the module comment for what is checked.
    pub fn load(bytes: &[u8]) -> Result<Fixture, FixtureError> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(4)? != MAGIC {
            return Err(FixtureError::BadMagic);
        }
        let n_rows = r.u32()?;
        let n_keys = r.u32()?;
        if u64::from(n_keys) > u64::from(u16::MAX) + 1 {
            return Err(FixtureError::TooManyKeys { n_keys });
        }
        // Every record is at least seven bytes, so a larger count is a lie that would
        // otherwise reserve memory before the first read failed.
        let reserve = usize::try_from(n_rows).map_or(0, |n| n.min(bytes.len() / 7));
        let mut rows = Vec::with_capacity(reserve);
        let mut start_ms = 0u64;
        let mut seen = 0u32;
        for row in 0..n_rows {
            let delta = r.varint()?;
            if row == 0 && delta != 0 {
                return Err(FixtureError::FirstStartNotZero { delta });
            }
            start_ms = start_ms
                .checked_add(delta)
                .ok_or(FixtureError::StartOverflow { row })?;
            let key = r.u16()?;
            let wall_ms = r.varint()?;
            let cpu_ms = r.varint()?;
            let peak_mem_mib = r.varint()?;
            let code = r.u8()?;
            let outcome = Outcome::from_code(code).ok_or(FixtureError::BadOutcome { row, code })?;
            if wall_ms == 0 {
                return Err(FixtureError::ZeroWall { row });
            }
            for (field, value, coarse) in [
                ("wall", wall_ms, coarse_ms(wall_ms)),
                ("cpu", cpu_ms, coarse_ms(cpu_ms)),
                ("peak memory", peak_mem_mib, coarse_mem_mib(peak_mem_mib)),
            ] {
                if value != coarse {
                    return Err(FixtureError::Uncoarsened {
                        row,
                        field,
                        value,
                        coarse,
                    });
                }
            }
            if u32::from(key) >= n_keys {
                return Err(FixtureError::KeyOutOfRange { row, key, n_keys });
            }
            if u32::from(key) > seen {
                return Err(FixtureError::KeyOrder {
                    row,
                    key,
                    expected: seen,
                });
            }
            if u32::from(key) == seen {
                seen += 1;
            }
            rows.push(Row {
                start_ms,
                key: KeyIx(key),
                wall_ms,
                cpu_ms,
                peak_mem_mib,
                outcome,
            });
        }
        if seen != n_keys {
            return Err(FixtureError::KeyCount {
                header: n_keys,
                seen,
            });
        }
        let computed = fnv1a(&bytes[..r.at]);
        let hash = r.u64()?;
        if hash != computed {
            return Err(FixtureError::BadHash {
                stored: hash,
                computed,
            });
        }
        if r.at != bytes.len() {
            return Err(FixtureError::TrailingBytes {
                extra: bytes.len() - r.at,
            });
        }
        Ok(Fixture { rows, n_keys, hash })
    }
}

/// Writes `rows` (sorted by `start_ms`, the first at 0) in the fixture format, with
/// `n_keys` set to one more than the largest key. The result is not checked: a test makes
/// an invalid file by encoding invalid rows or by editing the bytes and calling [`seal`].
pub fn encode(rows: &[Row]) -> Vec<u8> {
    let n_keys = rows
        .iter()
        .map(|r| u32::from(r.key.0) + 1)
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(
        &u32::try_from(rows.len())
            .expect("fewer than 2^32 rows")
            .to_le_bytes(),
    );
    out.extend_from_slice(&n_keys.to_le_bytes());
    let mut prev = 0u64;
    for r in rows {
        assert!(r.start_ms >= prev, "rows must be sorted by start");
        put_varint(&mut out, r.start_ms - prev);
        prev = r.start_ms;
        out.extend_from_slice(&r.key.0.to_le_bytes());
        put_varint(&mut out, r.wall_ms);
        put_varint(&mut out, r.cpu_ms);
        put_varint(&mut out, r.peak_mem_mib);
        out.push(r.outcome.code());
    }
    out.extend_from_slice(&[0; HASH_LEN]);
    seal(&mut out);
    out
}

/// Replaces the last eight bytes of `bytes` with the FNV-1a hash of the rest.
pub fn seal(bytes: &mut Vec<u8>) {
    let body = bytes.len().saturating_sub(HASH_LEN);
    bytes.truncate(body);
    let hash = fnv1a(bytes);
    bytes.extend_from_slice(&hash.to_le_bytes());
}

/// FNV-1a, 64 bit.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A wall or cpu time in ms as the fixture stores it: unchanged up to
/// [`SIG_FIGS_ABOVE_MS`], else rounded half up to three significant figures.
pub fn coarse_ms(ms: u64) -> u64 {
    if ms <= SIG_FIGS_ABOVE_MS {
        return ms;
    }
    let scale = 10u64.pow(ms.ilog10() - 2);
    (ms + scale / 2) / scale * scale
}

/// A peak memory in MiB as the fixture stores it: clamped to [`MEM_CLAMP_MIB`], and above
/// [`MEM_ROUND_ABOVE_MIB`] rounded up to a multiple of [`MEM_ROUND_MIB`].
pub fn coarse_mem_mib(mib: u64) -> u64 {
    let mib = mib.min(MEM_CLAMP_MIB);
    if mib > MEM_ROUND_ABOVE_MIB {
        mib.div_ceil(MEM_ROUND_MIB) * MEM_ROUND_MIB
    } else {
        mib
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A cursor over the bytes of a file.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FixtureError> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or(FixtureError::Truncated { at: self.at })?;
        let got = &self.bytes[self.at..end];
        self.at = end;
        Ok(got)
    }

    fn u8(&mut self) -> Result<u8, FixtureError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, FixtureError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, FixtureError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, FixtureError> {
        let b = self.take(HASH_LEN)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A shortest-form LEB128 `u64`; a longer form or a value past 64 bits is refused.
    fn varint(&mut self) -> Result<u64, FixtureError> {
        let start = self.at;
        let mut v = 0u64;
        for i in 0..10u32 {
            let b = self.u8()?;
            let low = u64::from(b & 0x7f);
            if i == 9 && low > 1 {
                return Err(FixtureError::BadVarint { at: start });
            }
            v |= low << (7 * i);
            if b & 0x80 == 0 {
                if i > 0 && b == 0 {
                    return Err(FixtureError::BadVarint { at: start });
                }
                return Ok(v);
            }
        }
        Err(FixtureError::BadVarint { at: start })
    }
}

/// Why a file is not a fixture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureError {
    /// The file does not start with [`MAGIC`].
    BadMagic,
    /// The file ends inside the header, a record or the hash.
    Truncated {
        /// Offset of the read that ran out.
        at: usize,
    },
    /// A varint is longer than needed or wider than 64 bits.
    BadVarint {
        /// Offset of its first byte.
        at: usize,
    },
    /// The header names more keys than a `u16` index holds.
    TooManyKeys {
        /// The header's count.
        n_keys: u32,
    },
    /// The first record does not start at 0.
    FirstStartNotZero {
        /// Its start delta.
        delta: u64,
    },
    /// Starts add up past 64 bits.
    StartOverflow {
        /// The record that overflowed.
        row: u32,
    },
    /// An outcome byte that is no [`Outcome`].
    BadOutcome {
        /// The record.
        row: u32,
        /// The byte.
        code: u8,
    },
    /// A wall time of 0.
    ZeroWall {
        /// The record.
        row: u32,
    },
    /// A wall, cpu or peak memory that is not a fixed point of its coarsening.
    Uncoarsened {
        /// The record.
        row: u32,
        /// Which field: "wall", "cpu" or "peak memory".
        field: &'static str,
        /// The value in the file.
        value: u64,
        /// What the coarsening makes of it.
        coarse: u64,
    },
    /// A key at or past the header's count.
    KeyOutOfRange {
        /// The record.
        row: u32,
        /// Its key.
        key: u16,
        /// The header's count.
        n_keys: u32,
    },
    /// A key that skips ahead of the next unseen index.
    KeyOrder {
        /// The record.
        row: u32,
        /// Its key.
        key: u16,
        /// The next unseen index.
        expected: u32,
    },
    /// The header's key count is not the number of keys the records use.
    KeyCount {
        /// The header's count.
        header: u32,
        /// Keys seen.
        seen: u32,
    },
    /// The stored hash is not the hash of the bytes before it.
    BadHash {
        /// In the file.
        stored: u64,
        /// Of the bytes.
        computed: u64,
    },
    /// Bytes after the hash.
    TrailingBytes {
        /// How many.
        extra: usize,
    },
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixtureError::BadMagic => write!(f, "not a fixture: wrong magic"),
            FixtureError::Truncated { at } => write!(f, "truncated at byte {at}"),
            FixtureError::BadVarint { at } => write!(f, "bad varint at byte {at}"),
            FixtureError::TooManyKeys { n_keys } => {
                write!(f, "{n_keys} keys do not fit a u16 index")
            }
            FixtureError::FirstStartNotZero { delta } => write!(f, "first start is {delta}, not 0"),
            FixtureError::StartOverflow { row } => write!(f, "start overflows at row {row}"),
            FixtureError::BadOutcome { row, code } => write!(f, "row {row}: outcome code {code}"),
            FixtureError::ZeroWall { row } => write!(f, "row {row}: zero wall"),
            FixtureError::Uncoarsened {
                row,
                field,
                value,
                coarse,
            } => write!(
                f,
                "row {row}: {field} {value} is not coarse (would be {coarse})"
            ),
            FixtureError::KeyOutOfRange { row, key, n_keys } => {
                write!(f, "row {row}: key {key} but the header has {n_keys} keys")
            }
            FixtureError::KeyOrder { row, key, expected } => {
                write!(f, "row {row}: key {key} before key {expected} appeared")
            }
            FixtureError::KeyCount { header, seen } => {
                write!(f, "header has {header} keys, records use {seen}")
            }
            FixtureError::BadHash { stored, computed } => {
                write!(
                    f,
                    "hash {stored:016x} but the bytes hash to {computed:016x}"
                )
            }
            FixtureError::TrailingBytes { extra } => write!(f, "{extra} bytes after the hash"),
        }
    }
}

impl std::error::Error for FixtureError {}
