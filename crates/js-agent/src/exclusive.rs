//! The session's half of a whole-node lease (whole-node design, sections 5 and 8): what
//! the heartbeat says while the host is prepared for the lease and while it is restored
//! after it.
//!
//! The server grants a lease by naming it in a heartbeat reply (`exclusive`) and goes on
//! naming it until the lease ends. The session follows that one field:
//!
//! 1. **Preparing.** The first heartbeat after the reply carries the phase `preparing`.
//!    The agent starts no run for any other lease from the reply on (the lifecycle's rule).
//!    On each later heartbeat the session asks the host ([`Quiesce::prepare`]) whether it
//!    is quiet: no run held, the post-job checks clean once more, the host not tainted.
//!    While it is not, the phase stays `preparing` and nothing is reported.
//! 2. **Ready.** The first heartbeat after the host is quiet carries `exclusive_ready` and
//!    the phase `serving`, and every heartbeat after it does too, until a reply stops
//!    naming the lease (the report is resent, as any report is until the server moves on).
//! 3. **Refused.** A finding on the way, or a tainted host, is `exclusive_refused` with
//!    the code `host-unsafe` and the finding as its detail. A run the agent still holds
//!    when `prepare_timeout` has passed since the reply is `exclusive_refused` with
//!    `not-quiet`: the deadline is the agent's own, counted on the clock it is given, so a
//!    server that cannot be reached does not leave the agent waiting for a verdict. The
//!    refusal is resent, with the phase `preparing`, until the reply stops naming the lease.
//! 4. **Restoring.** The first reply that does not name the lease makes the next heartbeat
//!    carry the phase `restoring`; on each later one the session asks the host
//!    ([`Quiesce::restore`]) until its runs are gone and the checks have run once more,
//!    and then returns to `serving`. A finding taints the host, which the taint report
//!    carries; it does not keep the agent from `serving`, as after any job.
//!
//! After a restart the agent has no memory of a ready lease. When it took over runs that
//! belong to the lease the first reply naming a lease names, the lifecycle answers [`Quiesce::prepare`]
//! with ready at once (those runs could only have been started once the host was found
//! quiet), so the next heartbeat reports ready and no refusal is made for a lease in use.
//! Taken-over runs of any other lease prove nothing and the host is prepared as usual,
//! as is a restart with no taken-over run (it prepares again on a quiet host).
//!
//! A phase other than `serving` that the agent is in for its own reasons (a drain) is not
//! overridden. Without a [`Quiesce`] the session ignores `exclusive`.
//!
//! The VM check of the design (5.1) is not here: no VM backend exists. The readiness hook
//! (5.5) is a [`Quiesce`] wrapper (`readiness::Hooked`): it answers [`Quiet::Refused`]
//! when the hook refuses, and the session refuses the lease with that reason.

use std::mem;
use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, Phase, RefusalReason, limits};

use crate::lifecycle::{Clock, Quiet, Restored};
use crate::session::Note;

/// How long the agent waits for a run it did not know of to end, counted from the reply
/// that granted the lease: the pool's default of `prepare_timeout`.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(600);

/// What the detail says when the agent gave up on a run that was still held.
const NOT_QUIET_DETAIL: &str = "a run was still held when prepare_timeout passed";

/// What the detail says when the finding's own text is not fit for the wire.
const UNSAFE_DETAIL: &str = "the host is not safe to hand over";

/// The host the session asks. The lifecycle answers (see
/// [`Lifecycle::prepare`](crate::lifecycle::Lifecycle::prepare)); a test answers from a script.
/// Every call may block as long as the lifecycle's checks do, so a session that runs jobs
/// asks through its worker, never from the heartbeat loop itself.
pub trait Quiesce {
    /// Whether the host is quiet for the lease the last reply named.
    fn prepare(&mut self) -> Quiet;

    /// Whether the host has been cleaned up after the lease that ended.
    fn restore(&mut self) -> Restored;
}

/// Where the agent is in a lease.
enum Stage {
    Idle,
    Preparing {
        lease: String,
        deadline_ms: u64,
        announced: bool,
    },
    Ready {
        lease: String,
    },
    Refused(ExclusiveRefusal),
    Restoring {
        announced: bool,
    },
}

/// What a heartbeat says about the lease.
#[derive(Default)]
pub(crate) struct Fields {
    /// The phase the lease puts the agent in, when it does.
    pub phase: Option<Phase>,
    /// The `exclusive_ready` lease.
    pub ready: Option<String>,
    /// The `exclusive_refused` report.
    pub refused: Option<ExclusiveRefusal>,
}

/// The lease the session follows.
pub(crate) struct Hold<'a> {
    host: &'a mut dyn Quiesce,
    clock: &'a dyn Clock,
    timeout: Duration,
    stage: Stage,
    note: Option<Note>,
}

impl<'a> Hold<'a> {
    pub(crate) fn new(host: &'a mut dyn Quiesce, clock: &'a dyn Clock, timeout: Duration) -> Self {
        Self {
            host,
            clock,
            timeout,
            stage: Stage::Idle,
            note: None,
        }
    }

    /// What was noted since the last call: a refusal the agent made.
    pub(crate) fn take_note(&mut self) -> Option<Note> {
        self.note.take()
    }

    /// Follows the `exclusive` of an accepted reply.
    pub(crate) fn follow(&mut self, lease: Option<&str>) -> Option<Note> {
        let held = match &self.stage {
            Stage::Preparing { lease, .. } | Stage::Ready { lease } => Some(lease.as_str()),
            Stage::Refused(refused) => Some(refused.lease_id.as_str()),
            Stage::Idle | Stage::Restoring { .. } => None,
        };
        match (held, lease) {
            (Some(held), Some(new)) if held == new => None,
            (_, Some(new)) => {
                let wait = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
                self.stage = Stage::Preparing {
                    lease: new.to_owned(),
                    deadline_ms: self.clock.now_ms().saturating_add(wait),
                    announced: false,
                };
                Some(Note::Preparing {
                    lease: new.to_owned(),
                })
            }
            (Some(_), None) => {
                self.stage = Stage::Restoring { announced: false };
                Some(Note::Restoring)
            }
            (None, None) => None,
        }
    }

    /// What a heartbeat says about a verdict already reached, without asking the host or
    /// moving the stage: the ready report or the refusal, nothing for a stage still
    /// waiting on the host. The drain sends this, so a lost last beat consumes nothing.
    pub(crate) fn peek(&self) -> Fields {
        match &self.stage {
            Stage::Ready { lease } => Fields {
                phase: Some(Phase::Serving),
                ready: Some(lease.clone()),
                refused: None,
            },
            Stage::Refused(refused) => Fields {
                phase: Some(Phase::Preparing),
                ready: None,
                refused: Some(refused.clone()),
            },
            Stage::Idle | Stage::Preparing { .. } | Stage::Restoring { .. } => Fields::default(),
        }
    }

    /// What the next heartbeat says, after asking the host what the stage needs.
    pub(crate) fn fields(&mut self) -> Fields {
        match mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Idle => Fields::default(),
            Stage::Preparing {
                lease,
                deadline_ms,
                announced: false,
            } => {
                self.stage = Stage::Preparing {
                    lease,
                    deadline_ms,
                    announced: true,
                };
                phase(Phase::Preparing)
            }
            Stage::Preparing {
                lease, deadline_ms, ..
            } => self.preparing(lease, deadline_ms),
            Stage::Ready { lease } => self.ready(lease),
            Stage::Refused(refused) => self.refused(refused),
            Stage::Restoring { announced: false } => {
                self.stage = Stage::Restoring { announced: true };
                phase(Phase::Restoring)
            }
            Stage::Restoring { announced: true } => match self.host.restore() {
                Restored::Busy => {
                    self.stage = Stage::Restoring { announced: true };
                    phase(Phase::Restoring)
                }
                Restored::Done => Fields::default(),
            },
        }
    }

    fn preparing(&mut self, lease: String, deadline_ms: u64) -> Fields {
        match self.host.prepare() {
            Quiet::Ready => self.ready(lease),
            Quiet::Unsafe { detail } => self.refuse(&lease, RefusalReason::HostUnsafe, &detail),
            Quiet::Refused { code, detail } => self.refuse(&lease, code, &detail),
            Quiet::Busy if self.clock.now_ms() >= deadline_ms => {
                self.refuse(&lease, RefusalReason::NotQuiet, NOT_QUIET_DETAIL)
            }
            Quiet::Busy => {
                self.stage = Stage::Preparing {
                    lease,
                    deadline_ms,
                    announced: true,
                };
                phase(Phase::Preparing)
            }
        }
    }

    fn ready(&mut self, lease: String) -> Fields {
        self.stage = Stage::Ready {
            lease: lease.clone(),
        };
        Fields {
            phase: Some(Phase::Serving),
            ready: Some(lease),
            refused: None,
        }
    }

    fn refused(&mut self, refused: ExclusiveRefusal) -> Fields {
        self.stage = Stage::Refused(refused.clone());
        Fields {
            phase: Some(Phase::Preparing),
            ready: None,
            refused: Some(refused),
        }
    }

    /// Refuses the lease. `detail` is cut to the wire's bound at a character
    /// boundary; one the wire would still refuse (a control or format character in what a
    /// check printed) is replaced by a fixed text, so the heartbeat can always be sent.
    fn refuse(&mut self, lease: &str, code: RefusalReason, detail: &str) -> Fields {
        let end = (0..=limits::MAX_REFUSAL_DETAIL.min(detail.len()))
            .rev()
            .find(|&at| detail.is_char_boundary(at))
            .unwrap_or(0);
        let mut refused = ExclusiveRefusal {
            lease_id: lease.to_owned(),
            code,
            detail: detail[..end].to_owned(),
        };
        if refused.validate().is_err() {
            refused.detail = UNSAFE_DETAIL.to_owned();
        }
        self.note = Some(Note::Refused { code });
        self.refused(refused)
    }
}

fn phase(phase: Phase) -> Fields {
    Fields {
        phase: Some(phase),
        ..Fields::default()
    }
}
