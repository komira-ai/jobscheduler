//! The read routes of the usage data, behind the client token:
//!
//! - `GET /v1/usage/nodes/{id}?from=<minute>&to=<minute>`: the rows of one node.
//! - `GET /v1/usage/keys/{key}`: what is known of one key of work.
//!
//! The id and the key are read from the raw path, not from the decoded one, and only in
//! their canonical spelling: an escape (`%2e`), a slash, a capital, a dot-only segment or
//! a segment over the length cap is a 400 that says what kind of fault it was and
//! nothing of the request. Nothing a client sent reaches a store key, a log line or an
//! answer.
//!
//! The routes exist when agents register hosts and `--usage-off` is not given; without
//! them the paths are unknown routes (404).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use js_types::HostId;
use serde::Serialize;
use tracing::error;

use crate::api::{Shared, authenticate};
use crate::usage::{KeyView, NodeRow, ProfileKey, UsageFault};
use crate::usage_hub::Usage;
use crate::util::blocking;
use crate::wire::ApiError;

/// The path before a node id.
const NODES: &str = "/v1/usage/nodes/";

/// The path before a key.
const KEYS: &str = "/v1/usage/keys/";

/// The most rows one answer holds; the answer's `next` says where the rest begins.
pub(crate) const PAGE_ROWS: usize = 1440;

/// The minutes an answer covers when the request names no range: the last hour.
const DEFAULT_MINUTES: u64 = 60;

/// What the handlers share.
#[derive(Clone)]
struct UsageApi {
    usage: Arc<Usage>,
}

/// The usage routes, behind the client token.
pub(crate) fn router(shared: Arc<Shared>, usage: Arc<Usage>) -> Router {
    Router::new()
        .route("/v1/usage/nodes/{*id}", get(nodes))
        .route("/v1/usage/keys/{*key}", get(key))
        .layer(from_fn_with_state(shared, authenticate))
        .with_state(UsageApi { usage })
}

/// The part of the raw path after `prefix`. The router matched the prefix, so it is there.
fn tail<'a>(uri: &'a Uri, prefix: &str) -> &'a str {
    uri.path().get(prefix.len()..).unwrap_or_default()
}

/// The host a path segment names, if it is exactly an id and not a segment made of dots.
fn host_id(text: &str) -> Option<HostId> {
    let dots = text.chars().all(|c| c == '.');
    HostId::new(text).ok().filter(|_| !dots)
}

/// A decimal number of minutes: digits only, and no more than a `u64` holds.
fn number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// The `from` and `to` of a query: each at most once, nothing else.
fn parse_query(query: Option<&str>) -> Result<(Option<u64>, Option<u64>), ApiError> {
    let bad = || ApiError::bad_request("the query takes from=<minute> and to=<minute>, once each");
    let (mut from, mut to) = (None, None);
    let pairs = query.unwrap_or_default().split('&');
    for pair in pairs.filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').ok_or_else(bad)?;
        let slot = match name {
            "from" => &mut from,
            "to" => &mut to,
            _ => return Err(bad()),
        };
        let minute = number(value).ok_or_else(bad)?;
        if slot.replace(minute).is_some() {
            return Err(bad());
        }
    }
    Ok((from, to))
}

/// The minutes to answer for: `to` defaults to `now`, `from` to the hour before it.
fn window(range: (Option<u64>, Option<u64>), now: u64) -> Result<(u64, u64), ApiError> {
    let to = range.1.unwrap_or(now);
    let from = range.0.unwrap_or(to.saturating_sub(DEFAULT_MINUTES - 1));
    match to < from {
        true => Err(ApiError::bad_request("to is before from")),
        false => Ok((from, to)),
    }
}

/// The 503 of a store that failed. The cause is in the log, and holds nothing the
/// client sent.
fn unavailable(fault: UsageFault) -> ApiError {
    error!(error = %fault, "cannot read the usage store");
    ApiError::coded(StatusCode::SERVICE_UNAVAILABLE, "usage-unavailable")
}

/// One minute of a node in an answer.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct RowView {
    minute: u64,
    cpu_millis: u32,
    ws_bytes: u64,
    leases: u32,
}

impl From<NodeRow> for RowView {
    fn from(row: NodeRow) -> Self {
        Self {
            minute: row.minute,
            cpu_millis: row.cpu_millis,
            ws_bytes: row.ws_max_bytes,
            leases: row.leases,
        }
    }
}

/// The body of `GET /v1/usage/nodes/{id}`.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct NodeRows {
    host: String,
    from: u64,
    to: u64,
    rows: Vec<RowView>,
    next: Option<u64>,
}

/// The answer for `rows`, which hold one row more than a page when there is more: that
/// row is not sent, and its minute is where the next request begins.
fn page(host: &HostId, (from, to): (u64, u64), mut rows: Vec<NodeRow>) -> NodeRows {
    let next = rows.get(PAGE_ROWS).map(|row| row.minute);
    rows.truncate(PAGE_ROWS);
    NodeRows {
        host: host.to_string(),
        from,
        to,
        rows: rows.into_iter().map(RowView::from).collect(),
        next,
    }
}

async fn nodes(State(api): State<UsageApi>, uri: Uri) -> Result<Response, ApiError> {
    let host =
        host_id(tail(&uri, NODES)).ok_or_else(|| ApiError::bad_request("not a valid host id"))?;
    let range = window(parse_query(uri.query())?, api.usage.now_minute())?;
    let (usage, asked) = (api.usage.clone(), host.clone());
    let rows = blocking(move || usage.node_rows(&asked, range.0, range.1, PAGE_ROWS + 1)).await;
    let rows = rows.map_err(unavailable)?;
    Ok((StatusCode::OK, Json(page(&host, range, rows))).into_response())
}

/// One point of a curve in an answer.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct PointView {
    minute: u16,
    cores_milli: u32,
    ws_bytes: u64,
}

/// The body of `GET /v1/usage/keys/{key}`.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct KeyBody {
    key: String,
    runs: u16,
    curve: Option<Vec<PointView>>,
    last_used_day: u64,
    stale: bool,
}

impl KeyBody {
    fn of(key: &ProfileKey, view: KeyView) -> Self {
        let point = |p: crate::usage::CurvePoint| PointView {
            minute: p.minute,
            cores_milli: p.cores_milli,
            ws_bytes: p.ws_bytes,
        };
        Self {
            key: key.to_string(),
            runs: view.runs,
            curve: view.curve.map(|c| c.into_iter().map(point).collect()),
            last_used_day: view.last_used_day,
            stale: view.stale,
        }
    }
}

async fn key(State(api): State<UsageApi>, uri: Uri) -> Result<Response, ApiError> {
    let key = ProfileKey::parse(tail(&uri, KEYS))
        .map_err(|_| ApiError::bad_request("not a valid key"))?;
    let (usage, asked) = (api.usage.clone(), key.clone());
    let view = blocking(move || usage.key_profile(&asked)).await;
    match view.map_err(unavailable)? {
        Some(view) => Ok((StatusCode::OK, Json(KeyBody::of(&key, view))).into_response()),
        None => Err(ApiError::coded(StatusCode::NOT_FOUND, "unknown-key")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::CurvePoint;

    /// The error as text: its status, its code and its reason.
    fn shown(err: ApiError) -> String {
        format!("{err:?}")
    }

    /// Asserts that a host id is taken only in its canonical spelling and that a segment
    /// made of dots is not an id although the alphabet has dots. It catches `.` and `..`
    /// reaching a store key, and a capital or an escape naming a host twice.
    #[test]
    fn a_host_segment_is_an_id_and_not_a_path_trick() {
        let longest = "a".repeat(63);
        let over = "a".repeat(64);
        for good in ["h1", "burst-1", "a.b", ".a", "a..", longest.as_str()] {
            assert!(host_id(good).is_some(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "...",
            "H1",
            "a/b",
            "a%2eb",
            "%2e",
            "a b",
            over.as_str(),
        ] {
            assert!(host_id(bad).is_none(), "{bad:?}");
        }
    }

    /// Asserts how minutes are read: digits only, no sign, no space, nothing empty, and a
    /// value over `u64` is refused and not wrapped. It catches `+5` and ` 5` being
    /// accepted by the standard parser, and an overflow read as a small number.
    #[test]
    fn a_minute_is_plain_digits_that_fit() {
        assert_eq!(number("0"), Some(0));
        assert_eq!(number("0007"), Some(7));
        assert_eq!(number("18446744073709551615"), Some(u64::MAX));
        for bad in [
            "",
            "+5",
            "-5",
            " 5",
            "5 ",
            "5a",
            "1.5",
            "18446744073709551616",
        ] {
            assert_eq!(number(bad), None, "{bad:?}");
        }
    }

    /// Asserts the query grammar: `from` and `to`, each at most once, in either order,
    /// empty pairs ignored, and anything else (another name, a pair without `=`, a bad
    /// number, a repeat) a 400 whose reason names the grammar and never the text. It
    /// catches a query parser that accepts a repeat (the last wins, silently) or ignores
    /// a name it does not know, and an error text that echoes the value.
    #[test]
    fn the_query_takes_from_and_to_once_each_and_never_echoes() {
        assert_eq!(parse_query(None).unwrap(), (None, None));
        assert_eq!(parse_query(Some("")).unwrap(), (None, None));
        assert_eq!(parse_query(Some("from=5")).unwrap(), (Some(5), None));
        assert_eq!(
            parse_query(Some("to=9&from=5&")).unwrap(),
            (Some(5), Some(9))
        );
        for bad in [
            "from",
            "from=",
            "from=x",
            "from=1&from=2",
            "to=1&to=2",
            "limit=5",
            "=5",
            "from=-1",
            "from=SECRET-TEXT",
            "SECRET-TEXT=1",
        ] {
            let text = shown(parse_query(Some(bad)).unwrap_err());
            assert!(text.contains("400"), "{text}");
            assert!(text.contains("bad-request"), "{text}");
            assert!(!text.contains("SECRET"), "{text}");
        }
    }

    /// Asserts the default window and its refusal: with no range the answer covers the 60
    /// minutes up to now, a named `to` carries its hour with it, a `from` alone runs to
    /// now, a range may be one minute, and a range that ends before it begins is a 400.
    /// It catches a window of 59 or 61 minutes and one that underflows near the epoch.
    #[test]
    fn the_default_window_is_the_last_sixty_minutes() {
        assert_eq!(window((None, None), 1000).unwrap(), (941, 1000));
        assert_eq!(window((None, Some(500)), 1000).unwrap(), (441, 500));
        assert_eq!(window((Some(990), None), 1000).unwrap(), (990, 1000));
        assert_eq!(window((Some(7), Some(7)), 1000).unwrap(), (7, 7));
        assert_eq!(window((None, None), 10).unwrap(), (0, 10));
        assert!(window((Some(8), Some(7)), 1000).is_err());
    }

    fn rows(n: usize) -> Vec<NodeRow> {
        (0..n)
            .map(|i| NodeRow {
                minute: 100 + i as u64,
                cpu_millis: 1,
                ws_max_bytes: 2,
                leases: 3,
            })
            .collect()
    }

    /// Asserts paging: a page of rows holds at most 1440, a full page with no row after it
    /// has no `next`, and when the sink returned a row past the page that row's minute is
    /// `next` and the row is not in the page. It catches an off-by-one that drops or
    /// repeats the row at the page boundary.
    #[test]
    fn a_page_holds_1440_rows_and_next_is_the_first_row_left_out() {
        let host = HostId::new("h1").unwrap();
        let few = page(&host, (0, 9), rows(3));
        assert_eq!((few.rows.len(), few.next), (3, None));
        assert_eq!(
            few.rows[0],
            RowView {
                minute: 100,
                cpu_millis: 1,
                ws_bytes: 2,
                leases: 3
            }
        );
        let full = page(&host, (0, 9), rows(PAGE_ROWS));
        assert_eq!((full.rows.len(), full.next), (PAGE_ROWS, None));
        let more = page(&host, (0, 9), rows(PAGE_ROWS + 1));
        assert_eq!(more.rows.len(), PAGE_ROWS);
        assert_eq!(more.next, Some(100 + PAGE_ROWS as u64));
        assert_eq!((more.host.as_str(), more.from, more.to), ("h1", 0, 9));
    }

    /// Asserts the body of a key: the curve is `null` until the sink has one, and its
    /// points carry the minute, the cores and the bytes under the documented names. It
    /// catches a renamed field and a curve that is an empty list when it is absent.
    #[test]
    fn a_key_body_has_a_null_curve_until_there_is_one() {
        let key = ProfileKey::parse("act.build").unwrap();
        let none = KeyView {
            runs: 2,
            curve: None,
            last_used_day: 9,
            stale: false,
        };
        let body = serde_json::to_value(KeyBody::of(&key, none)).unwrap();
        let wanted = serde_json::json!({
            "key": "act.build", "runs": 2, "curve": null, "last_used_day": 9, "stale": false
        });
        assert_eq!(body, wanted);
        let curve = vec![CurvePoint {
            minute: 0,
            cores_milli: 1500,
            ws_bytes: 4096,
        }];
        let some = KeyView {
            runs: 3,
            curve: Some(curve),
            last_used_day: 9,
            stale: true,
        };
        let body = serde_json::to_value(KeyBody::of(&key, some)).unwrap();
        let point = serde_json::json!({ "minute": 0, "cores_milli": 1500, "ws_bytes": 4096 });
        assert_eq!(body["curve"], serde_json::json!([point]));
        assert_eq!(body["stale"], true);
    }

    /// Asserts that the part of a path after the prefix is the raw text, with escapes left
    /// as they came. It catches a decoded path, which would turn `%2e` into a dot.
    #[test]
    fn the_tail_of_a_path_is_raw() {
        let uri: Uri = "/v1/usage/nodes/a%2e?from=1".parse().unwrap();
        assert_eq!(tail(&uri, NODES), "a%2e");
    }
}
