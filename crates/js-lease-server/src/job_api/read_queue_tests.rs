//! What `GetJob` and `ListJobs` show of a job's queue status, attempts and events
//! (`read/view.rs`, `docs/design/queue-status.md`): a job waiting for a whole host, one in
//! quota backoff and one that failed over and was placed, each read over both transports by
//! the rig of `read_tests.rs`; a job past the queue; and every code and kind.

use js_jobs::{JobEnd, Placement, StartFailure};
use js_proto::v1::{self, ListJobsRequest};
use js_types::{HostId, JobEventKind, QueueCode, QueueNote};

use super::read::view;
use super::read_tests::{BOOKED, Rig, clock, id, json_of, over_fake, queue, run, spec, start};

fn note(code: QueueCode, detail: &str, event: Option<JobEventKind>, attempt: bool) -> QueueNote {
    QueueNote {
        code,
        detail: detail.to_owned(),
        event,
        host: None,
        attempt,
    }
}

fn host(name: &str) -> HostId {
    HostId::new(name).expect("a host id")
}

fn shown_event(
    at_ms: u64,
    kind: v1::JobEventKind,
    detail: &str,
    host: Option<&str>,
) -> v1::JobEvent {
    v1::JobEvent {
        at_ms,
        kind: kind.into(),
        detail: detail.to_owned(),
        host_id: host.map(str::to_owned),
    }
}

fn status(code: v1::QueueStatusCode, detail: &str, since_ms: u64) -> Option<v1::QueueStatus> {
    Some(v1::QueueStatus {
        code: code.into(),
        detail: detail.to_owned(),
        since_ms,
    })
}

/// The time, in milliseconds, of the last event a note wrote: the store's clock at the write.
fn noted_at(written: Result<Option<js_jobs::JobRecord>, js_jobs::JobStoreError>) -> u64 {
    let job = written.expect("noted").expect("written");
    let last = job.queue.events().last().expect("an event");
    view::millis(last.at)
}

/// The queue fields of `job`, the rest left out.
fn queue_fields(job: &v1::Job) -> (Option<v1::QueueStatus>, u32, Vec<v1::JobEvent>, u32) {
    (
        job.queue_status.clone(),
        job.attempts,
        job.events.clone(),
        job.dropped_events,
    )
}

/// Asserts that get and list show the queue status of a job waiting for a whole host (the
/// detail moved without moving its time), one in quota backoff after a wait, and one that
/// failed over and was placed, each with its attempts and events at the store's times, and
/// that the gateway names them in camel case. Catches a field left out of the view, a time
/// read from another clock, events out of order, and a listing that shows other fields than
/// a get.
#[tokio::test]
async fn get_and_list_show_why_each_job_waits() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let whole = queue(jobs, "whole-1", spec(&[]));
    let quota = queue(jobs, "quota-1", spec(&[]));
    let over = queue(jobs, "over-1", spec(&[]));
    let waiting = note(
        QueueCode::WaitingForWholeHost,
        "no whole host is free",
        Some(JobEventKind::WaitingForWholeHost),
        false,
    );
    let first_whole = noted_at(jobs.note_queue(&whole, &[waiting]));
    let room = note(
        QueueCode::WaitingForCapacity,
        "no room",
        Some(JobEventKind::WaitingForCapacity),
        false,
    );
    let first_quota = noted_at(jobs.note_queue(&quota, std::slice::from_ref(&room)));
    rig.advance(std::time::Duration::from_millis(500));
    let emptying = note(
        QueueCode::WaitingForWholeHost,
        "host a is being emptied for the job",
        None,
        false,
    );
    jobs.note_queue(&whole, &[emptying]).expect("noted");
    let backoff = note(
        QueueCode::QuotaBackoff,
        "cloud refused offer big for quota",
        Some(JobEventKind::QuotaBackoff),
        true,
    );
    let backed_off = noted_at(jobs.note_queue(&quota, &[backoff]));
    assert!(
        backed_off >= first_quota + 500,
        "the time is the store's, moved on"
    );
    let refused = note(
        QueueCode::FailingOver,
        "cloud refused offer big for capacity",
        Some(JobEventKind::FailingOver),
        true,
    );
    let placed = QueueNote {
        host: Some(host("host-a")),
        ..note(
            QueueCode::Placed,
            "placed on host host-a",
            Some(JobEventKind::Placed),
            true,
        )
    };
    let placement = Placement {
        queue: vec![refused, placed],
        ..Placement::default()
    };
    let (placed, _) = jobs
        .place_with(&over, host("host-a"), BOOKED, placement)
        .expect("placed");
    let placed_at = view::millis(placed.placed_at.expect("placed"));
    use v1::{JobEventKind as K, QueueStatusCode as C};
    let expected = [
        (
            "whole-1",
            (
                status(
                    C::WaitingForWholeHost,
                    "host a is being emptied for the job",
                    first_whole,
                ),
                0,
                vec![shown_event(
                    first_whole,
                    K::WaitingForWholeHost,
                    "no whole host is free",
                    None,
                )],
                0,
            ),
        ),
        (
            "quota-1",
            (
                status(
                    C::QuotaBackoff,
                    "cloud refused offer big for quota",
                    backed_off,
                ),
                1,
                vec![
                    shown_event(first_quota, K::WaitingForCapacity, "no room", None),
                    shown_event(
                        backed_off,
                        K::QuotaBackoff,
                        "cloud refused offer big for quota",
                        None,
                    ),
                ],
                0,
            ),
        ),
        (
            "over-1",
            (
                status(C::Placed, "placed on host host-a", placed_at),
                2,
                vec![
                    shown_event(
                        placed_at,
                        K::FailingOver,
                        "cloud refused offer big for capacity",
                        None,
                    ),
                    shown_event(
                        placed_at,
                        K::Placed,
                        "placed on host host-a",
                        Some("host-a"),
                    ),
                ],
                0,
            ),
        ),
    ];
    let listed = rig.list(ListJobsRequest::default()).await.expect("listed");
    for (name, fields) in &expected {
        let got = rig.get_job(name).await.expect("read");
        assert_eq!(&queue_fields(&got), fields, "{name}");
        let in_list = listed
            .jobs
            .iter()
            .find(|job| job.id == *name)
            .expect("listed");
        assert_eq!(in_list, &got, "{name}: the listing shows what a get shows");
    }

    let (_, body) = rig.get("/v1/jobs/quota-1").await;
    let json = json_of(&body);
    let job = &json["job"];
    assert_eq!(
        job["queueStatus"]["code"],
        "QUEUE_STATUS_CODE_QUOTA_BACKOFF"
    );
    assert_eq!(job["queueStatus"]["sinceMs"], backed_off.to_string());
    assert_eq!(job["attempts"], 1);
    assert_eq!(job["events"][1]["kind"], "JOB_EVENT_KIND_QUOTA_BACKOFF");
    assert_eq!(job["events"][1]["atMs"], backed_off.to_string());
}

/// Asserts that a job past the queue shows `placed` while running, and none once it ended
/// without being placed, with its attempts and events kept in both. Catches a running job
/// shown as waiting, an ended job shown waiting for a host it no longer waits for, and
/// events dropped at the end.
#[tokio::test]
async fn a_job_past_the_queue_shows_placed_or_no_status_with_its_events() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let ran = queue(jobs, "ran-1", spec(&[]));
    let placed = QueueNote {
        host: Some(host("host-a")),
        ..note(
            QueueCode::Placed,
            "placed",
            Some(JobEventKind::Placed),
            true,
        )
    };
    let placement = Placement {
        queue: vec![placed],
        ..Placement::default()
    };
    jobs.place_with(&ran, host("host-a"), BOOKED, placement)
        .expect("placed");
    start(jobs, &ran);
    run(jobs, &ran);
    let running = rig.get_job("ran-1").await.expect("read");
    assert_eq!(running.state, i32::from(v1::JobState::Running));
    assert_eq!(
        running.queue_status.map(|status| status.code()),
        Some(v1::QueueStatusCode::Placed)
    );
    assert_eq!((running.attempts, running.events.len()), (1, 1));

    let expired = queue(jobs, "expired-1", spec(&[]));
    let room = note(
        QueueCode::WaitingForCapacity,
        "no room",
        Some(JobEventKind::WaitingForCapacity),
        false,
    );
    let waited = noted_at(jobs.note_queue(&expired, &[room]));
    let end = JobEnd::FailedToStart(StartFailure::NoCapacity);
    jobs.end_job(&expired, end).expect("ended");
    let ended = rig.get_job("expired-1").await.expect("read");
    assert_eq!(ended.queue_status, None);
    assert_eq!(
        ended.events,
        [shown_event(
            waited,
            v1::JobEventKind::WaitingForCapacity,
            "no room",
            None
        )]
    );
}

/// Asserts that every queue code and event kind of the record is shown as the job service's
/// value of the same name, and that a queued job nothing was recorded of shows no status, no
/// attempts and no events. Catches a code or kind mapped to another value.
#[tokio::test]
async fn every_code_and_kind_is_shown_as_its_own_value() {
    let clock = clock();
    let rig = Rig::over(over_fake(&clock), clock).await;
    let jobs = &rig.jobs;
    let fresh = queue(jobs, "fresh-1", spec(&[]));
    let shown = view::job(&jobs.get_job(&fresh).expect("reads").expect("kept"));
    assert_eq!(queue_fields(&shown), (None, 0, Vec::new(), 0));

    use v1::{JobEventKind as K, QueueStatusCode as C};
    let codes = [
        (QueueCode::WaitingForCapacity, C::WaitingForCapacity),
        (QueueCode::WaitingForWholeHost, C::WaitingForWholeHost),
        (QueueCode::QuotaBackoff, C::QuotaBackoff),
        (QueueCode::FailingOver, C::FailingOver),
        (QueueCode::Placed, C::Placed),
        (QueueCode::WaitingForMachine, C::WaitingForMachine),
    ];
    for (code, value) in codes {
        jobs.note_queue(&fresh, &[note(code, "", None, false)])
            .expect("noted");
        let record = jobs.get_job(&fresh).expect("reads").expect("kept");
        let shown = view::job(&record).queue_status.map(|status| status.code());
        assert_eq!(shown, Some(value), "{code:?}");
    }
    let kinds = [
        (JobEventKind::Unspecified, K::Unspecified),
        (JobEventKind::WaitingForCapacity, K::WaitingForCapacity),
        (JobEventKind::WaitingForWholeHost, K::WaitingForWholeHost),
        (JobEventKind::QuotaBackoff, K::QuotaBackoff),
        (JobEventKind::FailingOver, K::FailingOver),
        (JobEventKind::Placed, K::Placed),
        (JobEventKind::Requeued, K::Requeued),
        (JobEventKind::WaitingForMachine, K::WaitingForMachine),
    ];
    let notes: Vec<QueueNote> = kinds
        .iter()
        .map(|(kind, _)| note(QueueCode::Placed, "", Some(*kind), false))
        .collect();
    let kinded = queue(jobs, "kinds-1", spec(&[]));
    jobs.note_queue(&kinded, &notes).expect("noted");
    let record = jobs.get_job(&kinded).expect("reads").expect("kept");
    let shown: Vec<K> = view::job(&record)
        .events
        .iter()
        .map(v1::JobEvent::kind)
        .collect();
    let values: Vec<K> = kinds.iter().map(|(_, value)| *value).collect();
    assert_eq!(shown, values);
    let unread = id("unread-1");
    assert!(jobs.get_job(&unread).expect("reads").is_none());
}
