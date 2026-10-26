use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::Duration;

use js_provider::{Call, Observed, RefusalCode, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ResourceClass};

use crate::{Subject, Verdict, standard_request};

/// One rule of the delegated contract, with a name in the suite's output.
///
/// Six checks read a provider's behaviour, and each of those has a defect in
/// `js-provider-fake` that makes it fail. The four `Refuses...` checks call no provider:
/// they guard [`StartSpec::new`], which every provider shares, so only dropping a rule
/// from it can fail them. The rules themselves are unit-tested in `js-provider`
/// (`each_rule_has_its_code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Check {
    /// A started lease is observed Present on the host the state shows.
    StartThenPresent,
    /// A stopped lease is observed Absent and its host is free in the state.
    StopThenAbsent,
    /// An Unknown observation or stop leaves the booking in place.
    UnknownDoesNotFree,
    /// A zero wall time is refused before the provider is called.
    RefusesNonPositiveWallTime,
    /// A caller label under the reserved prefix is refused before the provider is called.
    RefusesReservedLabel,
    /// A caller label with a malformed key or value is refused before the provider is
    /// called.
    RefusesInvalidLabel,
    /// A class the provider does not serve is refused before the provider is called.
    RefusesUnknownClass,
    /// A second start of the same lease books nothing more.
    StartIsIdempotent,
    /// No host is booked by two leases, and a full pool refuses.
    NeverDoubleBooks,
    /// A start that answers Unknown is found by observing the lease id, whether or not
    /// it landed, and the retry books exactly one host (design 5.6).
    StartUnknownThenRetry,
    /// A start of a lease id that was stopped is refused with `LeaseEnded`, books
    /// nothing and leaves the lease Absent, whether the stop followed a start or came
    /// first.
    StartAfterStopIsRefused,
}

impl Check {
    /// Every check, in the order [`crate::run`] runs them.
    pub const ALL: [Check; 11] = [
        Check::StartThenPresent,
        Check::StopThenAbsent,
        Check::UnknownDoesNotFree,
        Check::RefusesNonPositiveWallTime,
        Check::RefusesReservedLabel,
        Check::RefusesInvalidLabel,
        Check::RefusesUnknownClass,
        Check::StartIsIdempotent,
        Check::NeverDoubleBooks,
        Check::StartUnknownThenRetry,
        Check::StartAfterStopIsRefused,
    ];

    /// The start-contract refusal this check guards, or `None` for a check that reads a
    /// provider's behaviour.
    pub fn guards(&self) -> Option<RefusalCode> {
        match self {
            Self::RefusesNonPositiveWallTime => Some(RefusalCode::WallTimeNotPositive),
            Self::RefusesReservedLabel => Some(RefusalCode::ReservedLabel),
            Self::RefusesInvalidLabel => Some(RefusalCode::InvalidLabel),
            Self::RefusesUnknownClass => Some(RefusalCode::UnknownClass),
            Self::StartThenPresent
            | Self::StopThenAbsent
            | Self::UnknownDoesNotFree
            | Self::StartIsIdempotent
            | Self::NeverDoubleBooks
            | Self::StartUnknownThenRetry
            | Self::StartAfterStopIsRefused => None,
        }
    }

    /// Runs this check on a fresh subject.
    pub(crate) fn run<S: Subject>(&self, subject: S) -> Verdict {
        match self {
            Self::StartThenPresent => start_then_present(subject),
            Self::StopThenAbsent => stop_then_absent(subject),
            Self::UnknownDoesNotFree => unknown_does_not_free(subject),
            Self::RefusesNonPositiveWallTime => refuses_wall_time(subject),
            Self::RefusesReservedLabel => refuses_reserved_label(subject),
            Self::RefusesInvalidLabel => refuses_invalid_label(subject),
            Self::RefusesUnknownClass => refuses_unknown_class(subject),
            Self::StartIsIdempotent => start_is_idempotent(subject),
            Self::NeverDoubleBooks => never_double_books(subject),
            Self::StartUnknownThenRetry => start_unknown_then_retry(subject),
            Self::StartAfterStopIsRefused => start_after_stop_is_refused(subject),
        }
    }
}

/// Passes when what was observed equals what the contract says.
fn judge<T: Debug + PartialEq>(observed: T, expected: T) -> Verdict {
    match observed == expected {
        true => Verdict::Passed,
        false => Verdict::Failed(format!("expected {expected:?}, observed {observed:?}")),
    }
}

fn lease(sequence: u64) -> LeaseId {
    LeaseId::new(1, sequence)
}

fn host(name: &str) -> HostId {
    HostId::new(name).expect("constant id is valid")
}

fn held(pairs: &[(&str, u64)]) -> BTreeMap<HostId, LeaseId> {
    pairs
        .iter()
        .map(|(name, sequence)| (host(name), lease(*sequence)))
        .collect()
}

/// The standard request for lease `sequence`, checked against the shipped class.
fn spec(sequence: u64) -> StartSpec {
    let known = [ResourceClass::gpu_host()];
    StartSpec::new(lease(sequence), standard_request(), &known)
        .expect("the standard request meets the start contract")
}

fn start_then_present<S: Subject>(mut s: S) -> Verdict {
    let ack = s.provider().start(spec(1));
    let seen = s.provider().observe(&lease(1));
    let observed = (ack, seen, s.booked());
    let expected = (
        StartAck::Started { host: host("h1") },
        Observed::Present { host: host("h1") },
        held(&[("h1", 1)]),
    );
    judge(observed, expected)
}

fn stop_then_absent<S: Subject>(mut s: S) -> Verdict {
    s.provider().start(spec(1));
    let ack = s.provider().stop(&lease(1));
    let seen = s.provider().observe(&lease(1));
    let observed = (ack, seen, s.booked());
    let expected = (
        StopAck::Stopped {
            read_back: Observed::Absent,
        },
        Observed::Absent,
        held(&[]),
    );
    judge(observed, expected)
}

fn unknown_does_not_free<S: Subject>(mut s: S) -> Verdict {
    s.provider().start(spec(1));
    let can_observe = s.script_unknown(Call::Observe, false);
    let can_stop = s.script_unknown(Call::Stop, false);
    if ![can_observe, can_stop].into_iter().all(|ok| ok) {
        return Verdict::NotApplicable;
    }
    let seen = s.provider().observe(&lease(1));
    let after_observe = s.booked();
    let stopped = s.provider().stop(&lease(1));
    let observed = (seen, after_observe, stopped, s.booked());
    let expected = (
        Observed::Unknown,
        held(&[("h1", 1)]),
        StopAck::Unknown,
        held(&[("h1", 1)]),
    );
    judge(observed, expected)
}

fn refuses_wall_time<S: Subject>(mut s: S) -> Verdict {
    let mut request = standard_request();
    request.max_wall = Duration::ZERO;
    refusal(&mut s, request, RefusalCode::WallTimeNotPositive)
}

fn refuses_reserved_label<S: Subject>(mut s: S) -> Verdict {
    let mut request = standard_request();
    request
        .labels
        .insert("jobscheduler.dev/tenant".to_string(), "someone".to_string());
    refusal(&mut s, request, RefusalCode::ReservedLabel)
}

fn refuses_invalid_label<S: Subject>(mut s: S) -> Verdict {
    let mut request = standard_request();
    request
        .labels
        .insert("tenant".to_string(), "NOT VALID".to_string());
    refusal(&mut s, request, RefusalCode::InvalidLabel)
}

fn refuses_unknown_class<S: Subject>(mut s: S) -> Verdict {
    let mut request = standard_request();
    request.class = js_types::ClassId::new("no-such-class").expect("constant id is valid");
    refusal(&mut s, request, RefusalCode::UnknownClass)
}

/// A request that breaks a start-contract rule never becomes a spec, so no provider
/// call is possible. This guards `StartSpec::new` against the provider's class list;
/// the booked state is empty because nothing was started, so no provider defect can
/// fail it.
fn refusal<S: Subject>(s: &mut S, request: js_types::Request, code: RefusalCode) -> Verdict {
    let known = s.provider().classes();
    let made = StartSpec::new(lease(1), request, &known).map(|_| ());
    judge((made, s.booked()), (Err(code), held(&[])))
}

fn start_is_idempotent<S: Subject>(mut s: S) -> Verdict {
    let first = s.provider().start(spec(1));
    let second = s.provider().start(spec(1));
    let observed = (first, second, s.booked());
    let started = StartAck::Started { host: host("h1") };
    judge(observed, (started.clone(), started, held(&[("h1", 1)])))
}

fn never_double_books<S: Subject>(mut s: S) -> Verdict {
    s.provider().start(spec(1));
    s.provider().start(spec(2));
    let third = s.provider().start(spec(3));
    let third_refused = matches!(third, StartAck::Refused(_));
    let listed = s
        .provider()
        .capacity()
        .into_iter()
        .filter_map(|capacity| capacity.leased_to);
    let observed = (s.booked(), listed.collect::<Vec<_>>(), third_refused);
    judge(
        observed,
        (
            held(&[("h1", 1), ("h2", 2)]),
            vec![lease(1), lease(2)],
            true,
        ),
    )
}

/// Lease 1 starts, answers Unknown and landed; lease 2 starts, answers Unknown and did
/// not land. In both legs, observing the lease id must tell the two apart, and the
/// retry must end with exactly one host booked for the lease.
fn start_unknown_then_retry<S: Subject>(mut s: S) -> Verdict {
    if !s.script_unknown(Call::Start, true) {
        return Verdict::NotApplicable;
    }
    let landed = unknown_start_leg(&mut s, 1);
    s.script_unknown(Call::Start, false);
    let lost = unknown_start_leg(&mut s, 2);
    let started = |name: &str| StartAck::Started { host: host(name) };
    let present = Observed::Present { host: host("h1") };
    let expected = (
        (
            StartAck::Unknown,
            present,
            held(&[("h1", 1)]),
            started("h1"),
            held(&[("h1", 1)]),
        ),
        (
            StartAck::Unknown,
            Observed::Absent,
            held(&[("h1", 1)]),
            started("h2"),
            held(&[("h1", 1), ("h2", 2)]),
        ),
    );
    judge((landed, lost), expected)
}

/// One start that was scripted Unknown, then the lookup by lease id, then the retry.
/// Returns the first answer, the lookup, the state after it, the retry's answer and
/// the state after the retry.
fn unknown_start_leg<S: Subject>(
    s: &mut S,
    sequence: u64,
) -> (
    StartAck,
    Observed,
    BTreeMap<HostId, LeaseId>,
    StartAck,
    BTreeMap<HostId, LeaseId>,
) {
    let first = s.provider().start(spec(sequence));
    let seen = s.provider().observe(&lease(sequence));
    let after_lookup = s.booked();
    let retry = s.provider().start(spec(sequence));
    (first, seen, after_lookup, retry, s.booked())
}

/// Lease 1 starts, stops, and a start of it arrives again; lease 2 is stopped before
/// any start of it and then a start of it arrives. Both late starts must be refused
/// with `LeaseEnded`, both leases must read Absent, and the provider's table must hold
/// nothing: neither the duplicate nor the reordered start may book a host.
fn start_after_stop_is_refused<S: Subject>(mut s: S) -> Verdict {
    s.provider().start(spec(1));
    s.provider().stop(&lease(1));
    let after_stop = s.provider().start(spec(1));
    s.provider().stop(&lease(2));
    let before_start = s.provider().start(spec(2));
    let seen = (
        s.provider().observe(&lease(1)),
        s.provider().observe(&lease(2)),
    );
    let ended = StartAck::Refused(RefusalCode::LeaseEnded);
    let observed = (after_stop, before_start, seen, s.booked());
    let expected = (
        ended.clone(),
        ended,
        (Observed::Absent, Observed::Absent),
        held(&[]),
    );
    judge(observed, expected)
}
