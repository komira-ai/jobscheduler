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
//! - 401, 403 and every other 4xx but 408, 409 and 429: the server will not have this
//!   agent, and asking again changes nothing. The session ends with the status and the
//!   server's code ([`Fatal`]); the program exits 2.
//! - 409 to a heartbeat (`stale-epoch`, `unknown-host`, `host-lost`), and 403
//!   `session-mismatch` to one: the server no longer holds this session, as after its
//!   restart. The agent says hello again at once, but only when its last heartbeat was
//!   accepted: a heartbeat refused straight after a hello backs off like any failure, so
//!   a server that welcomes and then refuses is not asked in a loop.
//! - 409 to a hello (`host-live`: the old session of this host id is still within its
//!   grace), 408, 429, 5xx, anything unexpected, and a server that cannot be reached:
//!   wait ([`Backoff`]: doubling, capped, jittered, at least the `Retry-After` the server
//!   gave, up to a bound) and try the same message again.
//!
//! The session secret is held in memory, goes into the body of every heartbeat and into
//! nothing else: it is in no log line, no [`Note`] and no `Debug` output.

use std::fmt;
use std::time::Duration;

use js_agent_wire::refusal;
use js_agent_wire::{Heartbeat, HeartbeatReply, Hello, Phase, Refused, ValidationError, Welcome};
use serde_json::Value;

use crate::backoff::{Backoff, Jitter};
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
}

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the server refused the {} with status {} ({}); not retrying",
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
        (408 | 429, _) => Verdict::Retry,
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

/// The number of commands (accepted or refused) in a heartbeat answer, which is checked
/// as a whole.
fn reply_commands(body: &Value) -> Result<usize, &'static str> {
    serde_json::from_value::<HeartbeatReply>(body.clone())
        .map_err(|_| refusal::INVALID_REPLY)
        .map(|reply| {
            let checked = reply.validate();
            checked.commands.len() + checked.refused.len()
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
        }
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
    /// no session.
    pub fn drain(&mut self) -> Option<Note> {
        self.phase = Phase::Draining;
        self.live.take().map(|live| {
            let message = self.heartbeat_message(&live);
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

    fn heartbeat_message(&self, live: &Live) -> Result<Value, ValidationError> {
        let beat = Heartbeat {
            host_id: self.hello.host_id.clone(),
            epoch: live.epoch,
            session: live.secret.0.clone(),
            phase: self.phase,
            held: Vec::new(),
            results: Vec::new(),
            taint: None,
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
        Step::Wait {
            after: interval,
            note: Some(Note::Registered {
                epoch: welcome.epoch,
                interval,
            }),
        }
    }

    fn beat_step(&mut self, live: Live) -> Step {
        let message = self.heartbeat_message(&live);
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
        match reply_commands(body) {
            Ok(commands) => {
                self.backoff.reset();
                self.proven = true;
                Step::Wait {
                    after: interval,
                    note: (commands > 0).then_some(Note::Ignored { commands }),
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
