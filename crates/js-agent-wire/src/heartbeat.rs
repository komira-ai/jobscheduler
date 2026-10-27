//! Keeping a session: [`Heartbeat`] up, [`HeartbeatReply`] down, and the results that
//! ride in the heartbeat until the server acknowledges them.

use std::collections::BTreeSet;
use std::fmt;

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
    /// The job exited with a nonzero status; an exit status of zero is refused.
    Failed,
    /// The job was stopped or cancelled.
    Killed,
    /// The job passed its wall-time limit and was killed.
    TimedOut,
    /// The job passed its memory limit.
    OutOfMemory,
    /// No container could be started; no job ran, so an exit status of zero is refused.
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

/// Why a host will not take a whole-node lease (design 5.4). The scheduler acts on the
/// code only and never parses the detail.
///
/// An unknown code text decodes as [`RefusalReason::SetupFailed`]: a refusal this version
/// cannot classify still counts as a failed prepare, never as a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RefusalReason {
    /// The host is not quiet: other work runs on it.
    NotQuiet,
    /// The host's noise is over the pool's bound.
    NoiseOverBound,
    /// The agent's setup (checks or the readiness hook) failed. Also what a code this
    /// version does not know reads as.
    SetupFailed,
    /// The host is not safe to hand over whole.
    HostUnsafe,
    /// The host cannot serve a whole-node lease at all.
    Unsupported,
}

impl From<String> for RefusalReason {
    fn from(text: String) -> Self {
        match text.as_str() {
            "not-quiet" => Self::NotQuiet,
            "noise-over-bound" => Self::NoiseOverBound,
            "host-unsafe" => Self::HostUnsafe,
            "unsupported" => Self::Unsupported,
            _ => Self::SetupFailed,
        }
    }
}

impl From<RefusalReason> for String {
    fn from(reason: RefusalReason) -> Self {
        let text = match reason {
            RefusalReason::NotQuiet => "not-quiet",
            RefusalReason::NoiseOverBound => "noise-over-bound",
            RefusalReason::SetupFailed => "setup-failed",
            RefusalReason::HostUnsafe => "host-unsafe",
            RefusalReason::Unsupported => "unsupported",
        };
        text.to_string()
    }
}

/// An agent's refusal of an exclusive lease it was told to prepare for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusiveRefusal {
    /// The lease the refusal is for.
    pub lease_id: String,
    /// Why, as a code from the bounded set.
    pub code: RefusalReason,
    /// Free text for a person. Decoding cuts a detail over
    /// [`limits::MAX_REFUSAL_DETAIL`] bytes to the longest prefix within the bound that
    /// ends on a character boundary and accepts the heartbeat, so a long detail never
    /// costs the beat's results, held list or the refusal code. [`ExclusiveRefusal::validate`]
    /// does not cut: it refuses a detail over the bound, a sender's own check. An empty
    /// detail is accepted without the text check; a non-empty one holds no control or
    /// format character. The scheduler never parses it.
    #[serde(default, deserialize_with = "cut_detail")]
    pub detail: String,
}

impl ExclusiveRefusal {
    /// Checks the lease id and the detail.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.check(At::new("exclusive_refused", refusal::INVALID_HEARTBEAT))
    }

    fn check(&self, at: At) -> Result<(), ValidationError> {
        check::ident(at.field("exclusive_refused.lease_id"), &self.lease_id)?;
        if self.detail.is_empty() {
            return Ok(());
        }
        check::text(
            at.field("exclusive_refused.detail"),
            &self.detail,
            limits::MAX_REFUSAL_DETAIL,
        )
    }
}

/// The periodic message, agent to server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The host's id.
    pub host_id: String,
    /// The session epoch from [`crate::Welcome`].
    pub epoch: u64,
    /// The session secret from [`crate::Welcome::session`], the one issued at the hello
    /// that began this epoch. The server refuses a heartbeat without it, or with another,
    /// as `session-mismatch` and changes nothing; the pool's join token is not proof of
    /// a session. A heartbeat body with no `session` decodes as an empty text so the rest
    /// of it can be read, and the server refuses it; [`Heartbeat::validate`] accepts an
    /// empty text for that reason and holds a present one to
    /// [`limits::MAX_SESSION_LEN`] and `[A-Za-z0-9._-]`.
    #[serde(default)]
    pub session: String,
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
    /// The lease id of the exclusive lease the host has finished preparing for; the
    /// scheduler mints the lease token only on seeing it. Not sent together with
    /// [`Heartbeat::exclusive_refused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_ready: Option<String>,
    /// The host's refusal of an exclusive lease. Not sent together with
    /// [`Heartbeat::exclusive_ready`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive_refused: Option<ExclusiveRefusal>,
}

/// The server's verdict on a [`RunResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ack {
    /// The result was recorded.
    Accepted,
    /// The result was recorded earlier.
    Duplicate,
    /// The lease has moved on, or the server holds no record of the run (it forgot it at a
    /// restart, while the lease can still hold the host); the result is dropped either
    /// way. The two are not told apart.
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
    /// The lease id of an exclusive lease granted to this host: the agent is to prepare
    /// for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive: Option<String>,
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
            (Outcome::Failed | Outcome::FailedToStart, Some(0)) => {
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

/// Every field but `session`, which prints as `".."`: the secret never reaches a log or a
/// panic message.
impl fmt::Debug for Heartbeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heartbeat")
            .field("host_id", &self.host_id)
            .field("epoch", &self.epoch)
            .field("session", &"..")
            .field("phase", &self.phase)
            .field("held", &self.held)
            .field("results", &self.results)
            .field("taint", &self.taint)
            .field("exclusive_ready", &self.exclusive_ready)
            .field("exclusive_refused", &self.exclusive_refused)
            .finish()
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
        if !self.session.is_empty() {
            check::session(at.field("session"), &self.session)?;
        }
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
        if let Some(taint) = &self.taint {
            taint.check(at)?;
        }
        self.check_exclusive(at)
    }

    /// The lease id of a ready or refused report, and that the two are not both sent.
    fn check_exclusive(&self, at: At) -> Result<(), ValidationError> {
        check::opt_ident(at.field("exclusive_ready"), self.exclusive_ready.as_ref())?;
        if let Some(refused) = &self.exclusive_refused {
            refused.check(at)?;
            if self.exclusive_ready.is_some() {
                return Err(at.field("exclusive_refused").fail(Rule::Inconsistent));
            }
        }
        Ok(())
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

/// One command of a reply that failed validation, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusedCommand {
    /// Where the command sits in [`HeartbeatReply::commands`].
    pub index: usize,
    /// The rule it broke; [`ValidationError::code`] is the refusal code for it.
    pub error: ValidationError,
}

/// What [`HeartbeatReply::validate`] found, command by command.
///
/// **The agent acts on every command in `commands`, and on every `Stop` and `Cancel`
/// among them even when a `Start` of the same reply is in `refused`.** One bad command
/// never costs the commands beside it: a stop is retained intent the server resends only
/// until the run leaves the held list, and a reply with a start the agent cannot accept
/// (an over-long image, or one cap a newer server has raised) must still stop the run
/// the stop names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyCheck<'a> {
    /// The commands that passed, in the order the reply lists them. An unknown command
    /// passes and is in here; the agent skips it.
    pub commands: Vec<&'a Command>,
    /// The commands that did not pass: refused one by one, never acted on.
    pub refused: Vec<RefusedCommand>,
    /// Whether the acknowledgements are acceptable (at most [`limits::MAX_RESULTS`], each
    /// with a good run id, none twice). When this is an error the agent ignores
    /// `result_acks` and still acts on `commands`.
    pub acks: Result<(), ValidationError>,
    /// Whether the lease id of `exclusive` is acceptable. When this is an error the agent
    /// does not prepare and still acts on `commands`.
    pub exclusive: Result<(), ValidationError>,
}

impl HeartbeatReply {
    /// Checks the reply the way the agent must before it acts on it.
    ///
    /// Every command is checked on its own ([`Command::validate`]); a start that names a
    /// run id or a GPU index an earlier accepted start of the reply already names is
    /// refused too, and so is every command at an index of [`limits::MAX_COMMANDS`] or
    /// more (the first [`limits::MAX_COMMANDS`] are still read, so a `Stop` early in a
    /// reply from a server with a higher cap is not lost). See [`ReplyCheck`] for what
    /// the agent does with the answer.
    pub fn validate(&self) -> ReplyCheck<'_> {
        let at = At::new("reply", refusal::INVALID_REPLY);
        let mut taken = Taken::default();
        let mut commands = Vec::new();
        let mut refused = Vec::new();
        for (index, command) in self.commands.iter().enumerate() {
            let checked = if index < limits::MAX_COMMANDS {
                taken.admit(at.field("commands"), command)
            } else {
                Err(at.field("commands").fail(Rule::TooMany))
            };
            match checked {
                Ok(()) => commands.push(command),
                Err(error) => refused.push(RefusedCommand { index, error }),
            }
        }
        ReplyCheck {
            commands,
            refused,
            acks: self.check_acks(at),
            exclusive: check::opt_ident(at.field("exclusive"), self.exclusive.as_ref()),
        }
    }

    fn check_acks(&self, at: At) -> Result<(), ValidationError> {
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
        )
    }
}

/// The run ids and GPU indices the accepted starts of a reply name.
#[derive(Default)]
struct Taken<'a> {
    runs: BTreeSet<&'a str>,
    gpus: BTreeSet<u32>,
}

impl<'a> Taken<'a> {
    /// Checks one command and, when it is a start, records what it names.
    fn admit(&mut self, at: At, command: &'a Command) -> Result<(), ValidationError> {
        command.validate()?;
        let Command::Start(spec) = command else {
            return Ok(());
        };
        let gpu_taken = spec.gpu_indices.iter().any(|gpu| self.gpus.contains(gpu));
        if gpu_taken || self.runs.contains(spec.run_id.as_str()) {
            return Err(at.fail(Rule::Duplicate));
        }
        self.runs.insert(spec.run_id.as_str());
        self.gpus.extend(spec.gpu_indices.iter().copied());
        Ok(())
    }
}

/// Decodes a refusal detail, cutting it to the longest prefix within
/// [`limits::MAX_REFUSAL_DETAIL`] bytes that ends on a character boundary, so a long
/// detail is shortened instead of failing the heartbeat.
fn cut_detail<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let mut text = String::deserialize(deserializer)?;
    if text.len() > limits::MAX_REFUSAL_DETAIL {
        let mut end = limits::MAX_REFUSAL_DETAIL;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Ok(text)
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
