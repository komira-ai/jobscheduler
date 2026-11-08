//! What `js job get` prints of why a job waits: the queue status, attempts and events the
//! server sets, as text lines and as proto3 JSON. The CLI has no code of its own for these
//! fields; it prints every field the server sets.

use js_proto::v1::{
    GetJobResponse, Job, JobEvent, JobEventKind, JobState, QueueStatus, QueueStatusCode,
};
use prost::Message as _;
use serde_json::{Value, json};

use crate::fake_jobs::{Answer, Fake};
use crate::jobs::js;
use crate::support::Scratch;

/// Asserts that `js job get` prints why a job waits, its attempts, each of its events and the
/// count of those dropped, as lines in descriptor order (the queue status's code by its short
/// name, the events by index, an event's absent host left out), and as proto3 JSON with the
/// new fields in camel case. The CLI has no code of its own for them: it prints every field
/// the server sets. Catches a field the text or JSON leaves out or names otherwise.
#[test]
fn a_jobs_queue_status_attempts_and_events_print_as_lines_or_json() {
    let scratch = Scratch::new("jobs_queue_status");
    let job = GetJobResponse {
        job: Some(Job {
            id: "j-1".to_owned(),
            state: JobState::Queued.into(),
            queue_status: Some(QueueStatus {
                code: QueueStatusCode::WaitingForWholeHost.into(),
                detail: "host a is being emptied for the job".to_owned(),
                since_ms: 9,
            }),
            attempts: 2,
            events: vec![
                JobEvent {
                    at_ms: 3,
                    kind: JobEventKind::Placed.into(),
                    detail: "placed on host a".to_owned(),
                    host_id: Some("a".to_owned()),
                },
                JobEvent {
                    at_ms: 9,
                    kind: JobEventKind::Requeued.into(),
                    detail: "queued again".to_owned(),
                    host_id: None,
                },
            ],
            dropped_events: 1,
            ..Job::default()
        }),
    };
    let fake = Fake::new();
    fake.answer("GetJob", vec![Answer::Message(job.encode_to_vec())]);
    let got = js(&scratch, fake.dialer(), &["job", "get", "--job-id", "j-1"]);
    let text = [
        "job.id: j-1",
        "job.state: queued",
        "job.queue_status.code: waiting-for-whole-host",
        "job.queue_status.detail: host a is being emptied for the job",
        "job.queue_status.since_ms: 9",
        "job.attempts: 2",
        "job.events[0].at_ms: 3",
        "job.events[0].kind: placed",
        "job.events[0].detail: placed on host a",
        "job.events[0].host_id: a",
        "job.events[1].at_ms: 9",
        "job.events[1].kind: requeued",
        "job.events[1].detail: queued again",
        "job.dropped_events: 1",
    ];
    assert_eq!(
        (got.code, got.out.as_str()),
        (0, format!("{}\n", text.join("\n")).as_str())
    );
    let got = js(
        &scratch,
        fake.dialer(),
        &["job", "get", "--job-id", "j-1", "--json"],
    );
    let printed: Value = serde_json::from_str(&got.out).unwrap();
    assert_eq!(
        printed,
        json!({ "job": {
            "id": "j-1",
            "state": "JOB_STATE_QUEUED",
            "queueStatus": {
                "code": "QUEUE_STATUS_CODE_WAITING_FOR_WHOLE_HOST",
                "detail": "host a is being emptied for the job",
                "sinceMs": "9",
            },
            "attempts": 2,
            "events": [
                { "atMs": "3", "kind": "JOB_EVENT_KIND_PLACED", "detail": "placed on host a", "hostId": "a" },
                { "atMs": "9", "kind": "JOB_EVENT_KIND_REQUEUED", "detail": "queued again" },
            ],
            "droppedEvents": 1,
        }})
    );
}
