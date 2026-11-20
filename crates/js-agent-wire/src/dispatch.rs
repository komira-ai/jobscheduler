//! The dispatch poll (`docs/design/multi-instance.md` section 0.2): the agent's long poll
//! [`DISPATCH_PATH`] by which it takes its host's starts and stops from whichever server
//! answers, and the acknowledgements it sends back in its next poll.
//!
//! - **Who polls.** An agent that can poll reports the hello capability
//!   [`DISPATCH_CAPABILITY`]; a server that serves the poll to it says so in the welcome
//!   ([`crate::Welcome::dispatch_poll`]). Only then does the agent poll, and only then does
//!   that server leave out of the heartbeat's reply the starts and stops its queue hands out
//!   by the poll. An agent without the capability, or one welcomed by a server that does not
//!   say so, never polls and takes every start and stop in the heartbeat's reply as before.
//! - **The poll.** [`DispatchPoll`] names the host, the session (as a heartbeat does) and the
//!   messages the agent acted on since its last poll ([`DispatchAck`]). The server deletes
//!   the acknowledged messages, hands out the host's due ones and, when none is due, holds
//!   the poll open for a few seconds until one is. [`DispatchReply`] carries what it handed
//!   out.
//! - **At least once.** A message the agent does not acknowledge is handed out again, on any
//!   server, once its hold on the server's store ends. A start is idempotent on the job id
//!   and fenced by the allocation id, and a stop of a job the agent no longer holds changes
//!   nothing, so a second delivery changes nothing.
//!
//! Every field is additive within protocol major 1: an unknown [`DispatchKind`] reads as
//! [`DispatchKind::Unknown`], and a start that does not decode reads as absent, so the
//! message around it fails [`Dispatched::validate`] and is neither acted on nor
//! acknowledged, while the messages beside it are.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::hello::check_host_id;
use crate::limits;
use crate::per_job::JobStartSpec;
use crate::refusal;

/// The hello capability of an agent that polls [`DISPATCH_PATH`] when its welcome says
/// [`crate::Welcome::dispatch_poll`]. Its value is `true`.
pub const DISPATCH_CAPABILITY: &str = "dispatch-poll";

/// The route of the poll, on the agent listener: a `POST` of a [`DispatchPoll`], answered
/// with a [`DispatchReply`]. It is a `POST` because the poll carries the session secret,
/// which a URL would leave in the logs of every proxy on the way.
pub const DISPATCH_PATH: &str = "/v1/agent/dispatch";

/// What a dispatched message tells the agent to do with one allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    /// Start the job of the allocation.
    Start,
    /// Stop the job of the allocation.
    Stop,
    /// A kind this version does not know: the message is not acted on, and an
    /// acknowledgement of one is refused.
    #[serde(other)]
    Unknown,
}

/// The agent's acknowledgement of one message it acted on, by its allocation and its kind,
/// so that the acknowledgement of a start never deletes the stop of the same allocation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DispatchAck {
    /// The kind of the message acted on.
    pub kind: DispatchKind,
    /// The allocation the message was for.
    pub allocation_id: String,
}

/// The agent's poll, agent to server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchPoll {
    /// The host's id.
    pub host_id: String,
    /// The session epoch from [`crate::Welcome`].
    pub epoch: u64,
    /// The session secret from [`crate::Welcome::session`], checked as a heartbeat's is
    /// ([`crate::Heartbeat::session`]). A poll with no `session` decodes as an empty text,
    /// which the server refuses as `session-mismatch`.
    #[serde(default)]
    pub session: String,
    /// The messages the agent acted on since its last poll that was answered, at most
    /// [`limits::MAX_COMMANDS`], no pair twice. Sent again in the next poll when a poll that
    /// carried them was not answered. Absent means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acks: Vec<DispatchAck>,
}

/// Every field but `session`, which prints as `".."`: the secret never reaches a log or a
/// panic message.
impl fmt::Debug for DispatchPoll {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchPoll")
            .field("host_id", &self.host_id)
            .field("epoch", &self.epoch)
            .field("session", &"..")
            .field("acks", &self.acks)
            .finish()
    }
}

impl DispatchPoll {
    /// Checks the poll the way the server must before it acts on it: the host id, a present
    /// session, and the acknowledgements (each of a known kind and an id, no pair twice, at
    /// most [`limits::MAX_COMMANDS`]). Its refusal code is [`refusal::INVALID_DISPATCH`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("dispatch", refusal::INVALID_DISPATCH);
        check_host_id(at.field("host_id"), &self.host_id)?;
        if !self.session.is_empty() {
            check::session(at.field("session"), &self.session)?;
        }
        let acks = at.field("acks");
        check::count(acks, self.acks.len(), limits::MAX_COMMANDS)?;
        for ack in &self.acks {
            if ack.kind == DispatchKind::Unknown {
                return Err(at.field("acks.kind").fail(Rule::Inconsistent));
            }
            check::ident(at.field("acks.allocation_id"), &ack.allocation_id)?;
        }
        check::distinct(acks, self.acks.iter())
    }
}

/// One message the server handed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatched {
    /// Start or stop.
    pub kind: DispatchKind,
    /// The job.
    pub job_id: String,
    /// The allocation the message is for: the placement it starts or stops. The agent
    /// acknowledges the message by it and the kind.
    pub allocation_id: String,
    /// For a start, the start, as a heartbeat's reply carries it
    /// ([`crate::Command::JobStart`]); absent for a stop. A start this version cannot decode
    /// reads as absent.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_start"
    )]
    pub start: Option<Box<JobStartSpec>>,
}

impl Dispatched {
    /// A start of `spec`, named by its job id and allocation id.
    #[must_use]
    pub fn start(spec: JobStartSpec) -> Self {
        Dispatched {
            kind: DispatchKind::Start,
            job_id: spec.job_id.clone(),
            allocation_id: spec.allocation_id.clone(),
            start: Some(Box::new(spec)),
        }
    }

    /// A stop of the job `job_id` on the allocation `allocation_id`.
    #[must_use]
    pub fn stop(job_id: String, allocation_id: String) -> Self {
        Dispatched {
            kind: DispatchKind::Stop,
            job_id,
            allocation_id,
            start: None,
        }
    }

    /// The acknowledgement of this message.
    #[must_use]
    pub fn ack(&self) -> DispatchAck {
        DispatchAck {
            kind: self.kind,
            allocation_id: self.allocation_id.clone(),
        }
    }

    /// Checks the message the way the agent must before it acts on it: a known kind, the
    /// ids, and for a start a start that passes [`JobStartSpec::validate`] and
    /// [`JobStartSpec::require_pinned`] and names the message's job and allocation; a stop
    /// carries no start. Its refusal code is [`refusal::INVALID_REPLY`], or the start's own.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("dispatch", refusal::INVALID_REPLY);
        check::ident(at.field("job_id"), &self.job_id)?;
        check::ident(at.field("allocation_id"), &self.allocation_id)?;
        match (self.kind, &self.start) {
            (DispatchKind::Start, Some(spec)) => {
                spec.validate()?;
                spec.require_pinned()?;
                let named =
                    (spec.job_id == self.job_id) & (spec.allocation_id == self.allocation_id);
                match named {
                    true => Ok(()),
                    false => Err(at.field("start").fail(Rule::Inconsistent)),
                }
            }
            (DispatchKind::Stop, None) => Ok(()),
            (DispatchKind::Unknown, _) => Err(at.field("kind").fail(Rule::Inconsistent)),
            _ => Err(at.field("start").fail(Rule::Inconsistent)),
        }
    }
}

/// The server's answer to a [`DispatchPoll`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReply {
    /// The messages handed out, in the order the agent takes them, at most
    /// [`limits::MAX_COMMANDS`]. Empty when none was due within the hold.
    #[serde(default)]
    pub messages: Vec<Dispatched>,
}

impl DispatchReply {
    /// Checks the number of messages. Each message is checked on its own
    /// ([`Dispatched::validate`]), so one the agent cannot act on never costs the others.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("dispatch", refusal::INVALID_REPLY);
        check::count(
            at.field("messages"),
            self.messages.len(),
            limits::MAX_COMMANDS,
        )
    }
}

/// Decodes a start, reading one that does not decode as absent.
fn lenient_start<'de, D>(deserializer: D) -> Result<Option<Box<JobStartSpec>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
