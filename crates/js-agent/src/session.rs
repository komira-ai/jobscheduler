//! The session with the server: say hello, heartbeat, say hello again when the server
//! lost the session.
//!
//! [`Session`] is a state machine over a [`Transport`]. It does one exchange per
//! [`Session::tick`] and answers with a [`Step`]: how long to wait before the next tick,
//! or that the server's answer ends the agent. It never sleeps and reads no clock, so the
//! loop around it ([`crate::drive`]) owns time and a test owns the loop.
//!
//! # The states
//!
//! *Unregistered* sends a hello. A welcome makes the session *registered* with the
//! epoch, the heartbeat interval and the session secret the server chose. *Registered*
//! sends a heartbeat that names the epoch and carries the secret, once per interval.
//!
//! # What an answer does
//!
//! - 200: the work went through. A welcome or a heartbeat reply that does not decode or
//!   validate is treated as a failed exchange, not trusted.
//! - Fatal: 401, every 4xx but 408, 409 and 429, and every 403 but `session-mismatch` and
//!   `pool-full`. The same message will be refused again however long the agent waits,
//!   so the session ends with the status and the server's code ([`Fatal`]) and the
//!   program exits 2. These are the refusals that name something only the operator can
//!   change: the token (401), the pool (`unknown-pool`), the shape the pool accepts
//!   (`shape-not-accepted`), the credential the host id was admitted with (`host-bound`),
//!   the pool's static host list (`host-listed`), and a message the server cannot read or
//!   serve (`invalid-hello`, `invalid-heartbeat`, `unsupported-protocol`,
//!   `body-too-large`). The operator fixes the cause and restarts the agent.
//! - 409 to a heartbeat (`stale-epoch`, `unknown-host`, `host-lost`), and 403
//!   `session-mismatch` to one: the server no longer holds this session, as after its
//!   restart. The agent says hello again at once, but only when its last heartbeat was
//!   accepted: a heartbeat refused straight after a hello backs off like any failure, so
//!   a server that welcomes and then refuses is not asked in a loop.
//! - 403 `pool-full` to a hello, 409 to a hello (`host-live`: the old session of this
//!   host id is still within its grace), 408, 429, 5xx, anything unexpected, and a server
//!   that cannot be reached: wait ([`Backoff`]: doubling, capped, jittered, at least the
//!   `Retry-After` the server gave, up to a bound) and try the same message again. A pool
//!   is full until a host leaves it, so a full pool is a condition that passes, and the
//!   agent joins when it does.
//!
//! The run of failures ends with an accepted heartbeat, and with a welcome, so the first
//! heartbeat that fails after a long outage of the hello waits the shortest time and not
//! longer than the grace the welcome set. The one exception is a welcome after a session
//! that was lost before one heartbeat of it was accepted: that run goes on, so a server
//! that welcomes and at once refuses is asked less and less often.
//!
//! The session secret is held in memory, goes into the body of every heartbeat and into
//! nothing else: it is in no log line, no [`Note`] and no `Debug` output.

use std::fmt;
use std::time::Duration;

use js_agent_wire::refusal;
use js_agent_wire::{
    Heartbeat, HeartbeatReply, Hello, Phase, ProtocolRange, RefusalReason, Refused,
    ValidationError, Welcome,
};
use serde_json::Value;

use crate::backoff::{Backoff, Jitter};
use crate::exclusive::{Fields, Hold, PREPARE_TIMEOUT, Quiesce};
use crate::lifecycle::Clock;
use crate::transport::{Answer, Transport};

/// The route of the hello.
pub const HELLO_PATH: &str = "/v1/agent/hello";

/// The route of the heartbeat.
pub const HEARTBEAT_PATH: &str = "/v1/agent/heartbeat";

/// The code shown when the server's refusal has none the agent can print.
const UNREADABLE_CODE: &str = "unreadable";

/// The secret of a session. It has no `Debug` and no `Display`, so no log line or panic
/// message can print it.
struct Secret(String);

/// A session the server granted.
struct Live {
    epoch: u64,
    secret: Secret,
    interval: Duration,
}

/// The server's answer ends the agent: it will not have this host, and the answer will
/// not change by asking again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fatal {
    /// The message refused: `hello` or `heartbeat`.
    pub what: &'static str,
    /// The HTTP status.
    pub status: u16,
    /// The server's code, such as `host-bound`, or `unreadable`.
    pub code: String,
    /// The majors the server speaks, when it said so (`unsupported-protocol`).
    pub supported: Option<ProtocolRange>,
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let majors = self
            .supported
            .map(|range| format!("; it speaks majors {} to {}", range.min, range.max))
            .unwrap_or_default();
        write!(
            f,
            "the server refused the {} with status {} ({}{majors}); not retrying",
            self.what, self.status, self.code
        )
    }
}

/// Why an exchange did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The server could not be reached, or went away before it answered.
    Unreachable(String),
    /// The server answered with this status and code.
    Status {
        /// The HTTP status.
        status: u16,
        /// The server's code, or `unreadable`.
        code: String,
    },
    /// The answer, or the message to send, failed its checks; this is the wire code.
    Unusable(&'static str),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "{why}"),
            Self::Status { status, code } => write!(f, "status {status} ({code})"),
            Self::Unusable(code) => write!(f, "unusable message ({code})"),
        }
    }
}

/// Something the operator may want to read in the log. None of them holds a token or a
/// session secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The server welcomed the host.
    Registered {
        /// The session's epoch.
        epoch: u64,
        /// How often the host heartbeats.
        interval: Duration,
    },
    /// The server no longer holds the session; the agent says hello again.
    Lost {
        /// The server's code.
        code: String,
    },
    /// An exchange failed and will be tried again.
    Retrying {
        /// `hello` or `heartbeat`.
        what: &'static str,
        /// Why it failed.
        reason: Reason,
    },
    /// A heartbeat answer carried commands, which this agent does not act on yet.
    Ignored {
        /// How many.
        commands: usize,
    },
    /// The server granted a whole-node lease; the agent prepares the host for it.
    Preparing {
        /// The lease.
        lease: String,
    },
    /// The agent refused the lease it was preparing for.
    Refused {
        /// The code sent.
        code: RefusalReason,
    },
    /// The lease ended; the agent restores the host.
    Restoring,
    /// The host announced that it is draining.
    Draining {
        /// Whether the server acknowledged it.
        told: bool,
    },
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registered { epoch, interval } => write!(
                f,
                "registered, epoch {epoch}, heartbeat every {}ms",
                interval.as_millis()
            ),
            Self::Lost { code } => write!(f, "the server lost the session ({code})"),
            Self::Retrying { what, reason } => write!(f, "{what} failed: {reason}"),
            Self::Ignored { commands } => {
                write!(f, "ignoring {commands} command(s): not handled yet")
            }
            Self::Preparing { lease } => write!(f, "preparing the host for lease {lease}"),
            Self::Refused { code } => {
                write!(f, "refusing the whole-node lease: {}", String::from(*code))
            }
            Self::Restoring => write!(f, "the whole-node lease ended: restoring the host"),
            Self::Draining { told: true } => write!(f, "draining: the server was told"),
            Self::Draining { told: false } => {
                write!(f, "draining: the server could not be told")
            }
        }
    }
}

/// What one [`Session::tick`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Wait this long, then tick again.
    Wait {
        /// The wait.
        after: Duration,
        /// What happened, when it is worth a log line.
        note: Option<Note>,
    },
    /// The agent ends.
    Fatal(Fatal),
}

/// How an answer's status and code read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The message went through.
    Done,
    /// The server and the agent disagree about the session.
    Conflict,
    /// Try again later.
    Retry,
    /// Do not try again.
    Fatal,
}

fn verdict(status: u16, code: &str) -> Verdict {
    match (status, code) {
        (200, _) => Verdict::Done,
        (403, refusal::SESSION_MISMATCH) | (409, _) => Verdict::Conflict,
        (403, refusal::POOL_FULL) | (408 | 429, _) => Verdict::Retry,
        (400..=499, _) => Verdict::Fatal,
        _ => Verdict::Retry,
    }
}

/// The `error` code of a refusal body, held to the wire's rules for a code; the server's
/// text is never repeated as it came.
fn code_of(body: &Value) -> String {
    let refused = Refused {
        code: body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        supported: None,
    };
    match refused.validate() {
        Ok(()) => refused.code,
        Err(_) => UNREADABLE_CODE.to_owned(),
    }
}

/// The majors a refusal body names under `supported` (the server sends them with
/// `unsupported-protocol`), when they are a sane range and `code` is a plain code.
fn supported_of(body: &Value, code: &str) -> Option<ProtocolRange> {
    let supported = body
        .get("supported")
        .and_then(|range| serde_json::from_value::<ProtocolRange>(range.clone()).ok());
    let refused = Refused {
        code: code.to_owned(),
        supported,
    };
    refused.validate().ok().and(refused.supported)
}

/// The welcome in an answer body, checked.
fn welcome(body: &Value) -> Result<Welcome, &'static str> {
    serde_json::from_value::<Welcome>(body.clone())
        .map_err(|_| refusal::INVALID_WELCOME)
        .and_then(|welcome| {
            welcome
                .validate()
                .map(|()| welcome)
                .map_err(|err| err.code())
        })
}

/// What a heartbeat answer says, checked as a whole: the number of commands (accepted or
/// refused), and the whole-node lease it names (`Some(None)` when it names none, `None`
/// when the lease id it names is not acceptable and the agent changes nothing).
fn reply_of(body: &Value) -> Result<(usize, Option<Option<String>>), &'static str> {
    serde_json::from_value::<HeartbeatReply>(body.clone())
        .map_err(|_| refusal::INVALID_REPLY)
        .map(|reply| {
            let checked = reply.validate();
            let commands = checked.commands.len() + checked.refused.len();
            let lease = checked.exclusive.is_ok().then(|| reply.exclusive.clone());
            (commands, lease)
        })
}

/// The agent's side of one host's session.
pub struct Session<'a> {
    transport: &'a mut dyn Transport,
    jitter: &'a mut dyn Jitter,
    hello: Hello,
    live: Option<Live>,
    phase: Phase,
    backoff: Backoff,
    proven: bool,
    /// The last session was lost before one heartbeat of it was accepted. A welcome then
    /// does not end the run of failures: a server that welcomes and at once refuses is
    /// backed off from more and more, not forgiven by each welcome.
    lost_young: bool,
    hold: Option<Hold<'a>>,
}

impl<'a> Session<'a> {
    /// A session that has not said hello. The hello is checked before every send, and a
    /// hello that breaks a rule of the wire is never sent: the tick reports it as an
    /// unusable message and retries, so a caller that wants to refuse such a hello for good
    /// calls [`Hello::validate`] first.
    pub fn new(hello: Hello, transport: &'a mut dyn Transport, jitter: &'a mut dyn Jitter) -> Self {
        Self {
            transport,
            jitter,
            phase: hello.phase,
            hello,
            live: None,
            backoff: Backoff::default(),
            proven: false,
            lost_young: false,
            hold: None,
        }
    }

    /// The same session following the whole-node lease the server grants (see the
    /// `exclusive` module): it asks `host` whether it is quiet, counts
    /// the `prepare_timeout` on `clock`, and says what it finds in the heartbeat. Without
    /// it the session ignores `exclusive`.
    #[must_use]
    pub fn with_quiesce(
        self,
        host: &'a mut dyn Quiesce,
        clock: &'a dyn Clock,
        prepare_timeout: Duration,
    ) -> Self {
        Self {
            hold: Some(Hold::new(host, clock, prepare_timeout)),
            ..self
        }
    }

    /// [`Session::with_quiesce`] with the pool's default `prepare_timeout`.
    #[must_use]
    pub fn with_default_quiesce(self, host: &'a mut dyn Quiesce, clock: &'a dyn Clock) -> Self {
        self.with_quiesce(host, clock, PREPARE_TIMEOUT)
    }

    /// Does one exchange: a hello when there is no session, a heartbeat when there is.
    pub fn tick(&mut self) -> Step {
        match self.live.take() {
            None => self.hello_step(),
            Some(live) => self.beat_step(live),
        }
    }

    /// Announces that the host is draining, once: one heartbeat with the phase
    /// `draining`, answered or not. Nothing is sent, and `None` returned, when there is
    /// no session. The lease is reported as already decided (see [`Hold::peek`]): the
    /// host is not asked, and the lease's stage does not move.
    pub fn drain(&mut self) -> Option<Note> {
        self.phase = Phase::Draining;
        let lease = self.hold.as_ref().map(Hold::peek).unwrap_or_default();
        self.live.take().map(|live| {
            let message = self.heartbeat_message(&live, lease);
            let told = self.post(HEARTBEAT_PATH, message);
            Note::Draining {
                told: told.is_ok_and(|answer| answer.status == 200),
            }
        })
    }

    fn hello_message(&self) -> Result<Value, ValidationError> {
        self.hello
            .validate()
            .map(|()| serde_json::to_value(&self.hello).unwrap_or_default())
    }

    fn heartbeat_message(&self, live: &Live, lease: Fields) -> Result<Value, ValidationError> {
        // The lease puts the agent in a phase only when nothing else does.
        let phase = match (self.phase, lease.phase) {
            (Phase::Serving, Some(phase)) => phase,
            (phase, _) => phase,
        };
        let beat = Heartbeat {
            host_id: self.hello.host_id.clone(),
            epoch: live.epoch,
            session: live.secret.0.clone(),
            phase,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
            exclusive_ready: lease.ready,
            exclusive_refused: lease.refused,
        };
        beat.validate()
            .map(|()| serde_json::to_value(&beat).unwrap_or_default())
    }

    /// Sends a message that passed its checks (or the reason it did not).
    fn post(
        &mut self,
        path: &str,
        message: Result<Value, ValidationError>,
    ) -> Result<Answer, Reason> {
        message
            .map_err(|err| Reason::Unusable(err.code()))
            .and_then(|body| {
                self.transport
                    .post(path, &body)
                    .map_err(Reason::Unreachable)
            })
    }

    fn hello_step(&mut self) -> Step {
        let message = self.hello_message();
        match self.post(HELLO_PATH, message) {
            Err(reason) => self.retry("hello", reason, None),
            Ok(answer) => self.after_hello(&answer),
        }
    }

    fn after_hello(&mut self, answer: &Answer) -> Step {
        let code = code_of(&answer.body);
        match verdict(answer.status, &code) {
            Verdict::Done => match welcome(&answer.body) {
                Ok(welcome) => self.registered(&welcome),
                Err(wire) => self.retry("hello", Reason::Unusable(wire), None),
            },
            other => self.refused("hello", other, answer, code),
        }
    }

    fn registered(&mut self, welcome: &Welcome) -> Step {
        let interval = Duration::from_millis(welcome.heartbeat_interval_ms);
        self.live = Some(Live {
            epoch: welcome.epoch,
            secret: Secret(welcome.session.clone()),
            interval,
        });
        self.proven = false;
        if !self.lost_young {
            self.backoff.reset();
        }
        Step::Wait {
            after: interval,
            note: Some(Note::Registered {
                epoch: welcome.epoch,
                interval,
            }),
        }
    }

    fn beat_step(&mut self, live: Live) -> Step {
        let lease = self.hold.as_mut().map(Hold::fields).unwrap_or_default();
        let message = self.heartbeat_message(&live, lease);
        match self.post(HEARTBEAT_PATH, message) {
            Err(reason) => {
                self.live = Some(live);
                self.retry("heartbeat", reason, None)
            }
            Ok(answer) => self.after_beat(live, &answer),
        }
    }

    fn after_beat(&mut self, live: Live, answer: &Answer) -> Step {
        let code = code_of(&answer.body);
        let verdict = verdict(answer.status, &code);
        let interval = live.interval;
        let kept = !matches!(verdict, Verdict::Conflict);
        self.live = kept.then_some(live);
        match verdict {
            Verdict::Done => self.beat_done(&answer.body, interval),
            Verdict::Conflict => self.rehello(code),
            other => self.refused("heartbeat", other, answer, code),
        }
    }

    fn beat_done(&mut self, body: &Value, interval: Duration) -> Step {
        match reply_of(body) {
            Ok((commands, lease)) => {
                self.backoff.reset();
                self.proven = true;
                self.lost_young = false;
                let followed = lease.and_then(|lease| {
                    let hold = self.hold.as_mut()?;
                    hold.follow(lease.as_deref())
                });
                let refused = self.hold.as_mut().and_then(Hold::take_note);
                Step::Wait {
                    after: interval,
                    note: followed
                        .or(refused)
                        .or_else(|| (commands > 0).then_some(Note::Ignored { commands })),
                }
            }
            Err(wire) => self.retry("heartbeat", Reason::Unusable(wire), None),
        }
    }

    /// The session is gone on the server: say hello again now when the heartbeat before
    /// was accepted, after a backoff when it was not.
    fn rehello(&mut self, code: String) -> Step {
        let proven = self.proven;
        self.proven = false;
        self.lost_young = !proven;
        let after = match proven {
            true => Duration::ZERO,
            false => self.backoff.next(&mut *self.jitter, None),
        };
        Step::Wait {
            after,
            note: Some(Note::Lost { code }),
        }
    }

    /// An answer that is not a success and not the loss of a session.
    fn refused(
        &mut self,
        what: &'static str,
        verdict: Verdict,
        answer: &Answer,
        code: String,
    ) -> Step {
        match verdict {
            Verdict::Fatal => Step::Fatal(Fatal {
                what,
                status: answer.status,
                supported: supported_of(&answer.body, &code),
                code,
            }),
            _ => {
                let reason = Reason::Status {
                    status: answer.status,
                    code,
                };
                self.retry(what, reason, answer.retry_after)
            }
        }
    }

    fn retry(&mut self, what: &'static str, reason: Reason, retry_after: Option<Duration>) -> Step {
        Step::Wait {
            after: self.backoff.next(&mut *self.jitter, retry_after),
            note: Some(Note::Retrying { what, reason }),
        }
    }
}
