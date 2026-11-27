//! Which wakes a transaction asks for, and the notices that carry them.

use js_state::{Cond, Notice, Txn, Version};
use js_types::HostId;

use super::{CHANNELS, HOST_CHANNEL, QUEUE_CHANNEL, Wake, wakes_of};
use crate::keys;

fn host(id: &str) -> HostId {
    HostId::new(id).expect("a host id")
}

fn notice(channel: &str, payload: &str) -> Notice {
    Notice {
        channel: channel.to_owned(),
        payload: payload.to_owned(),
    }
}

/// A queued entry put, an on-host entry deleted and a new dispatch message each ask for their
/// wake, each wake once however many operations ask for it. It catches a submit, a free or a
/// dispatch message that wakes no one.
#[test]
fn a_queued_job_a_free_and_a_new_message_ask_for_their_wakes() {
    let txn = Txn::new()
        .put(format!("{}job-1", keys::QUEUED), Vec::new(), Cond::Any)
        .put(format!("{}job-2", keys::QUEUED), Vec::new(), Cond::Any)
        .delete(format!("{}host-b/job-3", keys::ON_HOST), Cond::Any)
        .put(
            format!("{}host-a/1-ab-start", keys::DISPATCH),
            b"m".to_vec(),
            Cond::Absent,
        )
        .put(
            format!("{}host-b/2-cd-stop", keys::DISPATCH),
            b"m".to_vec(),
            Cond::Absent,
        );
    let wakes: Vec<Wake> = wakes_of(&txn).into_iter().collect();
    assert_eq!(
        wakes,
        [
            Wake::Queue,
            Wake::Host(host("host-a")),
            Wake::Host(host("host-b"))
        ]
    );
}

/// What changes no queue and writes no new message asks for nothing: a placement's own entries
/// (the queued entry deleted, an on-host entry put), a claim's rewrite of a message at its
/// version, an ack's delete, a new row outside the dispatch queue, a check, and a message key
/// whose host is not a host id. It catches wakes for every write (each claim would wake the
/// poll that made it) and a host wake for a key that names no host.
#[test]
fn what_queues_frees_and_dispatches_nothing_asks_for_nothing() {
    let txn = Txn::new()
        .delete(format!("{}job-1", keys::QUEUED), Cond::Any)
        .put(
            format!("{}host-a/job-1", keys::ON_HOST),
            Vec::new(),
            Cond::Any,
        )
        .put(
            format!("{}host-a/1-ab-start", keys::DISPATCH),
            b"m".to_vec(),
            Cond::Version(Version(3)),
        )
        .delete(format!("{}host-a/1-ab-start", keys::DISPATCH), Cond::Any)
        .put(
            keys::job(&crate::JobId::parse("job-1").expect("a job id")),
            b"j".to_vec(),
            Cond::Absent,
        )
        .put(
            format!("{}not a host/1-ab-stop", keys::DISPATCH),
            b"m".to_vec(),
            Cond::Absent,
        )
        .check("hostbook/host-a", Cond::Absent);
    assert!(wakes_of(&txn).is_empty(), "{:?}", wakes_of(&txn));
}

/// Each wake's notice is read back as the same wake, and a notice of another channel, or of the
/// host channel with no host id, asks for none. It catches a channel or payload that the sender
/// and the listener spell differently.
#[test]
fn a_wake_survives_its_notice_and_other_notices_ask_for_none() {
    for wake in [Wake::Queue, Wake::Host(host("host-a"))] {
        assert_eq!(Wake::of(&wake.notice()), Some(wake));
    }
    assert_eq!(Wake::Queue.notice(), notice(QUEUE_CHANNEL, ""));
    assert_eq!(
        Wake::Host(host("host-a")).notice(),
        notice(HOST_CHANNEL, "host-a")
    );
    assert_eq!(Wake::of(&notice("elsewhere", "host-a")), None);
    assert_eq!(Wake::of(&notice(HOST_CHANNEL, "not a host")), None);
    assert_eq!(CHANNELS, [QUEUE_CHANNEL, HOST_CHANNEL]);
}
