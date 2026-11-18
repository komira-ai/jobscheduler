//! The table of provider calls: a failure handed over during its retry pause, one handed
//! over after it, and the answer and pause of a call that a later call of the same key
//! replaced.

use super::{Begun, Flights, Key};
use crate::lifecycle::ProviderAnswer;

fn key() -> Key {
    ("tier".to_owned(), 1, "js.r1".to_owned(), 0)
}

fn unavailable() -> ProviderAnswer {
    ProviderAnswer::Unavailable("down".to_owned())
}

/// Asserts that a failure handed over before its pause has passed is handed over once,
/// sends nothing with it, and that the pause's end then finds nothing to mark, so the next
/// ask sends the call. It catches a table that sends a failure again before its pause.
#[test]
fn a_failure_handed_over_during_its_pause_is_not_sent_again_by_its_end() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    flights.finish(key(), 1, unavailable());
    assert_eq!(flights.begin(key()), Begun::Settled(unavailable()));
    flights.rest(&key(), 1);
    assert_eq!(flights.begin(key()), Begun::Send(2));
}

/// Asserts that an unavailable answer whose pause has passed is handed over with the call
/// to be sent again, the entry then out, and that a rested refusal is handed over alone. It
/// catches a table that sends a refusal again, or a rested failure that is only handed over.
#[test]
fn a_rested_failure_is_sent_again_and_a_rested_refusal_is_not() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    flights.finish(key(), 1, unavailable());
    flights.rest(&key(), 1);
    assert_eq!(flights.begin(key()), Begun::Retry(unavailable(), 2));
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    let refused = ProviderAnswer::Refused("bad".to_owned());
    flights.finish(key(), 2, refused.clone());
    flights.rest(&key(), 2);
    assert_eq!(flights.begin(key()), Begun::Settled(refused));
    assert_eq!(flights.begin(key()), Begun::Send(3));
}

/// Asserts that the pause's end of a call whose failure was handed over during it leaves
/// the entry of the call sent after it alone: still out, so nothing is sent while it is out,
/// and its own failure is not rested by the first call's pause. It catches a table in which
/// a late pause removes a live entry, so the same call is sent twice at once, or rests a
/// later failure before its own pause.
#[test]
fn the_pause_of_an_earlier_call_leaves_a_later_one_alone() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    flights.finish(key(), 1, unavailable());
    assert_eq!(flights.begin(key()), Begun::Settled(unavailable()));
    assert_eq!(flights.begin(key()), Begun::Send(2));
    flights.rest(&key(), 1);
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending),
        "the later call is still out"
    );
    flights.finish(key(), 2, unavailable());
    flights.rest(&key(), 1);
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(unavailable()),
        "handed over, not sent again: its own pause has not passed"
    );
}

/// Asserts that the answer of a call that a later call of the same key replaced is dropped:
/// the later call stays out and its own answer is the one handed over. It catches a table
/// that hands one call's answer to another's ask.
#[test]
fn the_answer_of_a_replaced_call_is_dropped() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    flights.finish(key(), 1, unavailable());
    assert_eq!(flights.begin(key()), Begun::Settled(unavailable()));
    assert_eq!(flights.begin(key()), Begun::Send(2));
    flights.finish(key(), 1, ProviderAnswer::Done(None));
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    flights.finish(key(), 2, ProviderAnswer::Done(Some("/d".to_owned())));
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Done(Some("/d".to_owned())))
    );
}
