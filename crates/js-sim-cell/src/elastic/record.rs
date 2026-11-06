//! What the recording wrapper writes down of each call the server makes to an elastic
//! pool's cloud, and of the brake the cell reads after every tick: one ordered list of
//! [`CloudCall`]s, each at the virtual time of the step it happened in.
//!
//! The list keeps the arguments and answers the elastic fake's own call log
//! (`ElasticFake::calls`, a list of call kinds) does not: the offer, the zone and the
//! acquire id of an acquire and what it was answered, the machine a release named, and
//! the machines a list showed. An invariant that needs an argument or a time reads it
//! here; one about what the cloud holds reads the fake itself.

use std::time::Duration;

use js_elastic::{Acquired, RefusalCode, Released};
use js_types::sched::FarmTime;

/// One call the server made to the cloud, or one reading of the pool's brake, at the
/// virtual time of the step it happened in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudCall {
    /// The virtual time of the step.
    pub at: FarmTime,
    /// What happened.
    pub event: CloudEvent,
}

/// What one [`CloudCall`] was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloudEvent {
    /// The scaler asked the cloud for a machine.
    Acquire {
        /// The offer's id.
        offer: String,
        /// The offer's zone.
        zone: Option<String>,
        /// The acquire id the scaler chose (it names the machine).
        acquire: String,
        /// What the cloud answered.
        answer: AcquireAnswer,
    },
    /// The scaler asked the cloud to end a machine.
    Release {
        /// The machine, by the cloud's id for it.
        machine: String,
        /// What the cloud answered.
        answer: ReleaseAnswer,
    },
    /// The scaler listed the pool's machines: the cloud's ids of the machines the list
    /// showed, or `None` when the list failed.
    List {
        /// The machines listed.
        listed: Option<Vec<String>>,
    },
    /// The cell read the pool's brake after a tick and found it changed: `true` once it
    /// has tripped (the pool launches nothing), `false` once it no longer reads tripped.
    Brake {
        /// Whether the brake reads tripped.
        tripped: bool,
    },
    /// The operator asked for the pool's brake to be reset ([`crate::Cell::reset_brake`]).
    BrakeReset,
}

/// What the cloud answered an acquire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcquireAnswer {
    /// A machine was launched, or an earlier one returned: its id.
    Accepted {
        /// The cloud's id of the machine.
        machine: String,
    },
    /// Refused, with the refusal's code.
    Refused(RefusalCode),
    /// No answer: the call may have landed.
    Unknown,
    /// Nothing was asked of the cloud: try again after the wait (a 429 answers
    /// [`js_elastic_fake::THROTTLE_WAIT`]).
    Deferred(Duration),
}

impl AcquireAnswer {
    /// The answer as recorded: the machine's id, never the held machine's other fields.
    #[must_use]
    pub fn of(answer: &Acquired) -> Self {
        match answer {
            Acquired::Accepted { held, .. } => AcquireAnswer::Accepted {
                machine: held.id.as_str().to_owned(),
            },
            Acquired::Refused(refusal) => AcquireAnswer::Refused(refusal.code),
            Acquired::Unknown => AcquireAnswer::Unknown,
            Acquired::Deferred { wait } => AcquireAnswer::Deferred(*wait),
        }
    }
}

/// What the cloud answered a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseAnswer {
    /// The machine is gone.
    Gone,
    /// The machine is being ended.
    Terminating,
    /// The cloud refused to end it.
    Refused,
    /// No answer.
    Unknown,
}

impl ReleaseAnswer {
    /// The answer as recorded.
    #[must_use]
    pub fn of(answer: &Released) -> Self {
        match answer {
            Released::Gone => ReleaseAnswer::Gone,
            Released::Terminating(_) => ReleaseAnswer::Terminating,
            Released::Refused(_) => ReleaseAnswer::Refused,
            Released::Unknown => ReleaseAnswer::Unknown,
        }
    }

    /// Whether the cloud stopped the machine at this call: its bill stops here.
    #[must_use]
    pub fn ended(self) -> bool {
        matches!(self, ReleaseAnswer::Gone | ReleaseAnswer::Terminating)
    }
}

#[cfg(test)]
mod tests {
    use js_elastic::{Claim, Held, HostRef, HostStatus, Reason, Refusal};

    use super::*;

    /// Catches: an answer recorded as another (a refusal as a deferral, a lost call as a
    /// refusal), a deferral that loses its wait, and an accepted machine recorded by
    /// another field than the cloud's id of it.
    #[test]
    fn every_acquire_answer_is_recorded_as_it_was_given() {
        let held = Held {
            id: HostRef("fake-0001".to_owned()),
            acquire: "burst-1".to_owned(),
            name: Some("js-1-1".to_owned()),
            offer: "sim_1x_a100".to_owned(),
            zone: Some("zone-a".to_owned()),
            status: HostStatus::Booting,
        };
        let accepted = Acquired::Accepted {
            held,
            extras: Vec::new(),
        };
        let refused = Acquired::Refused(Refusal {
            code: RefusalCode::Quota,
            reason: Reason::Api("scripted".to_owned()),
        });
        let wait = Duration::from_secs(30);
        let cases = [
            (
                accepted,
                AcquireAnswer::Accepted {
                    machine: "fake-0001".to_owned(),
                },
            ),
            (refused, AcquireAnswer::Refused(RefusalCode::Quota)),
            (Acquired::Unknown, AcquireAnswer::Unknown),
            (Acquired::Deferred { wait }, AcquireAnswer::Deferred(wait)),
        ];
        for (answer, recorded) in cases {
            assert_eq!(AcquireAnswer::of(&answer), recorded);
        }
    }

    /// Catches: a release answer recorded as another, and a refused or unanswered release
    /// taken as the end of the machine's bill.
    #[test]
    fn every_release_answer_is_recorded_and_only_an_ending_one_ends_the_bill() {
        let cases = [
            (Released::Gone, ReleaseAnswer::Gone, true),
            (
                Released::Terminating(HostStatus::Terminating),
                ReleaseAnswer::Terminating,
                true,
            ),
            (
                Released::Refused(Claim::Foreign),
                ReleaseAnswer::Refused,
                false,
            ),
            (Released::Unknown, ReleaseAnswer::Unknown, false),
        ];
        for (answer, recorded, ended) in cases {
            assert_eq!(ReleaseAnswer::of(&answer), recorded);
            assert_eq!(recorded.ended(), ended);
        }
    }
}
