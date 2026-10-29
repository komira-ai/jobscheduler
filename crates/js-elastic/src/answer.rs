//! What a provider answers: the machines it holds, and the outcome of an acquire and of a
//! release. The calls block and each is bound by the time limit of the client under it; a
//! call that runs out of time answers [`Acquired::Unknown`], [`Released::Unknown`] or an
//! error, never a guess.

use std::time::Duration;

use crate::ids::HostRef;

/// The longest status text kept.
const STATUS_KEPT: usize = 40;

/// A machine's status. A value a provider adds later reads as [`HostStatus::Unknown`];
/// one unknown value never fails a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    /// Starting.
    Booting,
    /// Up.
    Active,
    /// Up and failing its health check.
    Unhealthy,
    /// Gone.
    Terminated,
    /// Being terminated.
    Terminating,
    /// Reclaimed by the provider. Whether it still bills is the provider's to say.
    Preempted,
    /// A status this crate does not know, printable ASCII only and shortened. Treat it as
    /// a machine that exists and may bill.
    Unknown(String),
}

impl HostStatus {
    /// The status named by `text`, in the lower case words providers use.
    pub fn parse(text: &str) -> Self {
        match text {
            "booting" => Self::Booting,
            "active" => Self::Active,
            "unhealthy" => Self::Unhealthy,
            "terminated" => Self::Terminated,
            "terminating" => Self::Terminating,
            "preempted" => Self::Preempted,
            other => Self::Unknown(
                other
                    .chars()
                    .filter(char::is_ascii_graphic)
                    .take(STATUS_KEPT)
                    .collect(),
            ),
        }
    }
}

/// A machine this scheduler launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The provider's id for it.
    pub id: HostRef,
    /// The acquire id it carries, empty when the provider's marks do not hold one.
    pub acquire: String,
    /// The name, when it has one.
    pub name: Option<String>,
    /// The id of the offer it was launched from.
    pub offer: String,
    /// The zone it runs in, when the provider has zones.
    pub zone: Option<String>,
    /// The status.
    pub status: HostStatus,
}

/// What the markers of a machine say about whose it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Every marker is whole and carries this scheduler's id.
    Ours,
    /// Some mark of this scheduler's, not all: it may be ours, renamed or retagged. It
    /// counts toward the cost guard, blocks launches and is never terminated.
    Suspect,
    /// No mark of this scheduler's.
    Foreign,
}

/// The plain constants a refusal is one of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalCode {
    /// The account's limit was reached.
    Quota,
    /// The provider has no capacity for that offer now. The caller falls through to its
    /// next option or provider.
    Capacity,
    /// The request does not fit: an offer, zone or key the account does not know, a
    /// label that cannot become a mark, or a payload the provider cannot carry. A
    /// configuration error, not a state of the world.
    Shape,
    /// The request was not made: the key is refused, or a guard of the provider stopped
    /// it.
    Denied,
}

/// Why a request was refused, for logs and operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The provider is not enabled.
    Disabled,
    /// The provider is in dry-run mode: the request was built and logged, not sent.
    DryRun,
    /// The offer is not one this provider lists: an unknown id, or no zone.
    NotAllowed,
    /// The host name does not carry the prefix of this scheduler, so the machine could
    /// not be recognised as ours after it launched.
    Name,
    /// The payload is larger than the provider carries.
    PayloadTooLarge {
        /// The payload's size in bytes.
        len: usize,
        /// The most the provider carries.
        limit: usize,
    },
    /// A label or the pool name cannot become a mark; the provider's words for why.
    Label(String),
    /// Machines with some of this scheduler's marks, not all, exist: launching more
    /// could exceed the cap unseen. An operator clears them.
    HalfMarked {
        /// How many there are.
        count: usize,
    },
    /// The machines held, and launches not yet resolved, reach the cap.
    AtMaxMachines {
        /// The count.
        held: usize,
        /// The limit.
        max_machines: usize,
    },
    /// The provider's API refused with this code, in the provider's spelling.
    Api(String),
}

/// A refusal: the constant, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The constant.
    pub code: RefusalCode,
    /// The reason.
    pub reason: Reason,
}

/// The release of a machine that was not the one wanted, with the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    /// The machine.
    pub id: HostRef,
    /// What releasing it read back.
    pub released: Released,
}

/// The answer to an acquire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// A machine of this acquire exists: launched now, or found launched earlier.
    /// Launched, not ready: the machine's own agent says when it is.
    Accepted {
        /// The machine.
        held: Held,
        /// Further machines one launch answer named, which were released at once.
        extras: Vec<Extra>,
    },
    /// Nothing was launched, for a reason that is an answer.
    Refused(Refusal),
    /// No answer: the call timed out or failed in a way that may have launched a
    /// machine. Nothing is retried here; list before asking again.
    Unknown,
    /// Nothing was asked of the provider: a launch left too recently, or an earlier
    /// launch of this acquire is unresolved. Try again after `wait`.
    Deferred {
        /// How long until a launch is allowed.
        wait: Duration,
    },
}

/// The answer to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// The provider said, for a read by id, that the machine does not exist. This is the
    /// only evidence of "gone": a terminate answer, an empty list or a `terminated`
    /// status are not.
    Gone,
    /// The machine is being terminated, or terminate was accepted; the status read.
    Terminating(HostStatus),
    /// The machine is not entirely ours; nothing was sent.
    Refused(Claim),
    /// No answer: nothing observed shows the machine gone or going. Callers must treat
    /// this as "retry", never as "gone".
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that every documented status has a variant and any other text is an
    /// unknown status, cut to printable ASCII and to 40 characters.
    #[test]
    fn every_documented_status_has_a_variant_and_any_other_is_unknown() {
        let documented = [
            ("booting", HostStatus::Booting),
            ("active", HostStatus::Active),
            ("unhealthy", HostStatus::Unhealthy),
            ("terminated", HostStatus::Terminated),
            ("terminating", HostStatus::Terminating),
            ("preempted", HostStatus::Preempted),
        ];
        for (text, status) in documented {
            assert_eq!(HostStatus::parse(text), status);
        }
        assert_eq!(
            HostStatus::parse("Active"),
            HostStatus::Unknown("Active".to_owned())
        );
        assert_eq!(
            HostStatus::parse("a b\u{7}c"),
            HostStatus::Unknown("abc".to_owned())
        );
        assert_eq!(
            HostStatus::parse(&"s".repeat(100)),
            HostStatus::Unknown("s".repeat(40))
        );
    }

    /// Asserts that the answers compare by value, so a caller can match a refusal on
    /// its code and reason, and that a released machine carries what was read back.
    #[test]
    fn answers_compare_by_value() {
        let refusal = Refusal {
            code: RefusalCode::Shape,
            reason: Reason::PayloadTooLarge { len: 9, limit: 0 },
        };
        assert_eq!(
            Acquired::Refused(refusal.clone()),
            Acquired::Refused(Refusal {
                code: RefusalCode::Shape,
                reason: Reason::PayloadTooLarge { len: 9, limit: 0 },
            })
        );
        assert_ne!(refusal.reason, Reason::PayloadTooLarge { len: 9, limit: 1 });
        let extra = Extra {
            id: HostRef("i-2".to_owned()),
            released: Released::Terminating(HostStatus::Terminating),
        };
        assert_ne!(extra.released, Released::Refused(Claim::Suspect));
        assert_ne!(Released::Unknown, Released::Gone);
    }
}
