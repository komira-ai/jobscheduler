//! [`Wake`]: what a commit of the job store tells the other servers of its store to look at
//! again (`docs/design/multi-instance.md`, section 0 and G16), sent as notices of the store
//! ([`js_state::Store::notify`]) once the commit is made.
//!
//! - **The queue** ([`QUEUE_CHANNEL`], no payload): a commit that queues a job (a submit, a
//!   requeue, a retry: it puts an `idx/job/queued/` entry), one that frees an allocation (it
//!   deletes an `idx/alloc/host/` entry, as the free of a listing does) and a cancel. It wakes
//!   the placement of every server.
//! - **A host** ([`HOST_CHANNEL`], the host's id): a commit that writes a dispatch message for the
//!   host (it puts a new `dispatch/<host>/` row: a start or a stop). It wakes the held dispatch
//!   poll and held heartbeat of the host on whichever server holds them.
//!
//! The rows decide, and a wake only ends a wait early: what a server finds when it looks is what
//! it acts on, so a wake that comes twice, or for nothing new, costs one look.

use std::collections::BTreeSet;

use js_state::{Cond, Notice, Op, Txn};
use js_types::HostId;

use crate::keys;

/// The channel of the queue's notices.
pub const QUEUE_CHANNEL: &str = "js_jobs_queue";

/// The channel of a host's notices; the payload is the host's id.
pub const HOST_CHANNEL: &str = "js_jobs_host";

/// Every channel the job store sends notices on: what a server listens on.
pub const CHANNELS: [&str; 2] = [QUEUE_CHANNEL, HOST_CHANNEL];

/// What a notice of the job store asks a server to look at again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Wake {
    /// The queue: a job may be placeable now.
    Queue,
    /// The dispatch queue of the host.
    Host(HostId),
}

impl Wake {
    /// The wake `notice` asks for; `None` for a notice of another channel, or of a host channel
    /// whose payload is not a host id.
    #[must_use]
    pub fn of(notice: &Notice) -> Option<Wake> {
        match notice.channel.as_str() {
            QUEUE_CHANNEL => Some(Wake::Queue),
            HOST_CHANNEL => HostId::new(&notice.payload).ok().map(Wake::Host),
            _ => None,
        }
    }

    /// The notice that asks for this wake.
    #[must_use]
    pub fn notice(&self) -> Notice {
        let (channel, payload) = match self {
            Wake::Queue => (QUEUE_CHANNEL, String::new()),
            Wake::Host(host) => (HOST_CHANNEL, host.to_string()),
        };
        Notice {
            channel: channel.to_owned(),
            payload,
        }
    }
}

/// The wakes a commit of `txn` asks for, each once.
pub(crate) fn wakes_of(txn: &Txn) -> BTreeSet<Wake> {
    txn.ops().iter().filter_map(wake_of).collect()
}

/// The wake the operation `op` asks for, if any.
fn wake_of(op: &Op) -> Option<Wake> {
    match op {
        Op::Put { key, .. } if key.starts_with(keys::QUEUED) => Some(Wake::Queue),
        Op::Delete { key, .. } if key.starts_with(keys::ON_HOST) => Some(Wake::Queue),
        Op::Put {
            key,
            cond: Cond::Absent,
            ..
        } => {
            let (host, _) = key.strip_prefix(keys::DISPATCH)?.split_once('/')?;
            HostId::new(host).ok().map(Wake::Host)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "wake_tests.rs"]
mod tests;
