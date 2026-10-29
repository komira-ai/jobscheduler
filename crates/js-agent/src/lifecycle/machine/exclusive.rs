//! The agent's side of a whole-node lease (whole-node design, sections 5 and 8): the
//! gate on `Start`, the quiet check of `Preparing` and the clean-up check of `Restoring`.
//!
//! The server names the lease in the reply (`exclusive`) for as long as it holds the host
//! whole. [`Lifecycle::apply`] follows it: from the reply that names a lease until the
//! first that does not, the agent starts a run for that lease only, and only once
//! [`Lifecycle::prepare`] has found the host quiet. The session asks for that check on
//! each heartbeat until it passes, and for [`Lifecycle::restore`] once the lease is gone.
//!
//! Quiet means: the agent holds no run (a result the server has not acknowledged is a run
//! it holds), the host is not tainted (a container it could not remove taints it), and the post-job
//! checks, run once more with nothing live, find nothing. A finding taints the host as
//! after any job. The VM check of the design (5.1) is not here: no VM backend exists.

use std::mem;

use js_agent_wire::refusal;

use super::Lifecycle;
use super::Refusal;
use super::job::check_taint_of;
use crate::lifecycle::{CheckFinding, Event, Finding, Finished};
use js_agent_wire::JobSpec;

/// What [`Lifecycle::prepare`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quiet {
    /// A run, a result or a container is still held, or no lease is followed (yet). Ask
    /// again.
    Busy,
    /// The host is quiet and clean; the lease may start runs.
    Ready,
    /// The host is tainted, or a check found something or could not run (and tainted it).
    /// The host is not handed over.
    Unsafe {
        /// What a person may read; at most [`crate::lifecycle::CHECK_DETAIL_MAX`] bytes.
        detail: String,
    },
    /// The readiness hook of the operator refused the host (see
    /// [`crate::ReadinessHook`]); the lifecycle itself never answers this.
    Refused {
        /// The typed reason.
        code: js_agent_wire::RefusalReason,
        /// What a person may read; at most `limits::MAX_REFUSAL_DETAIL` bytes.
        detail: String,
    },
}

/// What [`Lifecycle::restore`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// A run, a result or a container is still held; ask again.
    Busy,
    /// The checks ran. A finding tainted the host, which the taint report carries.
    Done,
}

/// The run id the checks are given for the checks of a lease that ended.
const RESTORE_RUN: &str = "restore";

/// The lease the host is held for, and whether it was found quiet.
pub(super) struct Hold {
    lease: String,
    ready: bool,
    /// Ready because a run of the lease was taken over at start, not because the host was
    /// found quiet: the consumer is already using it.
    adopted: bool,
}

impl Lifecycle {
    /// The lease the host is held whole for, from the last reply that said so.
    pub fn exclusive(&self) -> Option<&str> {
        self.exclusive.as_ref().map(|hold| hold.lease.as_str())
    }

    /// Whether the lease the host is held for was ready from the start because a run of it
    /// was taken over after a restart. The host is in use then, and nothing that measures an
    /// idle host (the readiness hook) may run on it.
    pub fn adopted(&self) -> bool {
        self.exclusive.as_ref().is_some_and(|hold| hold.adopted)
    }

    /// Follows the `exclusive` of a reply: a lease that is not the one held starts a new
    /// hold, not yet ready; none ends it. The first reply after a restart that names a lease
    /// is the exception: a lease that a run taken over at start belongs to is ready at
    /// once, since the agent only starts a run of the lease after it found the host quiet,
    /// and the consumer is using the host. The adopted leases are consumed by that reply,
    /// not by an earlier one that names none: a reply and a grant may cross, and the grant
    /// that follows a reply without `exclusive` is still the restart's. (With no adopted
    /// run a restart prepares again on a quiet host, which costs one quiet check.)
    pub(super) fn follow(&mut self, lease: Option<&str>) {
        let adopted = if lease.is_some() {
            mem::take(&mut self.adopted_leases)
        } else {
            Vec::new()
        };
        let same = self
            .exclusive()
            .zip(lease)
            .is_some_and(|(held, new)| held == new);
        if !same {
            self.exclusive = lease.map(|lease| {
                let taken_over = adopted.iter().any(|adopted| adopted == lease);
                Hold {
                    lease: lease.to_owned(),
                    ready: taken_over,
                    adopted: taken_over,
                }
            });
        }
    }

    /// Refuses a start the whole-node lease does not allow: any run while the host is not
    /// ready, and a run of another lease after.
    pub(super) fn admit_exclusive(&self, spec: &JobSpec) -> Result<(), Refusal> {
        let no = |why: &str| (refusal::NO_CAPACITY, why.to_owned());
        match &self.exclusive {
            None => Ok(()),
            Some(hold) if hold.lease != spec.lease_id => {
                Err(no("this host is held whole for another lease"))
            }
            Some(hold) if !hold.ready => Err(no("this host is not ready for its whole-node lease")),
            Some(_) => Ok(()),
        }
    }

    /// Whether the agent holds no run and no result. (A container it could not remove has
    /// tainted the host, which [`Lifecycle::prepare`] answers before it asks this, and which
    /// [`Lifecycle::restore`] does not wait on: waiting would not remove it.)
    fn is_quiet(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Runs the post-job checks once with nothing live. A finding taints the host and is
    /// returned with its text.
    fn recheck(&mut self, run_id: &str, events: &mut Vec<Event>) -> Result<(), String> {
        let verdict = self.checks.run(&Finished { run_id, live: &[] });
        let Err(found) = verdict else { return Ok(()) };
        let message = match &found {
            CheckFinding::Residue { .. } => found.detail().to_owned(),
            CheckFinding::Unchecked { .. } => {
                format!("the check could not run: {}", found.detail())
            }
        };
        if let Err(why) = self.taint.seal(check_taint_of(None, &found), events) {
            events.push(Event::Finding(Finding {
                run_id: None,
                code: super::code::TAINT_WRITE_FAILED,
                message: why.to_string(),
            }));
        }
        events.push(Event::Finding(Finding {
            run_id: None,
            code: found.code(),
            message: message.clone(),
        }));
        Err(message)
    }

    /// Whether the host is quiet for `lease`, which must be the lease the last reply
    /// named: the session asks this on each heartbeat of `Preparing` until the answer is
    /// not [`Quiet::Busy`]. A question about any other lease, or while no lease is held, is
    /// [`Quiet::Busy`] (a lagging question about a lease that has ended must not be
    /// answered for the one that replaced it). A ready answer is kept: the lease may then
    /// start runs, and asking again costs nothing.
    pub fn prepare(&mut self, lease: &str, events: &mut Vec<Event>) -> Quiet {
        if self.exclusive() != Some(lease) {
            return Quiet::Busy;
        }
        let lease = lease.to_owned();
        if self.exclusive.as_ref().is_some_and(|hold| hold.ready) {
            return Quiet::Ready;
        }
        if let Some(why) = self.taint.reason() {
            return Quiet::Unsafe {
                detail: format!("this host is tainted ({why})"),
            };
        }
        if !self.is_quiet() {
            return Quiet::Busy;
        }
        match self.recheck(&lease, events) {
            Ok(()) => {
                self.exclusive.iter_mut().for_each(|hold| hold.ready = true);
                Quiet::Ready
            }
            Err(detail) => Quiet::Unsafe { detail },
        }
    }

    /// Runs the clean-up check of a lease that ended, once the runs it left are gone.
    pub fn restore(&mut self, events: &mut Vec<Event>) -> Restored {
        match self.is_quiet() {
            false => Restored::Busy,
            true => {
                let _ = self.recheck(RESTORE_RUN, events);
                Restored::Done
            }
        }
    }
}
