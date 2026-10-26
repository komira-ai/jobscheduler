use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::Duration;

use js_provider::{Call, Observed, RefusalCode, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ResourceClass};

use crate::{Subject, Verdict, standard_request};

/// One rule of the delegated contract. Each has a name in the suite's output and a
/// defect in `js-provider-fake` that fails it alone.
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
    /// A class the provider does not serve is refused before the provider is called.
    RefusesUnknownClass,
    /// A second start of the same lease books nothing more.
    StartIsIdempotent,
    /// No host is booked by two leases, and a full pool refuses.
    NeverDoubleBooks,
}

impl Check {
    /// Every check, in the order [`crate::run`] runs them.
    pub const ALL: [Check; 8] = [
        Check::StartThenPresent,
        Check::StopThenAbsent,
        Check::UnknownDoesNotFree,
        Check::RefusesNonPositiveWallTime,
        Check::RefusesReservedLabel,
        Check::RefusesUnknownClass,
        Check::StartIsIdempotent,
        Check::NeverDoubleBooks,
    ];

    /// Runs this check on a fresh subject.
    pub(crate) fn run<S: Subject>(&self, subject: S) -> Verdict {
        match self {
            Self::StartThenPresent => start_then_present(subject),
            Self::StopThenAbsent => stop_then_absent(subject),
            Self::UnknownDoesNotFree => unknown_does_not_free(subject),
            Self::RefusesNonPositiveWallTime => refuses_wall_time(subject),
            Self::RefusesReservedLabel => refuses_reserved_label(subject),
            Self::RefusesUnknownClass => refuses_unknown_class(subject),
            Self::StartIsIdempotent => start_is_idempotent(subject),
            Self::NeverDoubleBooks => never_double_books(subject),
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

fn refuses_unknown_class<S: Subject>(mut s: S) -> Verdict {
    let mut request = standard_request();
    request.class = js_types::ClassId::new("no-such-class").expect("constant id is valid");
    refusal(&mut s, request, RefusalCode::UnknownClass)
}

/// A request that breaks a start-contract rule never becomes a spec, so no provider
/// call is possible; the state must also show nothing booked.
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
