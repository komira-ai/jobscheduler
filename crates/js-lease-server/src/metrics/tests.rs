//! The text a scrape answers, over a server whose state the test sets: jobs in every state,
//! hosts with job allocations and a whole-host mark, and events fed to the recorder.
//! The registry, the server and the store read one manual clock, and every duration the
//! recorder is told is given by the test, so nothing here sleeps or reads the wall clock.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request as HttpRequest, StatusCode};
use js_elastic::{Acquired, Reason, Refusal, RefusalCode};
use js_jobs::{Booked, JobEnd, JobId, JobState, NewJob, Output, StartFailure};
use js_types::HostId;
use tower::ServiceExt;

use super::render::{label, render};
use super::scrape::{TEXT_FORMAT, snapshot};
use super::*;
use crate::agent_api::job_rig::{BOOKED, Rig, clocked_rig, placed, spec};
use crate::api::Shared;
use crate::config::parse_token;
use crate::job_expiry::JobLimits;

/// The client token of the fixture's server.
const TOKEN: [u8; 40] = [b'q'; 40];

/// A server over the job rig `rig`'s agent side.
pub(super) fn shared_over(rig: &Rig) -> Arc<Shared> {
    let token = parse_token(&TOKEN).unwrap();
    Arc::new(Shared::new(token, Arc::clone(&rig.agents)))
}

/// What the fixture is made of, by the ids a leak would show.
struct Fixture {
    rig: Rig,
    shared: Arc<Shared>,
    recorder: Arc<Recorder>,
    /// Every job id.
    ids: Vec<String>,
}

fn job_id(text: &str) -> JobId {
    JobId::parse(text).unwrap()
}

fn queue(rig: &Rig, id: &str) -> JobId {
    let new = NewJob {
        id: Some(job_id(id)),
        spec: spec(),
        priority: 0,
        client: None,
    };
    rig.jobs.insert_queued(new).unwrap();
    job_id(id)
}

fn exited(code: i32) -> JobEnd {
    JobEnd::Exited {
        exit_code: code,
        output: Output::default(),
    }
}

/// The server of the golden: four hosts of 8 cores and 16 GiB; one job in each state, the six
/// that hold an allocation (live or releasing) booking 1.5 cores and 1 GiB each on `host-a`, so it is booked
/// past its shape; a placed job booking the whole of `host-c`; two queued jobs, the older
/// submitted 90 s before the scrape and 64 ms more of the store's own clock (2 ms for each of
/// the 32 commits after it), the younger 5 s before it; `host-d` draining; and the recorder told of two placements, a start failure of
/// each code, two losses, a failed write, two job passes and three launches of the pool `gpu`,
/// one of them refused for capacity.
async fn fixture(name: &str) -> Fixture {
    let rig = clocked_rig(name, true, JobLimits::default());
    let mut rig = rig;
    for host in ["host-a", "host-b", "host-c", "host-d"] {
        rig.register(host, true);
    }
    let shared = shared_over(&rig);
    let jobs = &rig.jobs;
    let queued = queue(&rig, "alpha-queued");
    let placed_job = placed(jobs, "bravo-placed", "host-a", spec(), BOOKED).0.id;
    let starting = placed(jobs, "charlie-starting", "host-a", spec(), BOOKED)
        .0
        .id;
    jobs.transition(&starting, JobState::Placed, JobState::Starting)
        .unwrap();
    let mut ran = Vec::new();
    for id in ["delta-running", "echo-ok", "foxtrot-bad", "golf-gone"] {
        let id = placed(jobs, id, "host-a", spec(), BOOKED).0.id;
        jobs.transition(&id, JobState::Placed, JobState::Starting)
            .unwrap();
        jobs.transition(&id, JobState::Starting, JobState::Running)
            .unwrap();
        ran.push(id);
    }
    jobs.end_job(&ran[1], exited(0)).unwrap();
    jobs.end_job(&ran[2], exited(1)).unwrap();
    jobs.end_job(&ran[3], JobEnd::Lost).unwrap();
    let cancelled = queue(&rig, "hotel-cancelled");
    jobs.request_cancel(&cancelled).unwrap();
    rig.advance(Duration::from_secs(85));
    let young = queue(&rig, "kilo-young");
    rig.advance(Duration::from_secs(5));
    let refused = queue(&rig, "india-refused");
    let failure = JobEnd::FailedToStart(StartFailure::NoCapacity);
    jobs.end_job(&refused, failure).unwrap();
    let whole = placed(jobs, "juliet-whole", "host-c", spec(), Booked::WholeHost)
        .0
        .id;
    rig.agents
        .registry
        .drain(&HostId::new("host-d").unwrap())
        .unwrap();
    let recorder = Arc::new(Recorder::default());
    recorder.job_placed(Duration::from_secs(1));
    recorder.job_placed(Duration::from_secs(45));
    for failure in [
        StartFailure::NoCapacity,
        StartFailure::StartTimeout,
        StartFailure::ExclusiveNotReady,
        StartFailure::AgentRefused,
    ] {
        recorder.start_failed(failure);
    }
    recorder.job_lost();
    recorder.job_lost();
    recorder.store_write_failed();
    recorder.job_pass_took(Duration::from_millis(3));
    recorder.job_pass_took(Duration::from_millis(30));
    let capacity = Acquired::Refused(Refusal {
        code: RefusalCode::Capacity,
        reason: Reason::Api("no capacity in the zone".to_owned()),
    });
    recorder.acquire_answered("gpu", &capacity);
    recorder.acquire_answered("gpu", &Acquired::Unknown);
    recorder.acquire_answered("gpu", &Acquired::Unknown);
    let mut ids = Vec::new();
    let named = [
        queued, placed_job, starting, cancelled, refused, whole, young,
    ];
    ids.extend(named.iter().chain(&ran).map(ToString::to_string));
    Fixture {
        rig,
        shared,
        recorder,
        ids,
    }
}

/// The text of a scrape of `fixture`, reading the job tables two jobs at a time, so the read
/// crosses pages.
async fn scraped(fixture: &Fixture) -> String {
    let page = NonZeroUsize::new(2).unwrap();
    let seen = snapshot(&fixture.shared, page).await.unwrap();
    render(&seen, &fixture.recorder)
}

/// Asserts the whole text of a scrape of a known state: the jobs by state (the job that
/// failed to start counted under `failed-to-start`), the older queued job's wait (not the
/// younger's, which is listed after it), each host's
/// CPU and memory booked and free (`host-a` with its six allocations, booked past its shape for
/// CPU, so none free; `host-c` booked whole by a job; `host-b` and `host-d` free), `host-d`'s `draining`
/// mark, and
/// every event the recorder was told, histograms cumulative. It catches a family that is
/// missing, renamed or out of order, a count read from the wrong place, a booking left out,
/// and a free figure that is not the shape less what is booked.
#[tokio::test]
async fn the_text_of_a_known_state_is_the_golden() {
    let fixture = fixture("golden").await;
    let text = scraped(&fixture).await;
    assert_eq!(text, include_str!("golden.prom"), "the scrape now:\n{text}");
}

/// Asserts that the text names none of the fixture's job ids, the client token or the agents'
/// join token: a scrape shows states, codes, hosts, pools and marks only.
#[tokio::test]
async fn the_text_holds_no_job_id_or_token() {
    let fixture = fixture("no_secrets").await;
    let text = scraped(&fixture).await;
    let tokens = [String::from_utf8(TOKEN.to_vec()).unwrap(), "j".repeat(40)];
    for secret in fixture.ids.iter().chain(&tokens) {
        assert!(!text.contains(secret.as_str()), "{secret} is in:\n{text}");
    }
}

/// Asserts that a server without the job tables writes no job family, whatever its hosts,
/// and still writes the event families, at zero. It catches a scrape that fails, or writes
/// zero jobs, when the job service is off.
#[tokio::test]
async fn without_the_job_tables_no_job_family_is_written() {
    let seen = super::render::Snapshot {
        jobs: None,
        hosts: Vec::new(),
    };
    let text = render(&seen, &Recorder::default());
    assert!(!text.contains("js_jobs{"), "{text}");
    assert!(!text.contains("js_queue_oldest_age_seconds"), "{text}");
    assert!(text.contains("\njs_job_placements_total 0\n"), "{text}");
    assert!(text.contains("\njs_jobs_lost_total 0\n"), "{text}");
}

/// Asserts what a scrape reads of a server whose agent side holds no job tables (the job
/// service off): its hosts, booked and free, and no job part. It catches a scrape that fails,
/// or reads zero jobs, when the job service is off.
#[tokio::test]
async fn a_scrape_without_the_job_tables_reads_the_hosts_alone() {
    let mut rig = crate::agent_api::job_rig::rig("metrics_no_tables", false);
    rig.register("host-a", true);
    let shared = shared_over(&rig);
    let seen = snapshot(&shared, NonZeroUsize::MIN).await.unwrap();
    let free = |total| super::render::Split::of(total, 0);
    let host = super::render::HostSeen {
        host: HostId::new("host-a").unwrap(),
        cpu: free(8000),
        memory: free(17_179_869_184),
        mark: None,
    };
    let expected = super::render::Snapshot {
        jobs: None,
        hosts: vec![host],
    };
    assert_eq!(seen, expected);
}

/// Asserts the bucket a duration falls in: one on a bound falls in that bound's bucket, one
/// just over it in the next, one over the last bound only in `+Inf`; the buckets are
/// cumulative and the sum is exact. It catches an exclusive bound and a bucket that is not
/// cumulative.
#[test]
fn a_duration_on_a_bound_falls_in_its_bucket_and_one_past_the_last_only_in_inf() {
    let histogram = Histogram::new(QUEUE_WAIT);
    histogram.observe(Duration::from_secs(1));
    histogram.observe(Duration::from_millis(1_001));
    histogram.observe(Duration::from_secs(86_401));
    let (below, count, sum) = histogram.read();
    assert_eq!(&below[..3], &[("1", 1), ("5", 2), ("30", 2)]);
    assert_eq!(below.last(), Some(&("86400", 2)));
    assert_eq!((count, sum), (3, 86_403_001_000));
}

/// Asserts that only a refusal for capacity or quota counts as a failover, and that every
/// answer counts as a launch asked, each pool on its own. It catches a failover counted for a
/// refusal of the request itself (`shape`, `denied`) or for an answer that is not a refusal.
#[test]
fn only_a_capacity_or_quota_refusal_fails_over() {
    let recorder = Recorder::default();
    let refused = |code| {
        Acquired::Refused(Refusal {
            code,
            reason: Reason::Api("refused".to_owned()),
        })
    };
    let answers = [
        refused(RefusalCode::Capacity),
        refused(RefusalCode::Quota),
        refused(RefusalCode::Shape),
        refused(RefusalCode::Denied),
        Acquired::Unknown,
        Acquired::Deferred {
            wait: Duration::from_secs(5),
        },
    ];
    for answer in &answers {
        recorder.acquire_answered("gpu", answer);
    }
    recorder.acquire_answered("cpu", &refused(RefusalCode::Quota));
    let gpu = Acquires {
        asked: 6,
        failed_over: 2,
    };
    let cpu = Acquires {
        asked: 1,
        failed_over: 1,
    };
    let expected = BTreeMap::from([("cpu".to_owned(), cpu), ("gpu".to_owned(), gpu)]);
    assert_eq!(recorder.acquires(), expected);
}

/// Asserts that a pool's name is written escaped in the text, so a name with a quote or a line
/// feed cannot end its label or add a line.
#[test]
fn a_pool_name_is_written_escaped() {
    let recorder = Recorder::default();
    recorder.acquire_answered("a\"b\nc", &Acquired::Unknown);
    let seen = super::render::Snapshot {
        jobs: None,
        hosts: Vec::new(),
    };
    let text = render(&seen, &recorder);
    let line = "\njs_acquires_total{pool=\"a\\\"b\\nc\"} 1\n";
    assert!(text.contains(line), "{line} is not in:\n{text}");
}

/// Asserts that a label value escapes the three characters the text format gives a meaning
/// (backslash, double quote, line feed) and keeps every other, so a pool name cannot end its
/// label or add a line.
#[test]
fn a_label_value_escapes_backslash_quote_and_line_feed() {
    assert_eq!(label("a\"b\\c\nd é-1"), "a\\\"b\\\\c\\nd é-1");
}

/// Asserts that the meter that counts nothing reads no time: a pass timed by it took zero,
/// and what the work returned is passed through.
#[test]
fn the_off_meter_times_nothing() {
    let (done, took) = Meter::off().timed(|| 7);
    assert_eq!((done, took), (7, Duration::ZERO));
}

/// A request for `path` from `peer`, as the listener hands it on.
fn from(peer: Option<&str>, path: &str) -> HttpRequest<Body> {
    let mut request = HttpRequest::get(path).body(Body::empty()).unwrap();
    if let Some(peer) = peer {
        let addr: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    request
}

/// The status, the content type and the body of the metrics router's answer to `request`.
async fn answer(fixture: &Fixture, request: HttpRequest<Body>) -> (StatusCode, String, String) {
    let app = router(Arc::clone(&fixture.shared), Arc::clone(&fixture.recorder));
    let reply = app.oneshot(request).await.unwrap();
    let kind = reply.headers().get("content-type");
    let kind = kind
        .map(|kind| kind.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let status = reply.status();
    let body = to_bytes(reply.into_body(), 1 << 20).await.unwrap();
    (status, kind, String::from_utf8(body.to_vec()).unwrap())
}

/// Asserts who the route answers: a private peer (v4 or v6) gets 200 with the text format's
/// content type and the scrape; a public peer, a request whose peer is unknown and another
/// path get the answer of an unknown route. It catches metrics served to the internet from a
/// wildcard listener.
#[tokio::test]
async fn only_a_private_peer_is_answered_and_only_on_metrics() {
    let fixture = fixture("route").await;
    for peer in ["10.0.0.9:4000", "[::1]:4000"] {
        let (status, kind, body) = answer(&fixture, from(Some(peer), "/metrics")).await;
        assert_eq!(
            (status, kind.as_str()),
            (StatusCode::OK, TEXT_FORMAT),
            "{peer}"
        );
        assert_eq!(body, scraped(&fixture).await);
    }
    let unknown = crate::api::not_found();
    let unknown = to_bytes(unknown.into_body(), 1 << 20).await.unwrap();
    let unknown = String::from_utf8(unknown.to_vec()).unwrap();
    let refused = [
        (Some("192.0.2.1:4000"), "/metrics"),
        (None, "/metrics"),
        (Some("10.0.0.9:4000"), "/other"),
    ];
    for (peer, path) in refused {
        let (status, _, body) = answer(&fixture, from(peer, path)).await;
        assert_eq!(
            (status, &body),
            (StatusCode::NOT_FOUND, &unknown),
            "{peer:?} {path}"
        );
    }
}

/// The log lines a test's subscriber wrote.
#[derive(Clone, Default)]
struct Logged(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logged {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that a scrape whose read of the job tables fails answers 503 with a line that says
/// so, and logs the failure with the store's error, and that one whose read of the live
/// allocations fails (the jobs read) answers the same: a scrape never shows a part of the
/// state as all of it.
#[tokio::test]
async fn a_failed_read_of_the_job_tables_answers_503() {
    let fixture = fixture("read_fails").await;
    let logged = Logged::default();
    let writer = logged.clone();
    crate::api::rig::process_sink();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _scope = tracing::subscriber::set_default(subscriber);
    // The jobs fit in one page of the server's size, so the second read is the allocations'.
    for reads in [0, 1] {
        fixture.rig.store.fail_read_after(reads);
        let (status, _, body) = answer(&fixture, from(Some("127.0.0.1:1"), "/metrics")).await;
        let expected = (
            StatusCode::SERVICE_UNAVAILABLE,
            "the job tables could not be read\n",
        );
        assert_eq!((status, body.as_str()), expected, "after {reads} reads");
    }
    let text = String::from_utf8(logged.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    for line in lines {
        assert!(
            line.contains("a scrape could not read the job tables"),
            "{line}"
        );
        assert!(line.contains("error="), "{line}");
    }
}
