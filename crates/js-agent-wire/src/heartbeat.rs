//! Keeping a session: [`Heartbeat`] up, [`HeartbeatReply`] down, and the results that
//! ride in the heartbeat until the server acknowledges them.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::check::{self, At};
use crate::command::Command;
use crate::error::{Rule, ValidationError};
use crate::hello::Phase;
use crate::limits;
use crate::refusal;

/// Where a held run is in its life on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldState {
    /// The container is being created.
    Starting,
    /// The container runs.
    Running,
    /// The container has exited and its result is not yet acknowledged.
    ExitedUnacked,
    /// A state this version does not know; kept so the rest of the message decodes.
    #[serde(other)]
    Unknown,
}

/// A run the agent holds. A run stays listed until its container and scratch are gone
/// and its result is acknowledged, so the server never sees a gap that would make it
/// requeue running work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldRun {
    /// The run's id.
    pub run_id: String,
    /// Where the run is.
    pub state: HeldState,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The job exited with status zero.
    Succeeded,
    /// The job exited with a nonzero status.
    Failed,
    /// The job was stopped or cancelled.
    Killed,
    /// The job passed its wall-time limit and was killed.
    TimedOut,
    /// The job passed its memory limit.
    OutOfMemory,
    /// No container could be started; no job ran.
    FailedToStart,
    /// An outcome this version does not know; kept so the rest of the message decodes.
    #[serde(other)]
    Unknown,
}

/// The result of one run, resent in every heartbeat until a [`ResultAck`] names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    /// The run's id.
    pub run_id: String,
    /// The lease the run was started under.
    pub lease_id: String,
    /// How the run ended.
    pub outcome: Outcome,
    /// The exit status, when the job exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A refusal code or short text saying why, when the outcome is not a clean exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A host's report that it can no longer be trusted clean (design 5.12), sent in every
/// heartbeat until the server acknowledges it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Taint {
    /// A short code for why the host is tainted.
    pub reason: String,
    /// The run whose job or cleanup led to it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The check that found it, when a check did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// The periodic message, agent to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The host's id.
    pub host_id: String,
    /// The session epoch from [`crate::Welcome`].
    pub epoch: u64,
    /// The host's phase.
    pub phase: Phase,
    /// Every run the agent holds, results not yet acknowledged included.
    #[serde(default)]
    pub held: Vec<HeldRun>,
    /// Results the server has not yet acknowledged.
    #[serde(default)]
    pub results: Vec<RunResult>,
    /// The taint report, until the server acknowledges it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<Taint>,
}

/// The server's verdict on a [`RunResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ack {
    /// The result was recorded.
    Accepted,
    /// The result was recorded earlier.
    Duplicate,
    /// The lease has moved on; the result is dropped.
    Stale,
    /// The server holds no such lease.
    UnknownLease,
    /// The result does not match the lease it names (another host's, or another run's).
    Mismatch,
    /// A verdict this version does not know. The agent keeps the result and resends it.
    #[serde(other)]
    Unknown,
}

/// The server's acknowledgement of one result; the agent then drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultAck {
    /// The run the result was for.
    pub run_id: String,
    /// The verdict.
    pub ack: Ack,
}

/// The answer to a [`Heartbeat`]. Every field may be left out, so a server that does
/// not yet send one is still understood.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatReply {
    /// What the agent is to do now.
    #[serde(default, deserialize_with = "commands_or_unknown")]
    pub commands: Vec<Command>,
    /// The results the server has dealt with.
    #[serde(default)]
    pub result_acks: Vec<ResultAck>,
    /// Whether the taint report was recorded; the agent then stops sending it.
    #[serde(default)]
    pub taint_ack: bool,
}

impl HeldRun {
    /// Checks the run id.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("held", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("held.run_id"), &self.run_id)
    }
}

/// The held list of a hello or heartbeat: at most [`limits::MAX_HELD`] entries, each with
/// a good run id, no run id twice.
pub(crate) fn check_held(at: At, held: &[HeldRun]) -> Result<(), ValidationError> {
    check::count(at, held.len(), limits::MAX_HELD)?;
    for run in held {
        run.check(at)?;
    }
    check::distinct(at, held.iter().map(|run| run.run_id.as_str()))
}

impl RunResult {
    /// Checks the ids, the detail text, and that the exit status agrees with the outcome:
    /// a success exits with 0, a failure does not.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("results", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("results.run_id"), &self.run_id)?;
        check::ident(at.field("results.lease_id"), &self.lease_id)?;
        let detail = at.field("results.detail");
        check::opt_text(detail, self.detail.as_ref(), limits::MAX_DETAIL_LEN)?;
        match (self.outcome, self.exit_code) {
            (Outcome::Succeeded, Some(code)) if code != 0 => {
                Err(at.field("results.exit_code").fail(Rule::Inconsistent))
            }
            (Outcome::Failed, Some(0)) => {
                Err(at.field("results.exit_code").fail(Rule::Inconsistent))
            }
            _ => Ok(()),
        }
    }
}

impl Taint {
    /// Checks the reason, the run id and the check name.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("taint", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::text(
            at.field("taint.reason"),
            &self.reason,
            limits::MAX_TAINT_TEXT_LEN,
        )?;
        check::opt_ident(at.field("taint.run_id"), self.run_id.as_ref())?;
        check::opt_text(
            at.field("taint.check"),
            self.check.as_ref(),
            limits::MAX_TAINT_TEXT_LEN,
        )
    }
}

impl Heartbeat {
    /// Checks the heartbeat against every bound of [`crate::limits`]: the host id, the
    /// held runs and results (no run id twice in either) and the taint. It reads no
    /// state: whether the epoch is current and whether the host is known is the
    /// server's. The server calls it on every heartbeat before it acts on one.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("heartbeat", refusal::INVALID_HEARTBEAT);
        crate::hello::check_host_id(at.field("host_id"), &self.host_id)?;
        check_held(at.field("held"), &self.held)?;
        let results = at.field("results");
        check::count(results, self.results.len(), limits::MAX_RESULTS)?;
        for result in &self.results {
            result.check(at)?;
        }
        check::distinct(
            results,
            self.results.iter().map(|result| result.run_id.as_str()),
        )?;
        match &self.taint {
            Some(taint) => taint.check(at),
            None => Ok(()),
        }
    }
}

impl ResultAck {
    /// Checks the run id.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("result_acks", refusal::INVALID_REPLY))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("result_acks.run_id"), &self.run_id)
    }
}

impl HeartbeatReply {
    /// Checks the reply the way the agent must before it acts on it: at most
    /// [`limits::MAX_COMMANDS`] commands, each valid (an unknown one passes and is
    /// skipped), no run id and no GPU index named by two starts, and at most [`limits::MAX_RESULTS`] acknowledgements, each with a good
    /// run id, none twice.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let at = At::new("reply", refusal::INVALID_REPLY);
        check::count(
            at.field("commands"),
            self.commands.len(),
            limits::MAX_COMMANDS,
        )?;
        for command in &self.commands {
            command.validate()?;
        }
        let starts = self.commands.iter().filter_map(|command| match command {
            Command::Start(spec) => Some(spec),
            _ => None,
        });
        let commands = at.field("commands");
        check::distinct(commands, starts.clone().map(|spec| spec.run_id.as_str()))?;
        check::distinct(commands, starts.flat_map(|spec| spec.gpu_indices.iter()))?;
        check::count(
            at.field("result_acks"),
            self.result_acks.len(),
            limits::MAX_RESULTS,
        )?;
        for ack in &self.result_acks {
            ack.check(at)?;
        }
        check::distinct(
            at.field("result_acks"),
            self.result_acks.iter().map(|ack| ack.run_id.as_str()),
        )?;
        Ok(())
    }
}

/// Decodes the commands one by one. A command that does not decode (a kind this version
/// does not know, or a start whose isolation it cannot classify) reads as
/// [`Command::Unknown`], so the commands around it still arrive and the agent never runs
/// the one it cannot classify.
fn commands_or_unknown<'de, D>(deserializer: D) -> Result<Vec<Command>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<Value>::deserialize(deserializer)?;
    Ok(items
        .into_iter()
        .map(|item| Command::deserialize(item).unwrap_or(Command::Unknown))
        .collect())
}
