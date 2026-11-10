//! `GetUsage` over the server's own usage sink and job store, a page at a time, and how each
//! ended booking is told.

use js_usage::{EndedHistory, EndedKey};

use super::*;
use crate::job_api::fleet::usage::{Cursor, Source as From};

/// The answer of `GetUsage` to `request`.
async fn usage_of(api: &JobApi, request: GetUsageRequest) -> Result<GetUsageResponse, Status> {
    let answer = get_usage(api, Request::new(request)).await;
    answer.map(Response::into_inner)
}

fn job_ids(page: &GetUsageResponse) -> Vec<&str> {
    page.records
        .iter()
        .map(|record| record.job_id.as_str())
        .collect()
}

/// Asserts the record of each of three bookings that ended: one given back on `h1` with what
/// it used and its estimate, one cancelled while it waited for a host (no host, never
/// placed), one stopped at its wall limit on a host drained for it; in end order, each field
/// as the row has it; and that the window is from `from_ms` included to `to_ms` excluded. It
/// catches records keyed by anything but their end, the unplaced booking dropped, and a
/// window end included.
#[tokio::test]
async fn usage_lists_ended_allocations() {
    let s = server("usage-records");
    let rows = [
        EndedRow {
            host: Some(HostId::new("h1").unwrap()),
            booked: Some(UsageBooked {
                whole_host: false,
                cpu_milli: 1500,
                memory_bytes: 1 << 30,
            }),
            queued_ms: Some(40),
            run_ms: Some(1000),
            estimated_end: Some(1_000_200),
            used: Some(Used {
                cpu_milli: 900,
                memory_bytes: 512 << 20,
                cpu_mean_milli: 600,
            }),
            ..ended("job-ok", 1_000_000)
        },
        EndedRow {
            queued_ms: Some(5_000),
            run_ms: Some(0),
            ended_reason: EndedReason::CancelledWhileWaiting,
            drain_outcome: DrainOutcome::NoMark,
            give_up_outcome: Some(1),
            ..ended("job-cancel", 2_000_000)
        },
        EndedRow {
            host: Some(HostId::new("h2").unwrap()),
            booked: Some(UsageBooked {
                whole_host: true,
                cpu_milli: 8000,
                memory_bytes: 16 << 30,
            }),
            queued_ms: Some(0),
            run_ms: Some(600_000),
            ended_reason: EndedReason::StoppedAtWallLimit,
            drain_outcome: DrainOutcome::MarkHeld,
            give_up_outcome: Some(2),
            estimated_end: Some(2_900_000),
            ..ended("job-wall", 3_000_000)
        },
    ];
    for row in &rows {
        s.sink.allocation_ended(row).unwrap();
    }
    let all = usage_of(&s.api, GetUsageRequest::default()).await.unwrap();
    let expected = [
        wire::UsageRecord {
            job_id: "job-ok".to_owned(),
            host_id: "h1".to_owned(),
            queue_wait_ms: 40,
            run_ms: 1000,
            ended_at_ms: 1_000_000,
            booked: Some(booking("job-ok", 1500, 1 << 30, 0, 0, &[])),
            used: Some(wire::ResourceUsage {
                cpu_usec: 600_000,
                peak_memory_bytes: 512 << 20,
                ..wire::ResourceUsage::default()
            }),
            estimated_run_ms: Some(1_200),
            ..wire::UsageRecord::default()
        },
        wire::UsageRecord {
            job_id: "job-cancel".to_owned(),
            queue_wait_ms: 5_000,
            end_state: i32::from(wire::JobState::Cancelled),
            ended_reason: i32::from(wire::ResultReason::Cancelled),
            ended_at_ms: 2_000_000,
            mark_give_ups: 1,
            ..wire::UsageRecord::default()
        },
        wire::UsageRecord {
            job_id: "job-wall".to_owned(),
            host_id: "h2".to_owned(),
            run_ms: 600_000,
            end_state: i32::from(wire::JobState::Failed),
            ended_reason: i32::from(wire::ResultReason::WallLimit),
            ended_at_ms: 3_000_000,
            booked: Some(booking("job-wall", 8000, 16 << 30, 0, 0, &[])),
            whole_host: i32::from(wire::WholeHostOutcome::Drained),
            mark_give_ups: 2,
            estimated_run_ms: Some(500_000),
            ..wire::UsageRecord::default()
        },
    ];
    assert_eq!(all.records, expected);
    assert_eq!(all.next_page_token, "");
    let window = GetUsageRequest {
        from_ms: Some(2_000_000),
        to_ms: Some(3_000_000),
        ..GetUsageRequest::default()
    };
    let window = usage_of(&s.api, window).await.unwrap();
    assert_eq!(job_ids(&window), ["job-cancel"]);
    let after = GetUsageRequest {
        from_ms: Some(2_000_001),
        ..GetUsageRequest::default()
    };
    assert_eq!(
        job_ids(&usage_of(&s.api, after).await.unwrap()),
        ["job-wall"]
    );
}

/// Reads every page of `request`'s listing; the job ids of each page. `between` runs after
/// the first page.
async fn pages(api: &JobApi, request: GetUsageRequest, between: impl Fn()) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut request = request;
    loop {
        let page = usage_of(api, request.clone()).await.unwrap();
        pages.push(job_ids(&page).into_iter().map(str::to_owned).collect());
        if pages.len() == 1 {
            between();
        }
        match page.next_page_token.as_str() {
            "" => return pages,
            token => request.page_token = token.to_owned(),
        }
    }
}

/// Asserts that seven records read three to a page come as 3, 3 and 1, each page but the
/// last with a token, none repeated or skipped; that a record ending between two pages
/// behind the cursor is not read and one ahead of it is; that the host and job filters
/// apply before the page is cut (a host with two of the records fills a page of two, with
/// no next); that a token no answer gave is `INVALID_ARGUMENT`; and that a page size of 0
/// is 100 records. It catches an off-by-one cursor and a filter applied after the limit.
#[tokio::test]
async fn usage_pages_and_filters() {
    let s = server("usage-pages");
    for n in 1..=7 {
        let host = match n {
            2 | 5 => "h1",
            _ => "h2",
        };
        let row = EndedRow {
            host: Some(HostId::new(host).unwrap()),
            ..ended(&format!("u{n}"), n * 1000)
        };
        s.sink.allocation_ended(&row).unwrap();
    }
    let by_three = GetUsageRequest {
        page_size: 3,
        ..GetUsageRequest::default()
    };
    let read = pages(&s.api, by_three.clone(), || {}).await;
    assert_eq!(
        read,
        [vec!["u1", "u2", "u3"], vec!["u4", "u5", "u6"], vec!["u7"]]
    );
    let late = || {
        s.sink.allocation_ended(&ended("behind", 500)).unwrap();
        s.sink.allocation_ended(&ended("ahead", 7_500)).unwrap();
    };
    let read = pages(&s.api, by_three, late).await;
    assert_eq!(
        read,
        [
            vec!["u1", "u2", "u3"],
            vec!["u4", "u5", "u6"],
            vec!["u7", "ahead"]
        ]
    );
    let h1 = GetUsageRequest {
        host_id: "h1".to_owned(),
        page_size: 2,
        ..GetUsageRequest::default()
    };
    let h1 = usage_of(&s.api, h1).await.unwrap();
    assert_eq!(
        (job_ids(&h1), h1.next_page_token.as_str()),
        (vec!["u2", "u5"], "")
    );
    let one = GetUsageRequest {
        job_id: "u4".to_owned(),
        ..GetUsageRequest::default()
    };
    assert_eq!(job_ids(&usage_of(&s.api, one).await.unwrap()), ["u4"]);
    for token in [
        "garbage",
        "00000000000000001000/U1",
        "+0000000000000001000/u1",
    ] {
        let bad = GetUsageRequest {
            page_token: token.to_owned(),
            ..GetUsageRequest::default()
        };
        let answer = usage_of(&s.api, bad).await;
        assert_eq!(
            refused(answer),
            (Code::InvalidArgument, INVALID_PAGE_TOKEN.to_owned()),
            "{token}"
        );
    }
    for n in 0..100 {
        s.sink
            .allocation_ended(&ended(&format!("m{n}"), 10_000 + n))
            .unwrap();
    }
    let first = usage_of(&s.api, GetUsageRequest::default()).await.unwrap();
    assert_eq!(first.records.len(), 100);
    assert_ne!(first.next_page_token, "");
}

/// Asserts the page size a request gets: 0 is 100, a size up to 1000 is kept, and a larger
/// one (also the largest a request can carry) is cut to 1000.
#[test]
fn the_page_size_is_defaulted_and_cut() {
    let sizes = [0, 1, 7, 1000, 1001, 5000, u32::MAX].map(|asked| page_size(asked).get());
    assert_eq!(sizes, [100, 1, 7, 1000, 1000, 1000, 1000]);
}

/// A history that fails every read, or records that it was asked.
struct BrokenHistory(std::sync::Mutex<usize>);

#[tonic::async_trait]
impl History for BrokenHistory {
    async fn page(&self, _: EndedQuery) -> Result<EndedPage, UsageFault> {
        *self.0.lock().unwrap() += 1;
        Err(UsageFault("down".to_owned()))
    }
}

/// Asserts that a server holding neither history (`--usage-off`, and no job store's)
/// answers an empty page, not an error, whatever the filters, and still refuses a token no
/// answer gave; that a filter
/// no record can match (a host id or a job id no booking can have) answers an empty page
/// without reading the history; and that a history that cannot be read is `UNAVAILABLE`
/// `usage-unavailable`. It catches usage off answered as a failure, and a failed read
/// answered as an empty page.
#[tokio::test]
async fn usage_off_returns_an_empty_page() {
    let off = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(no_pools)),
        None,
    );
    let any = usage_of(&off, GetUsageRequest::default()).await.unwrap();
    assert_eq!(any, GetUsageResponse::default());
    let filtered = GetUsageRequest {
        host_id: "h1".to_owned(),
        page_size: 5,
        ..GetUsageRequest::default()
    };
    assert_eq!(
        usage_of(&off, filtered).await.unwrap(),
        GetUsageResponse::default()
    );
    let bad = GetUsageRequest {
        page_token: "x".to_owned(),
        ..GetUsageRequest::default()
    };
    assert_eq!(refused(usage_of(&off, bad).await).0, Code::InvalidArgument);

    let broken = Arc::new(BrokenHistory(std::sync::Mutex::new(0)));
    let history: Arc<dyn History> = broken.clone();
    let api = api_over(
        Arc::new(FakeHosts::new()),
        Arc::new(FakePools(no_pools)),
        Some(history),
    );
    for (host, job) in [("Not A Host", ""), ("", "UPPER-CASE"), ("h1", "a/b")] {
        let none = GetUsageRequest {
            host_id: host.to_owned(),
            job_id: job.to_owned(),
            ..GetUsageRequest::default()
        };
        assert_eq!(
            usage_of(&api, none).await.unwrap(),
            GetUsageResponse::default()
        );
    }
    assert_eq!(*broken.0.lock().unwrap(), 0);
    let failed = usage_of(&api, GetUsageRequest::default()).await;
    assert_eq!(
        refused(failed),
        (Code::Unavailable, USAGE_UNAVAILABLE.to_owned())
    );
    assert_eq!(*broken.0.lock().unwrap(), 1);
}

/// Asserts how each reason a booking ended is told, how a whole-host booking got its host,
/// and when there is an estimated run time. It catches a reason told as another, a
/// whole-host outcome given to a booking that was not whole-host, and an estimate made up
/// from a missing start.
#[test]
fn each_ending_is_told_by_its_state_and_reason() {
    let told = |reason| {
        let record = record(EndedRow {
            ended_reason: reason,
            ..ended("a", 10)
        });
        (record.end_state, record.ended_reason)
    };
    let cases = [
        (
            EndedReason::Released,
            wire::JobState::Unspecified,
            wire::ResultReason::Unspecified,
        ),
        (
            EndedReason::Expired,
            wire::JobState::Unspecified,
            wire::ResultReason::Unspecified,
        ),
        (
            EndedReason::Lost,
            wire::JobState::Lost,
            wire::ResultReason::Unspecified,
        ),
        (
            EndedReason::CancelledWhileWaiting,
            wire::JobState::Cancelled,
            wire::ResultReason::Cancelled,
        ),
        (
            EndedReason::FailedToStart,
            wire::JobState::Queued,
            wire::ResultReason::Unspecified,
        ),
        (
            EndedReason::StoppedAtWallLimit,
            wire::JobState::Failed,
            wire::ResultReason::WallLimit,
        ),
    ];
    for (reason, state, result) in cases {
        assert_eq!(
            told(reason),
            (i32::from(state), i32::from(result)),
            "{reason:?}"
        );
    }
    let booked = |whole_host| UsageBooked {
        whole_host,
        cpu_milli: 1,
        memory_bytes: 1,
    };
    let outcome = |booked: Option<UsageBooked>, drain_outcome| {
        whole_host(&EndedRow {
            booked,
            drain_outcome,
            ..ended("a", 10)
        })
    };
    let quiet = wire::WholeHostOutcome::QuietHost;
    let drained = wire::WholeHostOutcome::Drained;
    let none = wire::WholeHostOutcome::Unspecified;
    assert_eq!(outcome(Some(booked(true)), DrainOutcome::NoMark), quiet);
    assert_eq!(outcome(Some(booked(true)), DrainOutcome::MarkHeld), drained);
    assert_eq!(outcome(Some(booked(true)), DrainOutcome::MarkDropped), none);
    assert_eq!(outcome(Some(booked(true)), DrainOutcome::Unknown), none);
    assert_eq!(outcome(Some(booked(false)), DrainOutcome::None), none);
    assert_eq!(outcome(None, DrainOutcome::NoMark), none);
    let estimate = |run_ms, estimated_end| {
        estimated_run(&EndedRow {
            run_ms,
            estimated_end,
            ..ended("a", 1_000)
        })
    };
    assert_eq!(estimate(Some(400), Some(900)), Some(300));
    assert_eq!(estimate(Some(400), Some(500)), None);
    assert_eq!(estimate(None, Some(900)), None);
    assert_eq!(estimate(Some(400), None), None);
    assert_eq!(estimate(Some(2_000), Some(900)), None);
}

/// Writes `row` into the job store's state store, as a free of the job's allocation does.
fn job_ended(s: &Server, row: &EndedRow) {
    assert!(EndedHistory::new(&*s.store).put(row).unwrap());
}

/// Asserts that lease and job records are one listing in end order: three of each, ending
/// in turn, read two to a page come as lease, job on each page, with a token on each page
/// but the last, and one to a page come one at a time in the same order; that a job record
/// carries its job id and what it booked; and that the window and the host and job filters
/// apply to both sources before the page is cut. It catches the job store left unread, the
/// two sources listed one after the other instead of merged, a cursor that skips or repeats
/// a record where the listing crosses from one source to the other, and a filter applied to
/// one source only.
#[tokio::test]
async fn usage_pages_across_lease_and_job_records() {
    let s = server("usage-both");
    for n in 1..=6u64 {
        let host = match n {
            1 | 4 => "h1",
            _ => "h2",
        };
        let row = EndedRow {
            host: Some(HostId::new(host).unwrap()),
            booked: Some(UsageBooked {
                whole_host: false,
                cpu_milli: 1000,
                memory_bytes: 256 << 20,
            }),
            ..ended(&format!("r{n}"), n * 1000)
        };
        match n % 2 {
            1 => s.sink.allocation_ended(&row).unwrap(),
            _ => job_ended(&s, &row),
        }
    }
    let by_two = GetUsageRequest {
        page_size: 2,
        ..GetUsageRequest::default()
    };
    let read = pages(&s.api, by_two.clone(), || {}).await;
    assert_eq!(read, [["r1", "r2"], ["r3", "r4"], ["r5", "r6"]]);
    let first = usage_of(&s.api, by_two).await.unwrap();
    assert_eq!(first.next_page_token, "00000000000000002000/r2/job");
    assert_eq!(
        first.records[1],
        wire::UsageRecord {
            job_id: "r2".to_owned(),
            host_id: "h2".to_owned(),
            ended_at_ms: 2000,
            booked: Some(booking("r2", 1000, 256 << 20, 0, 0, &[])),
            ..wire::UsageRecord::default()
        }
    );
    let by_one = GetUsageRequest {
        page_size: 1,
        ..GetUsageRequest::default()
    };
    let read = pages(&s.api, by_one, || {}).await;
    assert_eq!(read, [["r1"], ["r2"], ["r3"], ["r4"], ["r5"], ["r6"]]);
    let window = GetUsageRequest {
        from_ms: Some(2000),
        to_ms: Some(5000),
        ..GetUsageRequest::default()
    };
    let window = usage_of(&s.api, window).await.unwrap();
    assert_eq!(job_ids(&window), ["r2", "r3", "r4"]);
    let h1 = GetUsageRequest {
        host_id: "h1".to_owned(),
        page_size: 2,
        ..GetUsageRequest::default()
    };
    let h1 = usage_of(&s.api, h1).await.unwrap();
    assert_eq!(
        (job_ids(&h1), h1.next_page_token.as_str()),
        (vec!["r1", "r4"], "")
    );
    for id in ["r3", "r4"] {
        let one = GetUsageRequest {
            job_id: id.to_owned(),
            ..GetUsageRequest::default()
        };
        assert_eq!(job_ids(&usage_of(&s.api, one).await.unwrap()), [id]);
    }
}

/// Asserts the listing where a lease record and a job record share their end and their id
/// (a job named like a lease id, ending in the same millisecond): both are listed, lease
/// first, one to a page, also with the window and the job filter set; and that a lease
/// token whose key a job record also has, read with a filter or window that leaves that
/// record out, does not list it. It catches the job record lost after a page that ends on
/// the lease record (the job store read strictly after the cursor's key only), and the
/// lookup of that record ignoring the request's filters.
#[tokio::test]
async fn usage_lists_a_lease_and_a_job_record_with_the_same_key() {
    let s = server("usage-tie");
    let lease = EndedRow {
        host: Some(HostId::new("h1").unwrap()),
        ..ended("1-2", 7000)
    };
    let job = EndedRow {
        host: Some(HostId::new("h2").unwrap()),
        ..ended("1-2", 7000)
    };
    s.sink.allocation_ended(&lease).unwrap();
    job_ended(&s, &job);
    s.sink.allocation_ended(&ended("1-3", 8000)).unwrap();
    let by_one = GetUsageRequest {
        page_size: 1,
        ..GetUsageRequest::default()
    };
    let first = usage_of(&s.api, by_one.clone()).await.unwrap();
    assert_eq!(first.records[0].host_id, "h1");
    assert_eq!(first.next_page_token, "00000000000000007000/1-2");
    let second = GetUsageRequest {
        page_token: first.next_page_token.clone(),
        ..by_one.clone()
    };
    let second = usage_of(&s.api, second).await.unwrap();
    assert_eq!(
        (
            second.records[0].host_id.as_str(),
            second.next_page_token.as_str()
        ),
        ("h2", "00000000000000007000/1-2/job")
    );
    let read = pages(&s.api, by_one.clone(), || {}).await;
    assert_eq!(read, [["1-2"], ["1-2"], ["1-3"]]);
    let narrowed = GetUsageRequest {
        job_id: "1-2".to_owned(),
        to_ms: Some(7001),
        ..by_one.clone()
    };
    let read = pages(&s.api, narrowed, || {}).await;
    assert_eq!(read, [["1-2"], ["1-2"]]);
    let after_lease = |request: GetUsageRequest| GetUsageRequest {
        page_token: first.next_page_token.clone(),
        ..request
    };
    let left_out = [
        GetUsageRequest {
            job_id: "1-3".to_owned(),
            ..GetUsageRequest::default()
        },
        GetUsageRequest {
            host_id: "h1".to_owned(),
            ..GetUsageRequest::default()
        },
        GetUsageRequest {
            from_ms: Some(7001),
            ..GetUsageRequest::default()
        },
        GetUsageRequest {
            to_ms: Some(7000),
            ..GetUsageRequest::default()
        },
    ];
    let expected = [vec!["1-3"], vec![], vec!["1-3"], vec![]];
    for (request, expected) in left_out.into_iter().zip(expected) {
        let page = usage_of(&s.api, after_lease(request.clone()))
            .await
            .unwrap();
        assert_eq!(job_ids(&page), expected, "{request:?}");
    }
}

/// Asserts the cursor's token: a lease record's is its key's text, as every answer gave
/// before job records were listed, and a job record's is the same followed by `/job`; each
/// reads back as itself, a lease named `job` included; a token with another suffix, a
/// second suffix, or a suffix alone reads as none; and a lease token at the last
/// millisecond a `u64` holds reads an empty last page. It catches a job token read as a
/// lease's (which would list the job record again), and a lookup past the last millisecond
/// that overflows.
#[tokio::test]
async fn the_page_token_names_the_source_of_its_record() {
    let key = |id: &str| EndedKey {
        ended_at_ms: 1000,
        id: AllocationId::parse(id).unwrap(),
    };
    let cursors = [
        (
            Cursor {
                key: key("1-2"),
                source: From::Lease,
            },
            "00000000000000001000/1-2",
        ),
        (
            Cursor {
                key: key("1-2"),
                source: From::Job,
            },
            "00000000000000001000/1-2/job",
        ),
        (
            Cursor {
                key: key("job"),
                source: From::Lease,
            },
            "00000000000000001000/job",
        ),
        (
            Cursor {
                key: key("job"),
                source: From::Job,
            },
            "00000000000000001000/job/job",
        ),
    ];
    for (cursor, text) in cursors {
        assert_eq!(cursor.text(), text);
        assert_eq!(Cursor::parse(text), Some(cursor), "{text}");
    }
    for text in [
        "00000000000000001000/1-2/lease",
        "00000000000000001000/1-2/job/job",
        "00000000000000001000/1-2/",
        "/job",
        "job",
        "",
    ] {
        assert_eq!(Cursor::parse(text), None, "{text}");
    }
    let s = server("usage-last-ms");
    s.sink.allocation_ended(&ended("1-2", u64::MAX)).unwrap();
    job_ended(&s, &ended("1-2", u64::MAX));
    let last = GetUsageRequest {
        page_token: format!("{}/1-2", u64::MAX),
        ..GetUsageRequest::default()
    };
    let last = usage_of(&s.api, last).await.unwrap();
    assert_eq!(
        (job_ids(&last), last.next_page_token.as_str()),
        (vec!["1-2"], "")
    );
}

/// Asserts that a server with the job store's history and no usage store's (`--usage-off`)
/// lists the job records alone, a page at a time; that one with only the usage store's lists
/// the lease records as before; and that either history failing (the job store's own state
/// store refusing a read among them), also on the lookup after a lease token, is
/// `UNAVAILABLE` `usage-unavailable`, never a page of the other source alone.
/// It catches job records hidden by `--usage-off`, and a failed source answered as an empty
/// part of the page.
#[tokio::test]
async fn usage_serves_each_source_alone_and_fails_when_either_fails() {
    let s = server("usage-one-source");
    for n in 1..=3u64 {
        job_ended(&s, &ended(&format!("j{n}"), n * 1000));
        s.sink
            .allocation_ended(&ended(&format!("l{n}"), n * 1000 + 1))
            .unwrap();
    }
    let lease_history: Arc<dyn History> = Arc::new(SinkHistory(s.sink.clone()));
    let job_history: Arc<dyn History> = Arc::new(StoreHistory(s.store.clone()));
    let with = |leases: Option<Arc<dyn History>>, jobs: Option<Arc<dyn History>>| {
        let hosts = Arc::new(FakeHosts::new());
        let api = api_over(hosts, Arc::new(FakePools(no_pools)), leases);
        let fleet = api.fleet.clone().with_job_history(jobs);
        api.with_fleet(fleet)
    };
    let by_two = GetUsageRequest {
        page_size: 2,
        ..GetUsageRequest::default()
    };
    let jobs_only = with(None, Some(job_history.clone()));
    let read = pages(&jobs_only, by_two.clone(), || {}).await;
    assert_eq!(read, [vec!["j1", "j2"], vec!["j3"]]);
    let leases_only = with(Some(lease_history.clone()), None);
    let read = pages(&leases_only, by_two.clone(), || {}).await;
    assert_eq!(read, [vec!["l1", "l2"], vec!["l3"]]);
    let broken = || Some(Arc::new(BrokenHistory(std::sync::Mutex::new(0))) as Arc<dyn History>);
    let unavailable = (Code::Unavailable, USAGE_UNAVAILABLE.to_owned());
    let lease_token = GetUsageRequest {
        page_token: "00000000000000001001/l1".to_owned(),
        ..GetUsageRequest::default()
    };
    let job_store_down = || {
        s.store.fail_read_after(0);
        usage_of(&s.api, GetUsageRequest::default())
    };
    assert_eq!(refused(job_store_down().await), unavailable);
    for (api, request) in [
        (with(broken(), Some(job_history.clone())), by_two.clone()),
        (with(Some(lease_history.clone()), broken()), by_two.clone()),
        (with(Some(lease_history.clone()), broken()), lease_token),
    ] {
        assert_eq!(refused(usage_of(&api, request).await), unavailable);
    }
}

/// Asserts that a context not served yet stays so when it is given the job store's history:
/// `GetUsage` still answers `UNIMPLEMENTED`. It catches a builder that serves a group with
/// none of its handles.
#[tokio::test]
async fn a_job_history_does_not_serve_an_unserved_group() {
    let s = server("usage-unserved");
    let job_history: Arc<dyn History> = Arc::new(StoreHistory(s.store.clone()));
    let fleet = Ctx::default().with_job_history(Some(job_history));
    assert_eq!(fleet, Ctx::default());
    let api = s.api.with_fleet(fleet);
    let answer = usage_of(&api, GetUsageRequest::default()).await;
    assert_eq!(refused(answer).0, Code::Unimplemented);
}
