//! `ListJobs` (`read.rs`) over both transports: its filters, its order, its pages and the
//! bound on the rows one call reads. The rig is `read_tests.rs`'s.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use js_jobs::{JobEnd, JobId, Output, StartFailure};
use js_proto::v1::{self, ListJobsRequest, ListJobsResponse};
use tonic::Code;

use super::read;
use super::read_tests::{Rig, START_MS, clock, over_fake, place, queue, run, running, spec, start};
use crate::job_store::ServerJobs;

/// The ids of a page's jobs.
fn ids(page: &ListJobsResponse) -> Vec<&str> {
    page.jobs.iter().map(|job| job.id.as_str()).collect()
}

/// A listing request with the states `states` and the labels `labels`.
fn filtered(states: &[v1::JobState], labels: &[(&str, &str)]) -> ListJobsRequest {
    ListJobsRequest {
        states: states.iter().map(|state| i32::from(*state)).collect(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        ..ListJobsRequest::default()
    }
}

/// The ids of every job `request` lists, page by page, with the ids of each page.
async fn walk(rig: &Rig, request: ListJobsRequest) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut token = String::new();
    loop {
        let page = ListJobsRequest {
            page_token: token,
            ..request.clone()
        };
        let page = rig.list(page).await.expect("a page");
        pages.push(ids(&page).into_iter().map(str::to_owned).collect());
        if page.next_page_token.is_empty() {
            return pages;
        }
        token = page.next_page_token;
    }
}

/// Places, starts and runs the queued job `id`.
fn to_running(jobs: &ServerJobs, id: &JobId) {
    place(jobs, id);
    start(jobs, id);
    run(jobs, id);
}

/// Ends the running job `id` as `end`.
fn end(jobs: &ServerJobs, id: &JobId, end: JobEnd) {
    jobs.end_job(id, end).expect("the job ends");
}

/// Exited with `code`, no output.
fn exited(code: i32) -> JobEnd {
    JobEnd::Exited {
        exit_code: code,
        output: Output::default(),
    }
}

/// Records eight jobs, one a second from [`START_MS`], whose ids do not sort in the order
/// they were made: (id, made at second, state, labels)
///
/// | id | s | state | labels |
/// |---|---|---|---|
/// | j-5 | 0 | queued | team=ml |
/// | j-2 | 1 | running | team=ml, tier=gold |
/// | j-8 | 2 | succeeded | team=web |
/// | j-1 | 3 | failed | team=ml, tier=gold |
/// | j-7 | 4 | cancelled | |
/// | j-3 | 5 | placed | team=web |
/// | j-6 | 6 | failed to start | team=ml |
/// | j-4 | 7 | lost | tier=gold |
fn eight(rig: &Rig) {
    let jobs = &rig.jobs;
    let tick = || rig.advance(Duration::from_secs(1));
    queue(jobs, "j-5", spec(&[("team", "ml")]));
    tick();
    let j2 = queue(jobs, "j-2", spec(&[("team", "ml"), ("tier", "gold")]));
    to_running(jobs, &j2);
    tick();
    let j8 = queue(jobs, "j-8", spec(&[("team", "web")]));
    to_running(jobs, &j8);
    end(jobs, &j8, exited(0));
    tick();
    let j1 = queue(jobs, "j-1", spec(&[("team", "ml"), ("tier", "gold")]));
    to_running(jobs, &j1);
    end(jobs, &j1, exited(1));
    tick();
    let j7 = queue(jobs, "j-7", spec(&[]));
    jobs.request_cancel(&j7)
        .expect("the queued job is cancelled");
    tick();
    let j3 = queue(jobs, "j-3", spec(&[("team", "web")]));
    place(jobs, &j3);
    tick();
    let j6 = queue(jobs, "j-6", spec(&[("team", "ml")]));
    end(jobs, &j6, JobEnd::FailedToStart(StartFailure::NoCapacity));
    tick();
    let j4 = queue(jobs, "j-4", spec(&[("tier", "gold")]));
    to_running(jobs, &j4);
    end(jobs, &j4, JobEnd::Lost);
}

/// The milliseconds of second `s` after [`START_MS`].
fn at(s: u64) -> u64 {
    START_MS + s * 1000
}

/// Asserts, over eight jobs across states, labels and times ([`eight`]), that `ListJobs`
/// lists in id order (not the order the jobs were made); keeps exactly the jobs of each state
/// alone (every state of the enum, and none for unspecified or a value this server does not
/// know); keeps the jobs with every label asked for; keeps the half-open time window (a job
/// made exactly at `queuedAfterMs` is in, one made exactly at `queuedBeforeMs` is out);
/// combines a state with a label and two states with a time; walks every match exactly once
/// with a page size of 3, every page but the last full and carrying a token; and refuses a
/// page token no listing gave as an argument naming `pageToken`. Catches a missed boundary,
/// a dropped or extra filter, a duplicate or a gap across pages, and a short page with a
/// token while matches remain.
#[tokio::test]
async fn list_filters_by_state_label_and_time() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    eight(&rig);
    let all = rig.list(ListJobsRequest::default()).await.expect("a list");
    let every = ["j-1", "j-2", "j-3", "j-4", "j-5", "j-6", "j-7", "j-8"];
    assert_eq!(
        (ids(&all), all.next_page_token.as_str()),
        (every.to_vec(), "")
    );

    let by_state = [
        (v1::JobState::Queued, vec!["j-5"]),
        (v1::JobState::Placed, vec!["j-3"]),
        (v1::JobState::Starting, vec![]),
        (v1::JobState::Running, vec!["j-2"]),
        (v1::JobState::Succeeded, vec!["j-8"]),
        (v1::JobState::Failed, vec!["j-1"]),
        (v1::JobState::Cancelled, vec!["j-7"]),
        (v1::JobState::Lost, vec!["j-4"]),
        (v1::JobState::FailedToStart, vec!["j-6"]),
        (v1::JobState::Unspecified, vec![]),
    ];
    for (state, expected) in by_state {
        let page = rig.list(filtered(&[state], &[])).await.expect("a list");
        assert_eq!(ids(&page), expected, "{state:?}");
        assert_eq!(page.next_page_token, "", "{state:?}");
    }
    let newer = ListJobsRequest {
        states: vec![42],
        ..ListJobsRequest::default()
    };
    let page: ListJobsResponse = rig.grpc("ListJobs", newer).await.expect("a list");
    assert!(page.jobs.is_empty());
    let two = filtered(&[v1::JobState::Queued, v1::JobState::Lost], &[]);
    assert_eq!(ids(&rig.list(two).await.expect("a list")), ["j-4", "j-5"]);

    let ml = rig
        .list(filtered(&[], &[("team", "ml")]))
        .await
        .expect("a list");
    assert_eq!(ids(&ml), ["j-1", "j-2", "j-5", "j-6"]);
    let gold_ml = filtered(&[], &[("team", "ml"), ("tier", "gold")]);
    assert_eq!(
        ids(&rig.list(gold_ml).await.expect("a list")),
        ["j-1", "j-2"]
    );
    let absent = filtered(&[], &[("team", "none")]);
    assert!(rig.list(absent).await.expect("a list").jobs.is_empty());

    let window = ListJobsRequest {
        queued_after_ms: Some(at(3)),
        queued_before_ms: Some(at(6)),
        ..ListJobsRequest::default()
    };
    assert_eq!(
        ids(&rig.list(window).await.expect("a list")),
        ["j-1", "j-3", "j-7"]
    );
    let after = ListJobsRequest {
        queued_after_ms: Some(at(6)),
        ..ListJobsRequest::default()
    };
    assert_eq!(ids(&rig.list(after).await.expect("a list")), ["j-4", "j-6"]);
    let before = ListJobsRequest {
        queued_before_ms: Some(at(1)),
        ..ListJobsRequest::default()
    };
    assert_eq!(ids(&rig.list(before).await.expect("a list")), ["j-5"]);
    let between = ListJobsRequest {
        queued_after_ms: Some(at(1) + 1),
        queued_before_ms: Some(at(2)),
        ..ListJobsRequest::default()
    };
    assert!(rig.list(between).await.expect("a list").jobs.is_empty());

    let failed_ml = filtered(&[v1::JobState::Failed], &[("team", "ml")]);
    assert_eq!(ids(&rig.list(failed_ml).await.expect("a list")), ["j-1"]);
    let late = ListJobsRequest {
        queued_after_ms: Some(at(2)),
        ..filtered(&[v1::JobState::Running, v1::JobState::Failed], &[])
    };
    assert_eq!(ids(&rig.list(late).await.expect("a list")), ["j-1"]);

    let by_three = ListJobsRequest {
        page_size: 3,
        ..ListJobsRequest::default()
    };
    let pages = walk(&rig, by_three).await;
    assert_eq!(
        pages,
        [
            vec!["j-1", "j-2", "j-3"],
            vec!["j-4", "j-5", "j-6"],
            vec!["j-7", "j-8"]
        ]
    );
    let ml_by_three = ListJobsRequest {
        page_size: 3,
        ..filtered(&[], &[("team", "ml")])
    };
    let pages = walk(&rig, ml_by_three).await;
    assert_eq!(pages, [vec!["j-1", "j-2", "j-5"], vec!["j-6"]]);

    for token in ["nope", "job/", "job/a*b", "alloc/j-1"] {
        let request = ListJobsRequest {
            page_token: token.to_owned(),
            ..ListJobsRequest::default()
        };
        let refused = rig.list(request).await.expect_err("refused");
        assert_eq!(
            (refused.code(), refused.message()),
            (Code::InvalidArgument, "field: pageToken"),
            "{token}"
        );
    }
}

/// Asserts that a page size of 0 is answered [`read::DEFAULT_PAGE_SIZE`] (100) jobs and one
/// above [`read::MAX_PAGE_SIZE`] (5000) is cut to 1000, not refused, each with a token that
/// reads the rest: over 1001 queued jobs, the cut page and its next page list every job once.
/// Catches a default or a cap that is not applied, and a large page size refused.
#[tokio::test]
async fn a_page_size_is_defaulted_and_cut() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let made: Vec<String> = (0..1001).map(|n| format!("bulk-{n:04}")).collect();
    for text in &made {
        queue(&rig.jobs, text, spec(&[]));
    }
    let default = rig.list(ListJobsRequest::default()).await.expect("a page");
    assert_eq!(default.jobs.len(), 100);
    assert_eq!(default.next_page_token, "job/bulk-0099");
    let large = ListJobsRequest {
        page_size: 5000,
        ..ListJobsRequest::default()
    };
    let pages = walk(&rig, large).await;
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [1000, 1]);
    assert_eq!(pages.concat(), made);
}

/// Asserts that with reads of two rows and a bound of ten rows a call, a filter whose
/// matches lie further apart than a read still fills each page within the bound, and that
/// the page cut at the bound is short and carries a token. Over thirty jobs `s-00` to `s-29`
/// with matches at 0, 5, 10, 15, 20 and 29, pages of two read: `s-00, s-05` (six rows, a
/// token), `s-10, s-15` (ten rows, a token), `s-20` alone (the bound, a token), `s-29`
/// (the end, no token). Catches a short page returned while matches within the bound
/// remain, a bound not applied, and a token that skips or repeats a row.
#[tokio::test]
async fn a_sparse_filter_still_fills_the_page_within_the_scan_bound() {
    let clock = clock();
    let ctx = read::Ctx {
        scan_window: NonZeroUsize::new(2).expect("2"),
        scan_bound: NonZeroUsize::new(10).expect("10"),
        ..read::Ctx::default()
    };
    let rig = Rig::with(over_fake(&clock), clock, ctx).await;
    for n in 0..30 {
        let labels: &[(&str, &str)] = match [0, 5, 10, 15, 20, 29].contains(&n) {
            true => &[("hit", "yes")],
            false => &[("hit", "no")],
        };
        queue(&rig.jobs, &format!("s-{n:02}"), spec(labels));
    }
    let request = ListJobsRequest {
        page_size: 2,
        ..filtered(&[], &[("hit", "yes")])
    };
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut pages = Vec::new();
    loop {
        let page = ListJobsRequest {
            page_token: token,
            ..request.clone()
        };
        let page = rig.list(page).await.expect("a page");
        pages.push(
            ids(&page)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        );
        tokens.push(page.next_page_token.clone());
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(
        pages,
        [
            vec!["s-00", "s-05"],
            vec!["s-10", "s-15"],
            vec!["s-20"],
            vec!["s-29"]
        ]
    );
    assert_eq!(tokens, ["job/s-05", "job/s-15", "job/s-25", ""]);
    assert_eq!(read::Ctx::default().scan_window, read::SCAN_WINDOW);
    assert_eq!(read::Ctx::default().scan_bound, read::SCAN_BOUND);
    assert_eq!(
        (read::SCAN_WINDOW.get(), read::SCAN_BOUND.get()),
        (1000, 10_000)
    );
}

/// Asserts that a listing that fills its page in the middle of a read answers a token after
/// the page's last job, so the next page starts at the next match in that read: over five
/// matching jobs read in one window, pages of two give `a-1, a-2`, `a-3, a-4`, `a-5`.
/// Catches a token after the whole read, which would skip the matches left in it.
#[tokio::test]
async fn a_page_full_inside_a_read_resumes_after_its_last_job() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    for text in ["a-1", "a-2", "a-3", "a-4", "a-5"] {
        running(&rig.jobs, text);
    }
    let request = ListJobsRequest {
        page_size: 2,
        ..filtered(&[v1::JobState::Running], &[])
    };
    let pages = walk(&rig, request).await;
    assert_eq!(pages, [vec!["a-1", "a-2"], vec!["a-3", "a-4"], vec!["a-5"]]);
    let first = ListJobsRequest {
        page_size: 2,
        ..ListJobsRequest::default()
    };
    let page = rig.list(first).await.expect("a page");
    assert_eq!(page.next_page_token, "job/a-2");
}
