//! The x2 export: the runs and node minutes a usage store holds, written as the two text
//! files the x2 replay of `js-sched` reads (`examples/sim_x2.rs` there, the format in its
//! `tests/sim_x_replay/engine/export.rs`).
//!
//! # Where the runs come from
//!
//! A run is a row of the ended-allocation history ([`EndedHistory`]), not a run of a key's
//! profile ring: a ring run carries no start, and a replay without starts has one run in
//! the pool at a time and compares nothing. A history row carries what the replay needs:
//! when the run started (`actual_end - run_ms`), what it is (its class, or its profile key
//! when it has no class), the host it ran on, the CPU second by second its host reported
//! ([`js_types::CpuSeries`]) and the largest minute of its working set ([`Used`]).
//!
//! A row is a run of the export when it ended [`EndedReason::Released`], names a class or a
//! profile key, was on a host, ran at least a millisecond, has what it used and a CPU series
//! that starts with the run and has a point. Every other row is counted under the first of
//! those it misses ([`X2Skipped`]).
//!
//! # What a run's lines say
//!
//! * The key is the FNV-1a digest of the class (or profile key) text, so the file names no
//!   job and no class.
//! * The platform is [`X2_PLATFORM`]: a history row does not carry one, and the replay
//!   does not read it.
//! * The start is in milliseconds from the export's zero: the minute of the earliest
//!   start of a run of the export.
//! * Minute `m` of a run holds the mean of the series over the seconds of the run in that
//!   minute (a point covers `granularity_secs` seconds, the last point what is left of the
//!   run), in millicores, rounded down. A minute with no second of the run (the last of a
//!   run of a whole number of minutes) holds the last point.
//! * The working set of every minute is the run's largest ([`Used::memory_bytes`]): the
//!   history keeps no memory series, so the export's memory is flat at the peak.
//!
//! # Node rows
//!
//! The node minutes of every host a run of the export ran on, from the export's zero to
//! the last minute before `before_ms`. A host is numbered by its place among those hosts in
//! the order of their ids, and a minute is counted from the export's zero.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::num::NonZeroUsize;

use js_state::Store;
use js_types::HostId;

use crate::error::UsageError;
use crate::history::{EndedHistory, EndedQuery, EndedReason, EndedRow};
use crate::store::NodeSeries;

/// The platform token every run of an export carries.
pub const X2_PLATFORM: &str = "any";

/// Milliseconds in a minute.
const MINUTE_MS: u64 = 60_000;

/// Sixteenths of a core in a core, the unit of a CPU series point.
const SIXTEENTHS: u64 = 16;

/// History rows read a page at a time.
const PAGE: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

/// How many history rows were left out of an export, by the first thing each missed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct X2Skipped {
    /// It did not end [`EndedReason::Released`].
    pub not_released: u64,
    /// It names neither a class nor a profile key.
    pub no_key: u64,
    /// It had no host.
    pub no_host: u64,
    /// Its run time is unknown or zero.
    pub no_run_time: u64,
    /// Nothing was measured of what it used.
    pub no_usage: u64,
    /// It has no CPU series.
    pub no_series: u64,
    /// Its CPU series misses the start of the run, or has no point.
    pub partial_series: u64,
}

/// What an export holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct X2Counts {
    /// Runs written.
    pub runs: u64,
    /// Their lines: one per minute of each run.
    pub run_minutes: u64,
    /// Node lines written.
    pub node_minutes: u64,
    /// Hosts those lines are of.
    pub nodes: u64,
    /// History rows left out.
    pub skipped: X2Skipped,
}

/// The two files of an export, and what they hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct X2Export {
    /// The text of `key_profile.txt`: one `kp` line per minute of every run.
    pub key_profile: String,
    /// The text of `node_minute.txt`: one `nm` line per node minute.
    pub node_minute: String,
    /// What they hold.
    pub counts: X2Counts,
}

/// One run of the export.
struct Run {
    digest: u64,
    start_ms: u64,
    wall_ms: u64,
    host: HostId,
    cores_milli: Vec<u64>,
    ws_bytes: u64,
}

/// FNV-1a over `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The mean of `points` (each `granularity` seconds, the last one the rest of the run)
/// over every minute of a run of `wall_ms`, in millicores: `wall_ms / 60,000 + 1` minutes.
/// `points` is not empty.
fn minute_means(points: &[u16], granularity: u64, wall_ms: u64) -> Vec<u64> {
    let seconds = wall_ms.div_ceil(1_000);
    let last = points.len() - 1;
    let at = |second: u64| {
        let i = usize::try_from(second / granularity).map_or(last, |i| i.min(last));
        u64::from(points[i])
    };
    (0..=wall_ms / MINUTE_MS)
        .map(|minute| {
            let from = minute * 60;
            let to = (from + 60).min(seconds);
            if from >= to {
                return at(u64::MAX) * 1_000 / SIXTEENTHS;
            }
            let sum: u64 = (from..to).map(at).sum();
            sum * 1_000 / SIXTEENTHS / (to - from)
        })
        .collect()
}

/// The run `row` makes, or the counter of the first thing it misses.
fn run_of(row: &EndedRow, skipped: &mut X2Skipped) -> Option<Run> {
    let miss = |counter: &mut u64| {
        *counter += 1;
        None
    };
    if row.ended_reason != EndedReason::Released {
        return miss(&mut skipped.not_released);
    }
    let Some(key) = row
        .class
        .as_deref()
        .or(row.profile.as_ref().map(|p| p.as_str()))
    else {
        return miss(&mut skipped.no_key);
    };
    let Some(host) = &row.host else {
        return miss(&mut skipped.no_host);
    };
    let Some(wall_ms) = row.run_ms.filter(|&ms| ms > 0) else {
        return miss(&mut skipped.no_run_time);
    };
    let Some(used) = row.used else {
        return miss(&mut skipped.no_usage);
    };
    let Some(series) = &row.cpu_series else {
        return miss(&mut skipped.no_series);
    };
    if series.partial || series.points.is_empty() {
        return miss(&mut skipped.partial_series);
    }
    Some(Run {
        digest: fnv1a(key.as_bytes()),
        start_ms: row.actual_end.saturating_sub(wall_ms),
        wall_ms,
        host: host.clone(),
        cores_milli: minute_means(&series.points, u64::from(series.granularity_secs), wall_ms),
        ws_bytes: used.memory_bytes,
    })
}

/// The export of the allocations that ended in `from_ms..before_ms` (milliseconds since the
/// Unix epoch), and the node minutes of their hosts. It writes nothing to the store.
///
/// # Errors
/// [`UsageError`] when the store fails or a stored node hour does not read.
pub fn x2_export(store: &dyn Store, from_ms: u64, before_ms: u64) -> Result<X2Export, UsageError> {
    paged(store, from_ms, before_ms, PAGE)
}

/// [`x2_export`], reading the history `page` rows at a time.
fn paged(
    store: &dyn Store,
    from_ms: u64,
    before_ms: u64,
    page: NonZeroUsize,
) -> Result<X2Export, UsageError> {
    let history = EndedHistory::new(store);
    let mut counts = X2Counts::default();
    let mut runs = Vec::new();
    let mut query = EndedQuery {
        after: None,
        from_ms,
        before_ms: Some(before_ms),
        host: None,
        id: None,
        limit: page,
    };
    loop {
        let page = history.page(&query)?;
        runs.extend(
            page.rows
                .iter()
                .filter_map(|row| run_of(row, &mut counts.skipped)),
        );
        match page.next {
            Some(next) => query.after = Some(next),
            None => break,
        }
    }
    runs.sort_by_key(|r| r.start_ms);
    let zero_minute = runs.first().map_or(0, |r| r.start_ms / MINUTE_MS);
    let zero_ms = zero_minute * MINUTE_MS;

    let mut key_profile = String::new();
    for run in &runs {
        let start = run.start_ms - zero_ms;
        for (minute, cores) in run.cores_milli.iter().enumerate() {
            let _ = writeln!(
                key_profile,
                "kp {:016x} {X2_PLATFORM} {start} {} {minute} {cores} {}",
                run.digest, run.wall_ms, run.ws_bytes
            );
        }
        counts.run_minutes += run.cores_milli.len() as u64;
    }
    counts.runs = runs.len() as u64;

    let hosts: BTreeSet<&HostId> = runs.iter().map(|r| &r.host).collect();
    let mut node_minute = String::new();
    let series = NodeSeries::new(store);
    let last_minute = before_ms.saturating_sub(1) / MINUTE_MS;
    for (node, host) in hosts.iter().enumerate() {
        let rows = series.range(host, zero_minute, last_minute, NonZeroUsize::MAX)?;
        for row in &rows {
            let _ = writeln!(
                node_minute,
                "nm {node} {} {} {} {}",
                row.minute - zero_minute,
                row.cpu_millis,
                row.ws_max_bytes,
                row.leases
            );
        }
        counts.node_minutes += rows.len() as u64;
    }
    counts.nodes = hosts.len() as u64;
    Ok(X2Export {
        key_profile,
        node_minute,
        counts,
    })
}

#[cfg(test)]
mod tests;
