use serde_json::{Value, json};

use super::*;
use crate::{Rule, Welcome};

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn start_json(job: &str, allocation: &str) -> Value {
    json!({
        "job_id": job,
        "allocation_id": allocation,
        "image": format!("ubuntu:24.04@{DIGEST}"),
        "command": ["make"],
        "booked": {"cpu_millis": 1000, "memory_bytes": 1_073_741_824_u64},
        "wall_ms": 60_000,
    })
}

fn spec(job: &str, allocation: &str) -> JobStartSpec {
    serde_json::from_value(start_json(job, allocation)).unwrap()
}

fn poll(acks: Vec<DispatchAck>) -> DispatchPoll {
    DispatchPoll {
        host_id: "host-1".into(),
        epoch: 3,
        session: "s3cret".into(),
        acks,
    }
}

fn ack(kind: DispatchKind, allocation: &str) -> DispatchAck {
    DispatchAck {
        kind,
        allocation_id: allocation.into(),
    }
}

/// The field and rule a validation failed with.
fn failed(error: ValidationError) -> (&'static str, Rule, &'static str) {
    (error.field(), error.rule(), error.code())
}

// Catches a poll whose text drops or renames a field the server reads, an empty ack list
// written out (an older server is sent what it was before), and a session a log would show.
#[test]
fn a_poll_reads_back_as_written_and_hides_its_session() {
    let sent = poll(vec![
        ack(DispatchKind::Start, "a1"),
        ack(DispatchKind::Stop, "a1"),
    ]);
    let text = serde_json::to_value(&sent).unwrap();
    assert_eq!(
        text,
        json!({
            "host_id": "host-1",
            "epoch": 3,
            "session": "s3cret",
            "acks": [
                {"kind": "start", "allocation_id": "a1"},
                {"kind": "stop", "allocation_id": "a1"},
            ],
        })
    );
    assert_eq!(serde_json::from_value::<DispatchPoll>(text).unwrap(), sent);
    let bare = serde_json::to_value(poll(Vec::new())).unwrap();
    assert_eq!(
        bare,
        json!({"host_id": "host-1", "epoch": 3, "session": "s3cret"})
    );
    let debug = format!("{sent:?}");
    assert_eq!(
        debug,
        "DispatchPoll { host_id: \"host-1\", epoch: 3, session: \"..\", acks: \
         [DispatchAck { kind: Start, allocation_id: \"a1\" }, \
         DispatchAck { kind: Stop, allocation_id: \"a1\" }] }"
    );
}

// Catches a poll with no session that fails to decode (the server must read it to refuse it
// as a session mismatch) and an unknown kind that fails the whole poll's decode.
#[test]
fn a_poll_without_a_session_or_with_an_unknown_kind_still_decodes() {
    let text =
        json!({"host_id": "host-1", "epoch": 1, "acks": [{"kind": "pause", "allocation_id": "a"}]});
    let read: DispatchPoll = serde_json::from_value(text).unwrap();
    assert_eq!(read.session, "");
    assert_eq!(read.acks, vec![ack(DispatchKind::Unknown, "a")]);
}

// Catches a validator that lets a bad host id, session, ack id, unknown kind, repeated ack
// or an over-long list through, and one that refuses a poll with no session (the server
// answers that with the session refusal) or with no acks.
#[test]
fn a_poll_is_validated_field_by_field() {
    assert_eq!(poll(Vec::new()).validate(), Ok(()));
    let unsessioned = DispatchPoll {
        session: String::new(),
        ..poll(Vec::new())
    };
    assert_eq!(unsessioned.validate(), Ok(()));
    let code = refusal::INVALID_DISPATCH;
    let bad_host = DispatchPoll {
        host_id: "Host_1".into(),
        ..poll(Vec::new())
    };
    assert_eq!(
        failed(bad_host.validate().unwrap_err()),
        ("host_id", Rule::BadChars, code)
    );
    let bad_session = DispatchPoll {
        session: "-x".into(),
        ..poll(Vec::new())
    };
    assert_eq!(
        failed(bad_session.validate().unwrap_err()),
        ("session", Rule::BadChars, code)
    );
    let unknown = poll(vec![ack(DispatchKind::Unknown, "a1")]);
    assert_eq!(
        failed(unknown.validate().unwrap_err()),
        ("acks.kind", Rule::Inconsistent, code)
    );
    let bad_id = poll(vec![ack(DispatchKind::Start, "a/1")]);
    assert_eq!(
        failed(bad_id.validate().unwrap_err()),
        ("acks.allocation_id", Rule::BadChars, code)
    );
    let twice = poll(vec![
        ack(DispatchKind::Stop, "a1"),
        ack(DispatchKind::Stop, "a1"),
    ]);
    assert_eq!(
        failed(twice.validate().unwrap_err()),
        ("acks", Rule::Duplicate, code)
    );
    let many = (0..=limits::MAX_COMMANDS)
        .map(|n| ack(DispatchKind::Start, &format!("a{n}")))
        .collect();
    assert_eq!(
        failed(poll(many).validate().unwrap_err()),
        ("acks", Rule::TooMany, code)
    );
    let most = (0..limits::MAX_COMMANDS)
        .map(|n| ack(DispatchKind::Start, &format!("a{n}")))
        .collect();
    assert_eq!(poll(most).validate(), Ok(()));
}

// Catches a start or stop whose text drops the ids the agent acknowledges by, a stop that
// writes an empty start, and an ack that names another allocation or kind.
#[test]
fn a_start_and_a_stop_read_back_and_name_their_acks() {
    let start = Dispatched::start(spec("job-1", "a1"));
    let stop = Dispatched::stop("job-2".into(), "a2".into());
    let reply = DispatchReply {
        messages: vec![start.clone(), stop.clone()],
    };
    let text = serde_json::to_value(&reply).unwrap();
    assert_eq!(
        text["messages"][1],
        json!({"kind": "stop", "job_id": "job-2", "allocation_id": "a2"})
    );
    assert_eq!(text["messages"][0]["kind"], "start");
    assert_eq!(text["messages"][0]["start"]["allocation_id"], "a1");
    assert_eq!(
        serde_json::from_value::<DispatchReply>(text).unwrap(),
        reply
    );
    assert_eq!(start.ack(), ack(DispatchKind::Start, "a1"));
    assert_eq!(stop.ack(), ack(DispatchKind::Stop, "a2"));
    assert_eq!(
        serde_json::from_value::<DispatchReply>(json!({})).unwrap(),
        DispatchReply::default()
    );
}

// Catches a start this version cannot read (an isolation it does not know) that fails the
// whole reply, so the messages beside it would be lost, and an unknown kind that does.
#[test]
fn a_message_this_version_cannot_read_costs_only_itself() {
    let mut odd = start_json("job-1", "a1");
    odd["isolation"] = json!("vm-of-the-future");
    let text = json!({"messages": [
        {"kind": "start", "job_id": "job-1", "allocation_id": "a1", "start": odd},
        {"kind": "wake", "job_id": "job-3", "allocation_id": "a3"},
        {"kind": "stop", "job_id": "job-2", "allocation_id": "a2"},
    ]});
    let reply: DispatchReply = serde_json::from_value(text).unwrap();
    assert_eq!(reply.validate(), Ok(()));
    assert_eq!(reply.messages[0].start, None);
    let code = refusal::INVALID_REPLY;
    assert_eq!(
        failed(reply.messages[0].validate().unwrap_err()),
        ("start", Rule::Inconsistent, code)
    );
    assert_eq!(reply.messages[1].kind, DispatchKind::Unknown);
    assert_eq!(
        failed(reply.messages[1].validate().unwrap_err()),
        ("kind", Rule::Inconsistent, code)
    );
    assert_eq!(reply.messages[2].validate(), Ok(()));
}

// Catches a message the agent would act on although its ids are bad, its start names
// another job or allocation, its start is unpinned or invalid, or a stop carries a start.
#[test]
fn a_message_is_validated_before_the_agent_acts_on_it() {
    let code = refusal::INVALID_REPLY;
    assert_eq!(Dispatched::start(spec("job-1", "a1")).validate(), Ok(()));
    let bad_job = Dispatched::stop("job/1".into(), "a1".into());
    assert_eq!(
        failed(bad_job.validate().unwrap_err()),
        ("job_id", Rule::BadChars, code)
    );
    let bad_allocation = Dispatched::stop("job-1".into(), "a 1".into());
    assert_eq!(
        failed(bad_allocation.validate().unwrap_err()),
        ("allocation_id", Rule::BadChars, code)
    );
    for (job, allocation) in [("job-2", "a1"), ("job-1", "a2")] {
        let other = Dispatched {
            job_id: job.into(),
            allocation_id: allocation.into(),
            ..Dispatched::start(spec("job-1", "a1"))
        };
        assert_eq!(
            failed(other.validate().unwrap_err()),
            ("start", Rule::Inconsistent, code)
        );
    }
    let mut unpinned = spec("job-1", "a1");
    unpinned.image = "ubuntu:24.04".into();
    assert_eq!(
        Dispatched::start(unpinned).validate().unwrap_err().rule(),
        Rule::NotPinned
    );
    let mut invalid = spec("job-1", "a1");
    invalid.wall_ms = 0;
    assert!(Dispatched::start(invalid).validate().is_err());
    let stop_with_start = Dispatched {
        kind: DispatchKind::Stop,
        ..Dispatched::start(spec("job-1", "a1"))
    };
    assert_eq!(
        failed(stop_with_start.validate().unwrap_err()),
        ("start", Rule::Inconsistent, code)
    );
}

// Catches a reply over the cap that passes, and one at the cap that does not.
#[test]
fn a_reply_holds_at_most_the_command_cap() {
    let stop = Dispatched::stop("job-1".into(), "a1".into());
    let most = DispatchReply {
        messages: vec![stop.clone(); limits::MAX_COMMANDS],
    };
    assert_eq!(most.validate(), Ok(()));
    let over = DispatchReply {
        messages: vec![stop; limits::MAX_COMMANDS + 1],
    };
    assert_eq!(
        failed(over.validate().unwrap_err()),
        ("messages", Rule::TooMany, refusal::INVALID_REPLY)
    );
}

// Catches a welcome that writes `dispatch_poll: false` (an older agent is sent what it was
// before), one that loses the field on the way, and a welcome of a server from before the
// poll that reads as one that serves it.
#[test]
fn a_welcome_says_dispatch_poll_only_when_true() {
    let old = json!({
        "protocol": 1, "epoch": 2, "heartbeat_interval_ms": 5000, "grace_ms": 15000,
        "pool": "p", "session": "s",
    });
    let welcome: Welcome = serde_json::from_value(old.clone()).unwrap();
    assert!(!welcome.dispatch_poll);
    assert_eq!(serde_json::to_value(&welcome).unwrap(), old);
    let polls = Welcome {
        dispatch_poll: true,
        ..welcome
    };
    let text = serde_json::to_value(&polls).unwrap();
    assert_eq!(text["dispatch_poll"], true);
    assert_eq!(serde_json::from_value::<Welcome>(text).unwrap(), polls);
    assert!(format!("{polls:?}").contains("dispatch_poll: true"));
}
