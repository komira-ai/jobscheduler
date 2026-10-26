use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::Duration;

use js_provider::{Call, Observed, RefusalCode, StartAck, StartSpec, StopAck};
use js_types::{HostId, LeaseId, ResourceClass};

use crate::{Subject, Verdict, standard_hosts, standard_request};

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

/// The provider's booking table, as the subject reads it: host to the lease on it.
type Table = BTreeMap<HostId, LeaseId>;

fn none_held() -> Table {
    Table::new()
}

/// The host the table shows lease `sequence` on, if any. Where a provider places a
/// lease is its own rank (design 6.1), so the checks read the placement from the table
/// and never name a host.
fn home(table: &Table, sequence: u64) -> Option<HostId> {
    let wanted = lease(sequence);
    let found = table.iter().find(|(_, held)| **held == wanted);
    found.map(|(host, _)| host.clone())
}

/// The table holding exactly these leases on these hosts; a lease with no host is left
/// out, which is how an absent placement reads.
fn table_of(placed: &[(&Option<HostId>, u64)]) -> Table {
    let entries = placed
        .iter()
        .filter_map(|(host, sequence)| Some(((*host).clone()?, lease(*sequence))));
    entries.collect()
}

/// Whether `host` is one of the standard hosts and the standard request fits on it.
fn fits(host: &Option<HostId>) -> bool {
    let wanted = standard_request();
    let offered = standard_hosts();
    let found = offered.iter().find(|(id, _)| Some(id) == host.as_ref());
    found.is_some_and(|(_, shape)| shape.satisfies(&wanted.resources, &wanted.capabilities))
}

/// The host a start answered with, or `None` when it did not answer `Started`.
fn started_on(ack: &StartAck) -> Option<HostId> {
    match ack {
        StartAck::Started { host } => Some(host.clone()),
        StartAck::Refused(_) | StartAck::Unknown => None,
    }
}

/// What observing a lease must answer when the table shows it on `host`.
fn seen_on(host: &Option<HostId>) -> Observed {
    match host {
        Some(host) => Observed::Present { host: host.clone() },
        None => Observed::Absent,
    }
}

/// The standard request for lease `sequence`, checked against the shipped class.
fn spec(sequence: u64) -> StartSpec {
    let known = [ResourceClass::gpu_host()];
    StartSpec::new(lease(sequence), standard_request(), &known)
        .expect("the standard request meets the start contract")
}

/// One start answered `Started`, observed Present on the host the table shows, and
/// that host is one the request fits on. Which host is the provider's rank.
fn start_then_present<S: Subject>(mut s: S) -> Verdict {
    let ack = s.provider().start(spec(1));
    let seen = s.provider().observe(&lease(1));
    let table = s.booked();
    let placed = home(&table, 1);
    let observed = (started_on(&ack), seen, table, fits(&placed));
    let expected = (
        placed.clone(),
        seen_on(&placed),
        table_of(&[(&placed, 1)]),
        true,
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
        none_held(),
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
    let placed = home(&s.booked(), 1);
    let seen = s.provider().observe(&lease(1));
    let after_observe = s.booked();
    let stopped = s.provider().stop(&lease(1));
    let observed = (seen, after_observe, stopped, s.booked(), fits(&placed));
    let kept = table_of(&[(&placed, 1)]);
    let expected = (
        Observed::Unknown,
        kept.clone(),
        StopAck::Unknown,
        kept,
        true,
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
    judge((made, s.booked()), (Err(code), none_held()))
}

fn start_is_idempotent<S: Subject>(mut s: S) -> Verdict {
    let first = s.provider().start(spec(1));
    let second = s.provider().start(spec(1));
    let table = s.booked();
    let placed = home(&table, 1);
    let observed = (
        started_on(&first),
        started_on(&second),
        table,
        fits(&placed),
    );
    let expected = (
        placed.clone(),
        placed.clone(),
        table_of(&[(&placed, 1)]),
        true,
    );
    judge(observed, expected)
}

/// Two leases start and a third is refused. The table must show one lease per host, the
/// two leases on distinct hosts the request fits on, and the provider's own capacity
/// listing must agree; which hosts they are is the provider's rank.
fn never_double_books<S: Subject>(mut s: S) -> Verdict {
    s.provider().start(spec(1));
    s.provider().start(spec(2));
    let third = s.provider().start(spec(3));
    let third_refused = matches!(third, StartAck::Refused(_));
    let table = s.booked();
    let first = home(&table, 1);
    let second = home(&table, 2);
    let apart = [first != second, fits(&first), fits(&second)];
    let mut leases: Vec<_> = table.values().copied().collect();
    leases.sort();
    let listed = s
        .provider()
        .capacity()
        .into_iter()
        .filter_map(|capacity| capacity.leased_to);
    let mut listed: Vec<_> = listed.collect();
    listed.sort();
    let observed = (
        leases,
        apart.into_iter().all(|ok| ok),
        listed,
        third_refused,
    );
    let both = vec![lease(1), lease(2)];
    judge(observed, (both.clone(), true, both, true))
}

/// Lease 1 starts, answers Unknown and landed; lease 2 starts, answers Unknown and did
/// not land. In both legs, observing the lease id must tell the two apart, and the
/// retry must end with exactly one host booked for the lease. The hosts are read from
/// the table: lease 2 must end on a host other than lease 1's, and both must fit.
fn start_unknown_then_retry<S: Subject>(mut s: S) -> Verdict {
    if !s.script_unknown(Call::Start, true) {
        return Verdict::NotApplicable;
    }
    let landed = unknown_start_leg(&mut s, 1);
    s.script_unknown(Call::Start, false);
    let lost = unknown_start_leg(&mut s, 2);
    let first_home = home(&landed.after_lookup, 1);
    let second_home = home(&lost.after_retry, 2);
    let observed = (
        (
            landed.first,
            landed.seen,
            landed.after_lookup,
            started_on(&landed.retry),
            landed.after_retry,
            fits(&first_home),
        ),
        (
            lost.first,
            lost.seen,
            lost.after_lookup,
            started_on(&lost.retry),
            lost.after_retry,
            fits(&second_home),
        ),
        first_home != second_home,
    );
    let one = table_of(&[(&first_home, 1)]);
    let both = table_of(&[(&first_home, 1), (&second_home, 2)]);
    let expected = (
        (
            StartAck::Unknown,
            seen_on(&first_home),
            one.clone(),
            first_home.clone(),
            one.clone(),
            true,
        ),
        (
            StartAck::Unknown,
            Observed::Absent,
            one,
            second_home,
            both,
            true,
        ),
        true,
    );
    judge(observed, expected)
}

/// One start that was scripted Unknown, then the lookup by lease id, then the retry.
struct Leg {
    first: StartAck,
    seen: Observed,
    after_lookup: Table,
    retry: StartAck,
    after_retry: Table,
}

fn unknown_start_leg<S: Subject>(s: &mut S, sequence: u64) -> Leg {
    let first = s.provider().start(spec(sequence));
    let seen = s.provider().observe(&lease(sequence));
    let after_lookup = s.booked();
    let retry = s.provider().start(spec(sequence));
    Leg {
        first,
        seen,
        after_lookup,
        retry,
        after_retry: s.booked(),
    }
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
        none_held(),
    );
    judge(observed, expected)
}
