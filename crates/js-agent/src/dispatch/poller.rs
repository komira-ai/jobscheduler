//! One poll and what its answer does (see the module documentation of [`crate::dispatch`]).

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::{
    Command, DISPATCH_PATH, DispatchAck, DispatchPoll, DispatchReply, Dispatched, HeartbeatReply,
};
use serde_json::Value;

use super::Route;
use crate::backoff::{Backoff, Jitter};
use crate::transport::Transport;
use crate::worker::{Delivery, Worker};

/// What to do after a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Poll again at once: the poll was answered.
    Again,
    /// Wait this long, then poll again: the poll failed.
    After(Duration),
    /// Poll no more until the next welcome: the server does not hold this session or will
    /// not serve the poll to it.
    Halt,
}

/// The state that outlives a poll: the acknowledgements not yet answered, and the run of
/// failures.
pub struct Poller<'a> {
    worker: &'a Worker,
    jitter: Box<dyn Jitter>,
    backoff: Backoff,
    /// The messages the worker took, acknowledged in the next poll and sent again until a
    /// poll that carried them is answered.
    acks: BTreeSet<DispatchAck>,
}

impl<'a> Poller<'a> {
    /// A poller that hands what it is dispatched to `worker`, and waits after a failure as
    /// `jitter` says.
    pub fn new(worker: &'a Worker, jitter: Box<dyn Jitter>) -> Self {
        Self {
            worker,
            jitter,
            backoff: Backoff::default(),
            acks: BTreeSet::new(),
        }
    }

    /// Polls once on `transport` for the session of `route`, and acts on the answer.
    pub fn poll(&mut self, transport: &mut dyn Transport, route: &Route) -> Next {
        let poll = DispatchPoll {
            host_id: route.host_id.clone(),
            epoch: route.epoch,
            session: route.session.clone(),
            acks: self.acks.iter().cloned().collect(),
        };
        let body = serde_json::to_value(&poll).unwrap_or_default();
        match transport.post(DISPATCH_PATH, &body) {
            Err(_) => self.failed(None),
            Ok(answer) => match answer.status {
                200 => self.answered(answer.body),
                404 | 408 | 429 => self.failed(answer.retry_after),
                400..=499 => Next::Halt,
                _ => self.failed(answer.retry_after),
            },
        }
    }

    /// The wait after one more failure, at least `retry_after`: the poll failed, or no poll
    /// could be made.
    pub fn failed(&mut self, retry_after: Option<Duration>) -> Next {
        Next::After(self.backoff.next(&mut *self.jitter, retry_after))
    }

    /// What a 200 does: the answer, if it is usable, is handed to the worker.
    fn answered(&mut self, body: Value) -> Next {
        let reply = serde_json::from_value::<DispatchReply>(body).ok();
        let Some(reply) = reply.filter(|reply| reply.validate().is_ok()) else {
            return self.failed(None);
        };
        self.acks.clear();
        self.backoff.reset();
        let (commands, acks): (Vec<_>, Vec<_>) =
            reply.messages.into_iter().filter_map(actionable).unzip();
        let taken = !commands.is_empty()
            && self.worker.try_deliver(HeartbeatReply {
                commands,
                ..HeartbeatReply::default()
            }) == Delivery::Delivered;
        if taken {
            self.acks.extend(acks);
        }
        Next::Again
    }
}

/// The command a message that passes its check stands for, with its acknowledgement; `None`
/// for one that does not pass.
fn actionable(message: Dispatched) -> Option<(Command, DispatchAck)> {
    message.validate().ok()?;
    let ack = message.ack();
    let command = match message.start {
        Some(spec) => Command::JobStart(spec),
        None => Command::JobStop {
            job_id: message.job_id,
        },
    };
    Some((command, ack))
}
