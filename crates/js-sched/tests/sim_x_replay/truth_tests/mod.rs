//! Tests of the truth model: the integer tables, the keyed draws and the run built from
//! a row. Each test names the defect it would catch. None reads a clock or asserts a
//! timing, and no assertion reads a float.

use crate::engine::fixture::{KeyIx, Outcome, Row};

mod keyed;
mod sanity;
mod series;
mod tables;

/// A row of one key, started at 0, that ended `ok`.
fn row(key: u16, wall_ms: u64, cpu_ms: u64) -> Row {
    Row {
        start_ms: 0,
        key: KeyIx(key),
        wall_ms,
        cpu_ms,
        peak_mem_mib: 512,
        outcome: Outcome::Ok,
    }
}
