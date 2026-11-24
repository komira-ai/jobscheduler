//! The page tokens of `ListJobs` for the clients of the job service over gRPC, in one
//! process: that a token a client is answered never names another client's job. No socket is
//! bound and nothing sleeps.

use std::sync::Arc;
use std::time::Duration;

use js_jobs::Retention;
use js_proto::v1::{ListJobsRequest, ListJobsResponse};
use js_state::ManualClock;

use super::JobApi;
use super::access_tests::{CI, OPS, clients, recorded};
use super::writes_rig::Writes;
use crate::agent_api::job_rig::{OneClock, START_MS, tables_on};
use crate::clock::SharedClock;

/// The service of [`served`] with a listing that reads windows of two rows and at most two
/// rows a call, so a page is cut at the bound after one window.
async fn served_in_small_windows() -> Writes {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_millis(START_MS));
    let (_, jobs) = tables_on(Arc::clone(&clock), Retention::DEFAULT);
    let shared = SharedClock::new(OneClock(clock));
    let two = std::num::NonZeroUsize::new(2).expect("two");
    let read = super::read::Ctx {
        scan_window: two,
        scan_bound: two,
        ..super::read::Ctx::default()
    };
    let api = JobApi::new(Arc::new(jobs), shared).with_read(read);
    Writes::over(api, clients()).await
}

/// Every page of `ListJobs` for `token`, following the tokens to the end: each page's ids
/// and the token it answered.
async fn pages(rig: &Writes, token: &str) -> Vec<(Vec<String>, String)> {
    let mut pages = Vec::new();
    let mut at = String::new();
    loop {
        let request = ListJobsRequest {
            page_token: at.clone(),
            page_size: 10,
            ..ListJobsRequest::default()
        };
        let page: ListJobsResponse = rig
            .grpc_as(token, "ListJobs", request)
            .await
            .expect("a listing");
        let ids = page.jobs.into_iter().map(|job| job.id).collect();
        at = page.next_page_token.clone();
        pages.push((ids, page.next_page_token));
        if at.is_empty() {
            return pages;
        }
    }
}

/// Asserts, over a table where another client's jobs sit between and after a client's own,
/// and pages cut at a bound of two rows, that every page token a client is answered names
/// one of its own jobs (never the last row read, another client's), that a page cut at the
/// bound with none of its jobs reads on to its next job, and that following the tokens lists
/// exactly its jobs; while an operator's page cut at the bound still carries the last row
/// read. Catches a client told another client's job id through a page token, and a client's
/// listing that ends early or repeats a job.
#[tokio::test]
async fn a_clients_page_token_never_names_another_clients_job() {
    let rig = served_in_small_windows().await;
    for (id, client) in [
        ("a-ci", "ci"),
        ("b-kbf", "kbf"),
        ("c-kbf", "kbf"),
        ("d-kbf", "kbf"),
        ("e-ci", "ci"),
    ] {
        recorded(&rig, id, Some(client));
    }
    let by_ci = pages(&rig, CI).await;
    let tokens: Vec<&str> = by_ci.iter().map(|(_, token)| token.as_str()).collect();
    assert_eq!(tokens, ["job/a-ci", ""], "{by_ci:?}");
    let ids: Vec<String> = by_ci.into_iter().flat_map(|(ids, _)| ids).collect();
    assert_eq!(ids, ["a-ci", "e-ci"]);
    let by_ops = pages(&rig, OPS).await;
    assert_eq!(
        by_ops[0],
        (
            vec!["a-ci".to_owned(), "b-kbf".to_owned()],
            "job/b-kbf".to_owned()
        )
    );
}
