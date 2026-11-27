//! One poll and what its answer does (see the module documentation of [`crate::dispatch`]).

use std::collections::BTreeSet;
use std::time::Duration;

use js_agent_wire::{Command, DISPATCH_PATH, DispatchAck, DispatchPoll, DispatchReply, Dispatched};
use serde_json::Value;

use super::Route;
use crate::backoff::{Backoff, Jitter};
use crate::transport::Transport;
use crate::worker::{Delivery, Worker};

/// The longest the poller's thread waits for the worker's room before it looks at the cell
/// again (a close, a new welcome) and tries the held messages once more. The room itself ends
/// the wait at once ([`Worker::wait_room`]); this bounds how late a close is seen.
pub const ROOM_WAIT: Duration = Duration::from_secs(1);

/// What to do after a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Poll again at once: the poll was answered.
    Again,
    /// Wait this long, then poll again: the poll failed.
    After(Duration),
    /// The worker had no room for the messages of an answer, which the poller holds: wait
    /// until the worker has taken more orders than this count ([`Worker::wait_room`]), then
    /// poll again; the poll hands the held messages over before it posts anything.
    Room(u64),
}

/// What the poller counts, from its start: each count rises once for each event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// The answers whose messages the worker could not queue when they arrived: its queue was
    /// full (they are held and handed over when it has room) or its thread was gone (they
    /// are dropped).
    pub undelivered: u64,
    /// The polls that failed: no transport could be made, the answer was lost, its status was
    /// not 200, or it was not a usable reply.
    pub failed: u64,
}

/// The messages of an answer the worker had no room for.
struct Held {
    /// What they stand for, in the answer's order.
    commands: Vec<Command>,
    /// Their acknowledgements, sent once the worker took them.
    acks: Vec<DispatchAck>,
    /// When the answer arrived, on the lifecycle's clock.
    received_ms: u64,
    /// How many orders the worker had taken before the last try ([`Worker::taken`]).
    seen: u64,
}

/// The state that outlives a poll: the acknowledgements not yet answered, the messages the
/// worker had no room for, the run of failures and the counts.
pub struct Poller<'a> {
    worker: &'a Worker,
    jitter: Box<dyn Jitter>,
    backoff: Backoff,
    /// The messages the worker took, acknowledged in the next poll and sent again until a
    /// poll that carried them is answered.
    acks: BTreeSet<DispatchAck>,
    /// The messages of the last answer while the worker has no room for them.
    held: Option<Held>,
    counts: Counts,
    /// What the poller wants logged, taken by [`Poller::take_lines`].
    lines: Vec<String>,
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
            held: None,
            counts: Counts::default(),
            lines: Vec::new(),
        }
    }

    /// Polls once on `transport` for the session of `route`, and acts on the answer. While
    /// it holds messages the worker had no room for it first hands them over
    /// ([`Poller::hand_over`]), and posts nothing while the worker still has no room.
    pub fn poll(&mut self, transport: &mut dyn Transport, route: &Route) -> Next {
        if let Some(next) = self.hand_over() {
            return next;
        }
        let poll = DispatchPoll {
            host_id: route.host_id.clone(),
            epoch: route.epoch,
            session: route.session.clone(),
            acks: self.acks.iter().cloned().collect(),
        };
        let body = serde_json::to_value(&poll).unwrap_or_default();
        match transport.post(DISPATCH_PATH, &body) {
            Err(_) => self.fail("it got no answer", None),
            Ok(answer) => match answer.status {
                200 => self.answered(answer.body),
                status => self.fail(&format!("it was answered {status}"), answer.retry_after),
            },
        }
    }

    /// Hands the messages held for the worker's room to the worker again. `None` when the
    /// poller holds none any more: none was held, the worker took them (their
    /// acknowledgements go into the next poll) or it has ended (they are dropped: it takes
    /// nothing more). [`Next::Room`] while the worker still has no room.
    pub fn hand_over(&mut self) -> Option<Next> {
        let held = self.held.take()?;
        let (count, received_ms) = (held.acks.len(), held.received_ms);
        if self.worker.ended() {
            self.lines.push(format!(
                "dispatch: the job worker ended: {count} held message(s) dropped"
            ));
            return None;
        }
        if self.offer(held) == Delivery::Delivered {
            let after = self.worker.clock().now_ms().saturating_sub(received_ms);
            self.lines.push(format!(
                "dispatch: the job worker took {count} held message(s) {after} ms after they \
                 arrived"
            ));
        }
        self.room()
    }

    /// The wait after one more failure, at least `retry_after`: no poll could be made.
    pub fn failed(&mut self, retry_after: Option<Duration>) -> Next {
        self.fail("no transport could be made", retry_after)
    }

    /// What the poller has counted so far.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The lines the poller wants logged since the last call: one for each answer the worker
    /// could not queue, each hand-over of held messages, and each failed poll, with the
    /// counts.
    pub fn take_lines(&mut self) -> Vec<String> {
        std::mem::take(&mut self.lines)
    }

    /// The wait after one more failure, at least `retry_after`: the poll failed as `what`
    /// says.
    fn fail(&mut self, what: &str, retry_after: Option<Duration>) -> Next {
        let wait = self.backoff.next(&mut *self.jitter, retry_after);
        self.counts.failed = self.counts.failed.saturating_add(1);
        self.lines.push(format!(
            "dispatch: a poll failed ({what}); the next in {} ms (failed polls: {})",
            wait.as_millis(),
            self.counts.failed
        ));
        Next::After(wait)
    }

    /// What a 200 does: the answer, if it is usable, is handed to the worker, or held while
    /// the worker has no room for it.
    fn answered(&mut self, body: Value) -> Next {
        let received_ms = self.worker.clock().now_ms();
        let reply = serde_json::from_value::<DispatchReply>(body).ok();
        let Some(reply) = reply.filter(|reply| reply.validate().is_ok()) else {
            return self.fail("its answer was not a usable reply", None);
        };
        self.acks.clear();
        self.backoff.reset();
        let (commands, acks): (Vec<_>, Vec<_>) =
            reply.messages.into_iter().filter_map(actionable).unzip();
        if commands.is_empty() {
            return Next::Again;
        }
        let count = acks.len();
        let held = Held {
            commands,
            acks,
            received_ms,
            seen: 0,
        };
        let why = match self.offer(held) {
            Delivery::Delivered => return Next::Again,
            Delivery::Full => "has no room for",
            Delivery::Gone => "is gone: dropped",
        };
        self.counts.undelivered = self.counts.undelivered.saturating_add(1);
        self.lines.push(format!(
            "dispatch: the job worker {why} {count} message(s) (undelivered answers: {})",
            self.counts.undelivered
        ));
        self.room().unwrap_or(Next::Again)
    }

    /// Tries `held` on the worker: its acknowledgements go into the next poll when it took
    /// them, and it is kept when the worker had no room.
    fn offer(&mut self, mut held: Held) -> Delivery {
        held.seen = self.worker.taken();
        let commands = held.commands.clone();
        let delivery = self.worker.try_deliver_commands(commands, held.received_ms);
        match delivery {
            Delivery::Delivered => self.acks.extend(held.acks),
            Delivery::Full => self.held = Some(held),
            Delivery::Gone => {}
        }
        delivery
    }

    /// [`Next::Room`] while messages are held.
    fn room(&self) -> Option<Next> {
        self.held.as_ref().map(|held| Next::Room(held.seen))
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
            reason: None,
        },
    };
    Some((command, ack))
}
