//! The agent's side of the dispatch poll (`docs/design/multi-instance.md` section 0.2): a
//! long poll of [`DISPATCH_PATH`] by which the agent takes its host's starts and stops from
//! whichever server answers, and acknowledges them in its next poll.
//!
//! # When the agent polls
//!
//! A job-running agent reports [`js_agent_wire::DISPATCH_CAPABILITY`] in its hello. It polls
//! only for a session whose welcome says [`js_agent_wire::Welcome::dispatch_poll`], and for
//! no other:
//!
//! - A welcome without it (a server from before the poll, or one that does not serve it to
//!   this host) leaves the agent as it was: no poll is ever sent, and every start and stop
//!   arrives in a heartbeat's reply.
//! - A welcome with it makes the [`Session`] publish a [`Route`] (the host, the epoch, the
//!   session secret, the host credential in use) in the [`Cell`] it was given
//!   ([`Session::with_dispatch`]). A later welcome replaces the route, and one without the
//!   flag withdraws it.
//! - The poller ([`serve`]) runs on a thread of its own beside the heartbeat loop, and
//!   makes its transport through the same connector, server, login and CA the session's was
//!   made with, with the host credential the session adopted ([`Dial`]). It keeps that
//!   transport for every poll, as the session keeps its own for every heartbeat, and makes
//!   a new one only when a route names another credential or none could be made. The
//!   poll carries the session secret as a heartbeat does; the [`Route`] has no `Debug` text
//!   for it.
//!
//! # What a poll does
//!
//! [`Poller::poll`] posts the acknowledgements it holds and reads the answer:
//!
//! - **200.** The answer is decoded and checked as a whole ([`DispatchReply::validate`]);
//!   one that fails is an error, below. The acknowledgements sent are dropped (a poll that
//!   was answered carried them). Each message that passes [`Dispatched::validate`] becomes a
//!   [`Command::JobStart`] or [`Command::JobStop`], and the valid ones are handed to the
//!   worker in one [`Worker::try_deliver_commands`](crate::Worker::try_deliver_commands):
//!   they are acted on as the commands of a heartbeat's reply are, and the answer says
//!   nothing of the whole-node lease or the acknowledgements, so what the host holds of
//!   them stays as the last heartbeat's reply left it. When the worker took them, each message's acknowledgement
//!   ([`Dispatched::ack`]) goes into the next poll, and the agent polls again at once.
//!   A message that fails its check is neither acted on nor acknowledged, and costs the
//!   messages beside it nothing.
//! - **A worker with no room.** When the worker's queue is full the poller holds the messages
//!   ([`Next::Room`]) and posts no poll: it waits for the worker to take an order, which
//!   makes room ([`Worker::wait_room`](crate::Worker::wait_room), at most [`ROOM_WAIT`] before
//!   it looks at the cell again), hands the messages over, and polls at once with their
//!   acknowledgements. So they are acknowledged as soon as the worker has room, and not
//!   handed out again by the server after its claim on them (`dispatch_lease_secs`, 10 s by
//!   default) ends, while the worker is busy no longer than that. They keep the time their answer
//!   arrived, so a start's timeline counts the hold. Messages for a worker whose thread is
//!   gone, or that ended while they were held, are dropped and not acknowledged.
//! - **Counts and lines.** The poller counts the answers the worker could not queue when they
//!   arrived and the polls that failed ([`Counts`]), and logs one line for each such answer,
//!   each hand-over of held messages (with how long after their arrival) and each failed
//!   poll, with the counts; the lines go through the [`Cell`] to the session, which prints
//!   them with the worker's.
//! - **Anything else: a transport error, any status but 200 (404, 408, 429, 5xx, and the
//!   refusals 400, 401, 403, 409), or an answer that is not usable.** The agent waits as the
//!   session does after a failure ([`Backoff`](crate::Backoff): doubling, capped, jittered,
//!   at least the `Retry-After` the server gave) and polls again with the route it holds
//!   and the same acknowledgements, so a poll whose answer was lost is sent again whole, to
//!   whichever instance answers. A refusal is one instance's (a 404 is a server without the
//!   route during an update; a 409 `dispatch-not-served` or `unknown-host` is one whose
//!   registry lacks the host's claim) and the next poll may reach another, while the
//!   heartbeats go on succeeding; a wait ends early at a new welcome, which replaces the
//!   route.
//!
//! The poller never reads the heartbeat's state and the heartbeat never waits for the
//! poller. A drain ([`Session::begin_drain`]) and the end of the loop close the cell: the
//! poller takes no new route, and the thread ends once the poll in flight is answered (at
//! most [`REQUEST_LIMIT`](crate::REQUEST_LIMIT) later).
//!
//! A start is idempotent on the job id and fenced by the allocation id
//! (`lifecycle/mod.rs`), and a stop of a job the agent does not hold changes nothing, so a
//! message handed out twice (an acknowledgement lost, a server that died after answering)
//! starts one job. A poll's answer is not contact for the silence bound
//! (`--silence-stop`): only the welcome and the heartbeat's replies are.

mod cell;
mod poller;
mod serve;

pub use cell::{Cell, Route};
pub use poller::{Counts, Next, Poller, ROOM_WAIT};
pub use serve::{Dial, serve};

#[cfg(doc)]
use crate::session::Session;
#[cfg(doc)]
use js_agent_wire::{Command, DISPATCH_PATH, DispatchReply, Dispatched};
