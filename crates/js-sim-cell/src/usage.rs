//! The usage history the server wrote for the allocations it ended, as the cell reads it at
//! the end of a scenario.
//!
//! The server writes one row of the history for each allocation it frees, in the same
//! transaction as the free (`usage/ended/<end ms>/<job id>`). The cell reads the rows
//! through the job service's own route, `GET /v1/usage`, a page at a time, as an operator
//! does ([`PAGE_SIZE`] rows a page), and keeps what it read as [`Observations::usage`]. The
//! read is made by [`ServerNode::client_call`] directly, not through the cell's recording
//! port, so it writes no trace line and the trace hashes do not move.
//!
//! The wire row carries less than the stored one: the job, the host, the end time and what
//! the allocation booked (the whole-host flag it shows is the drain outcome of the booking
//! and is unset for a job, so a whole host shows as a booking of no CPU and no memory).
//! [`UsageRow`] keeps those, and [`booked_totals`] says what the allocation side of the same
//! facts is.
//!
//! # A fault on the read
//!
//! The server opens its own store, so a test cannot make the store lose, double or
//! mis-write a row. [`UsageReadFault`] models that on the cell's side: it changes the rows
//! the cell read before it keeps them, as such a store would have shown them, so a test can
//! show that the usage invariants see each defect.

use axum::body::Body;
use axum::http::Request;
use axum::http::header::AUTHORIZATION;
use js_jobs::{Booked, JobId};
use js_types::HostId;
use js_types::sched::FarmTime;
use serde_json::Value;

use crate::observe::Observations;
use crate::server::{CLIENT_TOKEN, ServerNode, virtual_time};

/// How many rows one page of the read holds.
pub const PAGE_SIZE: u32 = 4;

/// One row of the usage history, as `GET /v1/usage` tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRow {
    /// The job the ended allocation booked for.
    pub job: JobId,
    /// The host it booked on.
    pub host: HostId,
    /// When the server freed it, on the cell's virtual clock.
    pub ended_at: FarmTime,
    /// The CPU it booked, in thousandths of a core (none for a whole host).
    pub cpu_millis: u64,
    /// The memory it booked, in bytes (none for a whole host).
    pub memory_bytes: u64,
}

/// A defect of the store's usage rows, applied to the rows the cell read before it keeps
/// them (see the module doc). The rows are in the order of their end, as the route lists
/// them; each fault acts on the first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageReadFault {
    /// The rows as the server told them.
    #[default]
    None,
    /// A store that lost the first row.
    DropRow,
    /// A store that wrote the first row twice.
    DoubleRow,
    /// A store that wrote the first row with one more thousandth of a core booked.
    SkewTotal,
}

impl UsageReadFault {
    /// `rows` as a store with this defect would have shown them.
    #[must_use]
    pub fn apply(self, mut rows: Vec<UsageRow>) -> Vec<UsageRow> {
        match self {
            UsageReadFault::None => rows,
            UsageReadFault::DropRow => rows.into_iter().skip(1).collect(),
            UsageReadFault::DoubleRow => {
                let first = rows.first().cloned();
                rows.extend(first);
                rows
            }
            UsageReadFault::SkewTotal => {
                rows.iter_mut()
                    .take(1)
                    .for_each(|row| row.cpu_millis = row.cpu_millis.saturating_add(1));
                rows
            }
        }
    }
}

/// What an allocation that books `booked` shows in a usage row: its CPU and memory, none of
/// either for a whole host.
#[must_use]
pub fn booked_totals(booked: Booked) -> (u64, u64) {
    match booked {
        Booked::Shape {
            cpu_millis,
            memory_bytes,
            ..
        } => (cpu_millis, memory_bytes),
        Booked::WholeHost => (0, 0),
    }
}

impl Observations {
    /// These observations with the usage history read as `rows`, in place of any read
    /// before.
    #[must_use]
    pub fn with_usage(mut self, rows: Vec<UsageRow>) -> Self {
        self.usage = Some(rows);
        self
    }

    /// These observations with `row` read in the usage history, after the rows before.
    #[must_use]
    pub fn with_usage_row(mut self, row: UsageRow) -> Self {
        self.usage.get_or_insert_with(Vec::new).push(row);
        self
    }
}

/// Every row of the usage history of `server`, read at `now`, [`PAGE_SIZE`] rows at a
/// time; `None` when the server does not answer the read 200 (it is down).
pub(crate) fn read(server: &mut ServerNode, now: FarmTime) -> Option<Vec<UsageRow>> {
    let mut rows = Vec::new();
    let mut token = String::new();
    loop {
        let (status, page) = page(server, now, &token);
        if status != 200 {
            return None;
        }
        rows.extend(
            page["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(row),
        );
        token = text(&page, "nextPageToken").to_owned();
        if token.is_empty() {
            return Some(rows);
        }
    }
}

/// One page of the history after `token` (the first page for an empty one): the status and
/// the body as JSON (`Null` when it is not JSON).
fn page(server: &mut ServerNode, now: FarmTime, token: &str) -> (u16, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/usage?pageSize={PAGE_SIZE}&pageToken={token}"))
        .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
        .body(Body::empty())
        // A token the server wrote is a path the request can carry; were it not, the empty
        // request is refused and the read is the server not answering.
        .unwrap_or_default();
    let answer = server.client_call(now, request);
    let status = answer.status().as_u16();
    (
        status,
        serde_json::from_str(answer.body()).unwrap_or_default(),
    )
}

/// The row one record of a page tells; `None` for a record that names no job or no host a
/// cell makes.
fn row(record: &Value) -> Option<UsageRow> {
    let job = JobId::parse(text(record, "jobId")).ok();
    let host = HostId::new(text(record, "hostId")).ok();
    let booked = &record["booked"];
    job.zip(host).map(|(job, host)| UsageRow {
        job,
        host,
        ended_at: virtual_time(number(record, "endedAtMs")),
        cpu_millis: number(booked, "cpuMillis"),
        memory_bytes: number(booked, "memoryBytes"),
    })
}

/// The text of `key` in `object`, empty when it has none.
fn text<'a>(object: &'a Value, key: &str) -> &'a str {
    object[key].as_str().unwrap_or_default()
}

/// The number of `key` in `object` (the JSON form of a 64-bit number is a string), zero
/// when it has none.
fn number(object: &Value, key: &str) -> u64 {
    let parsed = text(object, key).parse().ok();
    parsed.or(object[key].as_u64()).unwrap_or_default()
}
