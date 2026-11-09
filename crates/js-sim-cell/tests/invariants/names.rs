//! The names of the invariants and of the ways an item ends, as failure lines spell them.

use super::*;

/// Asserts each invariant's name and when it is checked (the five of a job, the five of an
/// elastic pool's cloud, the two of the usage history and the one of a cordon too), and each
/// way an item ends' name, as the failure lines spell them. Catches a renamed invariant
/// (a scenario's recorded failure would stop matching) and two answers spelled alike.
#[test]
fn names_are_stable_and_distinct() {
    let invariants: Vec<(&str, Checked)> = Invariant::ALL
        .iter()
        .map(|i| (i.name(), i.checked()))
        .collect();
    assert_eq!(
        invariants,
        [
            ("no-double-booking", Checked::EveryStep),
            ("at-most-one-launch", Checked::EveryStep),
            ("one-terminal-answer", Checked::EveryStep),
            ("settles-when-quiet", Checked::AtEnd),
            ("bookings-match-agents", Checked::AtEnd),
            ("no-lease-on-a-lost-host", Checked::EveryStep),
            ("no-result-lost", Checked::AtEnd),
            ("whole-host-is-exclusive", Checked::EveryStep),
            ("held-means-ready", Checked::EveryStep),
            ("drain-kills-nothing", Checked::EveryStep),
            ("marked-hosts-serve-no-ordinary-start", Checked::EveryStep),
            ("one-allocation-per-job", Checked::EveryStep),
            ("lost-is-terminal", Checked::EveryStep),
            ("freed-only-when-confirmed", Checked::EveryStep),
            ("no-start-without-allocation", Checked::EveryStep),
            ("result-kept-until-acknowledged", Checked::EveryStep),
            ("instances-within-limits", Checked::EveryStep),
            ("only-own-machines-released", Checked::EveryStep),
            ("no-acquire-while-plan-only", Checked::EveryStep),
            ("throttle-honoured", Checked::EveryStep),
            ("launched-machines-settle", Checked::AtEnd),
            ("usage-row-per-ended-allocation", Checked::AtEnd),
            ("usage-totals-match-terminal-jobs", Checked::AtEnd),
            ("cordon-never-kills-running-job", Checked::EveryStep),
        ]
    );
    let ends: Vec<&str> = HowEnded::ALL.iter().map(|h| h.name()).collect();
    assert_eq!(
        ends,
        [
            "succeeded",
            "failed",
            "cancelled",
            "lost",
            "forgotten",
            "gave-up"
        ]
    );
}
