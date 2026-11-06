//! The agent's side of the exclusive (whole-host) handshake, which the conformance
//! suite's [`FakeAgent`] does not know: the agent process keeps this state beside the
//! fake and puts what it says into every heartbeat (design sections 5.4 and 6.4).
//!
//! A heartbeat reply that names a lease in `exclusive`, and passes the wire's check of
//! it, tells a serving or restoring agent to prepare for that lease: it goes
//! `preparing` and takes no new start. What each heartbeat says from then on is what the
//! node's [`Prepare`] switch says when the heartbeat is sent:
//!
//! | switch | the heartbeat |
//! |---|---|
//! | [`Prepare::Ready`] | `preparing`, until no run is held and the delay has passed since the agent was told; then `serving` with `exclusive_ready` naming exactly the lease it was told, in every heartbeat until a reply names no lease |
//! | [`Prepare::Refuse`] | `preparing` with `exclusive_refused` for the lease and the reason, in every heartbeat until a reply names no lease |
//! | [`Prepare::Silent`] | `preparing`, and nothing more, for as long as replies name the lease |
//! | [`Prepare::ReadyForAnotherLease`] | as [`Prepare::Ready`] with no delay, but naming another lease than the one it was told: a defective agent, for negative tests |
//!
//! A reply that names another lease while the agent prepares, is ready or has refused
//! changes nothing: the agent keeps the first. The first reply that names no lease (the
//! lease was released, refused or timed out) makes it `restoring`, and the next reply
//! makes it `serving` again. A reply whose `exclusive` fails the wire's check changes
//! nothing (the process counts the answer as invalid).
//!
//! The agent launches a start only while it is serving, restoring or ready: one that
//! arrives while it prepares, or after it refused, is not launched. A ready agent
//! launches every start it is sent, another lease's too, so that a server that books a
//! held host twice is caught by the invariants and not hidden by the agent.
//!
//! The state belongs to the agent process: a replacement starts serving.

use std::time::Duration;

use js_agent_wire::{ExclusiveRefusal, Phase, RefusalReason};
use js_types::sched::FarmTime;

#[cfg(doc)]
use js_conformance::agent::FakeAgent;

/// What an agent does when a heartbeat reply tells it to prepare for an exclusive lease.
/// A switch of the agent node ([`crate::AgentNode::set_prepare`]), read at each
/// heartbeat, so a change reaches a prepare already under way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepare {
    /// Reports ready once no run is held and `delay` has passed since it was told.
    Ready {
        /// How long the preparation takes, at least.
        delay: Duration,
    },
    /// Refuses the lease with this reason.
    Refuse(RefusalReason),
    /// Never answers: it stays preparing.
    Silent,
    /// A defect: reports ready, once no run is held, for a lease other than the one it
    /// was told (the told id with a `0` appended).
    ReadyForAnotherLease,
}

/// A correct agent that reports ready as soon as it holds no run.
impl Default for Prepare {
    fn default() -> Self {
        Prepare::Ready {
            delay: Duration::ZERO,
        }
    }
}

/// Where an agent process is in the handshake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Exclusive {
    /// Not told of any lease, or back from one.
    #[default]
    Serving,
    /// Told of `lease` at `told_at`, and not yet ready or refused.
    Preparing { lease: String, told_at: FarmTime },
    /// Reported ready, naming `reported`, and reports it again in every heartbeat.
    Ready { reported: String },
    /// Refused `lease` for `reason`, and refuses it again in every heartbeat.
    Refused {
        lease: String,
        reason: RefusalReason,
    },
    /// A reply named no lease after one was named: one heartbeat says `restoring`.
    Restoring,
}

/// What a heartbeat reports of the handshake: its `exclusive_ready` and its
/// `exclusive_refused`.
pub(crate) type Report = (Option<String>, Option<ExclusiveRefusal>);

impl Exclusive {
    /// A heartbeat reply whose `exclusive` passed the wire's check arrived, naming
    /// `named`, at `now`.
    pub(crate) fn told(&mut self, named: Option<&str>, now: FarmTime) {
        *self = match (std::mem::take(self), named) {
            (Exclusive::Serving | Exclusive::Restoring, Some(lease)) => Exclusive::Preparing {
                lease: lease.to_owned(),
                told_at: now,
            },
            (Exclusive::Serving | Exclusive::Restoring, None) => Exclusive::Serving,
            (kept, Some(_)) => kept,
            (_, None) => Exclusive::Restoring,
        };
    }

    /// What the heartbeat sent at `now` reports, after moving on to ready or refused when
    /// `prepare` says so; `quiet` is whether the process holds no run.
    pub(crate) fn report(&mut self, prepare: Prepare, quiet: bool, now: FarmTime) -> Report {
        let next = match (&*self, prepare) {
            (Exclusive::Preparing { lease, told_at }, Prepare::Ready { delay })
                if quiet & (now >= told_at.saturating_add(delay)) =>
            {
                Some(Exclusive::Ready {
                    reported: lease.clone(),
                })
            }
            (Exclusive::Preparing { lease, .. }, Prepare::ReadyForAnotherLease) if quiet => {
                Some(Exclusive::Ready {
                    reported: format!("{lease}0"),
                })
            }
            (Exclusive::Preparing { lease, .. }, Prepare::Refuse(reason)) => {
                Some(Exclusive::Refused {
                    lease: lease.clone(),
                    reason,
                })
            }
            _ => None,
        };
        if let Some(next) = next {
            *self = next;
        }
        match self {
            Exclusive::Ready { reported } => (Some(reported.clone()), None),
            Exclusive::Refused { lease, reason } => {
                let refusal = ExclusiveRefusal {
                    lease_id: lease.clone(),
                    code: *reason,
                    detail: String::new(),
                };
                (None, Some(refusal))
            }
            _ => (None, None),
        }
    }

    /// The phase the agent announces: `serving` while serving or ready, `preparing` while
    /// it prepares or has refused, `restoring` while it restores.
    pub(crate) fn phase(&self) -> Phase {
        match self {
            Exclusive::Serving | Exclusive::Ready { .. } => Phase::Serving,
            Exclusive::Preparing { .. } | Exclusive::Refused { .. } => Phase::Preparing,
            Exclusive::Restoring => Phase::Restoring,
        }
    }

    /// Whether the agent launches a start it is sent now.
    pub(crate) fn takes_starts(&self) -> bool {
        !matches!(
            self,
            Exclusive::Preparing { .. } | Exclusive::Refused { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lease every case is told of.
    const LEASE: &str = "1-1";

    fn ms(n: u64) -> FarmTime {
        FarmTime::from_millis(n)
    }

    /// An agent told of [`LEASE`] at 1 000 ms.
    fn told() -> Exclusive {
        let mut exclusive = Exclusive::default();
        exclusive.told(Some(LEASE), ms(1_000));
        exclusive
    }

    fn ready_after(delay: u64) -> Prepare {
        Prepare::Ready {
            delay: Duration::from_millis(delay),
        }
    }

    /// Asserts that an agent told of a lease goes `preparing` and takes no start; that it
    /// reports nothing while a run is held or before its delay has passed (the delay
    /// counted from when it was told); that it then reports `exclusive_ready` naming
    /// exactly the lease it was told, in `serving` and taking starts; and that it says so
    /// again in every later heartbeat. Catches an agent that reports before it is quiet
    /// or before its delay, one that names another lease, and one that reports once only
    /// (a lost heartbeat would strand the lease).
    #[test]
    fn an_agent_goes_preparing_and_reports_ready_for_the_named_lease_only() {
        let mut exclusive = told();
        assert_eq!(exclusive.phase(), Phase::Preparing);
        assert!(!exclusive.takes_starts());
        let prepare = ready_after(2_000);
        assert_eq!(exclusive.report(prepare, false, ms(9_000)), (None, None));
        assert_eq!(exclusive.report(prepare, true, ms(2_999)), (None, None));
        assert_eq!(exclusive.phase(), Phase::Preparing);
        let ready = (Some(LEASE.to_owned()), None);
        assert_eq!(exclusive.report(prepare, true, ms(3_000)), ready);
        assert_eq!(exclusive.phase(), Phase::Serving);
        assert!(exclusive.takes_starts());
        assert_eq!(exclusive.report(prepare, false, ms(4_000)), ready);
        assert_eq!(exclusive.report(Prepare::default(), true, ms(5_000)), ready);
    }

    /// Asserts that a reply naming a second lease changes nothing while the agent
    /// prepares, once it is ready, and once it has refused: it keeps preparing for, and
    /// reports, the first. Catches an agent that switches to the lease named last, which
    /// would report ready for a lease the server did not commit on it.
    #[test]
    fn an_agent_that_is_asked_for_another_lease_while_preparing_keeps_the_first() {
        let mut exclusive = told();
        exclusive.told(Some("1-2"), ms(2_000));
        let first = Exclusive::Preparing {
            lease: LEASE.to_owned(),
            told_at: ms(1_000),
        };
        assert_eq!(exclusive, first);
        let ready = (Some(LEASE.to_owned()), None);
        assert_eq!(exclusive.report(ready_after(0), true, ms(2_000)), ready);
        exclusive.told(Some("1-2"), ms(3_000));
        assert_eq!(exclusive.report(ready_after(0), true, ms(3_000)), ready);

        let mut refused = told();
        let not_quiet = Prepare::Refuse(RefusalReason::NotQuiet);
        let (_, first) = refused.report(not_quiet, true, ms(2_000));
        refused.told(Some("1-2"), ms(3_000));
        assert_eq!(refused.report(not_quiet, true, ms(3_000)), (None, first));
    }

    /// Asserts that a refusing agent sends `exclusive_refused` for the lease it was told
    /// with each of the five reason codes of the wire, spelled as the wire spells them,
    /// in `preparing` and taking no start, again in every later heartbeat, and never
    /// together with `exclusive_ready`. Catches a reason sent as another, and a refusal
    /// sent once only.
    #[test]
    fn a_refusing_agent_sends_each_reason_code_the_wire_has() {
        let codes = [
            (RefusalReason::NotQuiet, "not-quiet"),
            (RefusalReason::NoiseOverBound, "noise-over-bound"),
            (RefusalReason::SetupFailed, "setup-failed"),
            (RefusalReason::HostUnsafe, "host-unsafe"),
            (RefusalReason::Unsupported, "unsupported"),
        ];
        for (reason, code) in codes {
            let mut exclusive = told();
            let (ready, refused) = exclusive.report(Prepare::Refuse(reason), false, ms(1_000));
            assert_eq!(ready, None);
            let refused = refused.expect("a refusal");
            assert_eq!(refused.lease_id, LEASE);
            assert_eq!(refused.code, reason);
            assert!(refused.validate().is_ok());
            let wire = serde_json::to_value(&refused).expect("encodes");
            assert_eq!(wire["code"], code);
            assert_eq!(exclusive.phase(), Phase::Preparing);
            assert!(!exclusive.takes_starts());
            let again = exclusive.report(Prepare::default(), true, ms(9_000));
            assert_eq!(again, (None, Some(refused)));
        }
    }

    /// Asserts that a silent agent, quiet and long past any delay, never reports ready
    /// or refused and keeps saying `preparing`. Catches a silence switch that still
    /// answers, which would leave the server's prepare deadline untested.
    #[test]
    fn a_silent_agent_never_reports() {
        let mut exclusive = told();
        for at in [1_000, 60_000, 6_000_000] {
            let report = exclusive.report(Prepare::Silent, true, ms(at));
            assert_eq!(report, (None, None));
            assert_eq!(exclusive.phase(), Phase::Preparing);
        }
    }

    /// Asserts the way back: the first reply that names no lease after a ready agent's
    /// lease makes it `restoring` (taking starts, reporting nothing), the next makes it
    /// `serving`; a refused or still preparing agent restores the same way; a serving
    /// agent told of nothing stays serving; and a restoring agent told of a new lease
    /// prepares for it. Catches an agent that stays in a phase the server books nothing
    /// on, and one that goes on reporting a lease that ended.
    #[test]
    fn a_restoring_agent_returns_to_serving_after_the_lease_ends() {
        let mut exclusive = told();
        exclusive.report(Prepare::default(), true, ms(1_000));
        exclusive.told(None, ms(2_000));
        assert_eq!(exclusive, Exclusive::Restoring);
        assert_eq!(exclusive.phase(), Phase::Restoring);
        assert!(exclusive.takes_starts());
        assert_eq!(
            exclusive.report(Prepare::default(), true, ms(2_000)),
            (None, None)
        );
        exclusive.told(None, ms(3_000));
        assert_eq!(exclusive, Exclusive::Serving);
        assert_eq!(exclusive.phase(), Phase::Serving);
        exclusive.told(None, ms(4_000));
        assert_eq!(exclusive, Exclusive::Serving);

        let mut refused = told();
        refused.report(Prepare::Refuse(RefusalReason::HostUnsafe), true, ms(1_000));
        refused.told(None, ms(2_000));
        assert_eq!(refused, Exclusive::Restoring);
        let mut silent = told();
        silent.told(None, ms(2_000));
        assert_eq!(silent, Exclusive::Restoring);
        silent.told(Some("1-2"), ms(3_000));
        let next = Exclusive::Preparing {
            lease: "1-2".to_owned(),
            told_at: ms(3_000),
        };
        assert_eq!(silent, next);
    }

    /// Asserts that the defective switch reports ready, once quiet, for the told id with
    /// a `0` appended (another lease id the wire accepts), and nothing while a run is
    /// held; and that the default switch is ready with no delay. Catches a defect that
    /// names the right lease, which would make its negative test vacuous.
    #[test]
    fn the_defective_switch_reports_ready_for_another_lease() {
        let mut exclusive = told();
        let wrong = Prepare::ReadyForAnotherLease;
        assert_eq!(exclusive.report(wrong, false, ms(1_000)), (None, None));
        let report = exclusive.report(wrong, true, ms(1_000));
        assert_eq!(report, (Some("1-10".to_owned()), None));
        assert_eq!(Prepare::default(), ready_after(0));
    }
}
