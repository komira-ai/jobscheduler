//! The table of provider calls: a failure handed over and the call sent again only when
//! asked again, the answer of a call that a later call of the same key replaced, the call
//! that follows a success, and the entries a release forgets.

use super::{Begun, Flights, Key};
use crate::lifecycle::ProviderAnswer;

fn key() -> Key {
    ("tier".to_owned(), 1, "js.r1".to_owned(), 1, 0)
}

/// The key of a `Prepare`, whose `Mount` is [`key`].
fn prepared() -> Key {
    ("tier".to_owned(), 0, "js.r1".to_owned(), 1, 0)
}

fn directory() -> ProviderAnswer {
    ProviderAnswer::Done(Some("/d".to_owned()))
}

fn unavailable() -> ProviderAnswer {
    ProviderAnswer::Unavailable("down".to_owned())
}

/// Asserts that a failure (unavailable or refused) is handed over once and sends nothing
/// with it, however long after its answer it is asked for, and that only the ask after it
/// sends the call again. It catches a table that sends a failed call again by itself, which
/// would ask a provider in trouble again before the lifecycle's wait has passed.
#[test]
fn a_failure_is_handed_over_once_and_sent_again_only_when_asked_again() {
    let flights = Flights::default();
    let refused = ProviderAnswer::Refused("bad".to_owned());
    for (number, answer) in [(1, unavailable()), (2, refused)] {
        assert_eq!(flights.begin(key()), Begun::Send(number));
        assert_eq!(
            flights.begin(key()),
            Begun::Settled(ProviderAnswer::Pending)
        );
        flights.finish(key(), number, answer.clone(), None);
        assert!(flights.wait_answered(&key(), std::time::Duration::ZERO));
        assert_eq!(flights.begin(key()), Begun::Settled(answer));
    }
    assert_eq!(flights.begin(key()), Begun::Send(3));
}

/// Asserts that the answer of a call that a later call of the same key replaced is dropped:
/// the later call stays out and its own answer is the one handed over. It catches a table
/// that hands one call's answer to another's ask.
#[test]
fn the_answer_of_a_replaced_call_is_dropped() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    flights.finish(key(), 1, unavailable(), None);
    assert_eq!(flights.begin(key()), Begun::Settled(unavailable()));
    assert_eq!(flights.begin(key()), Begun::Send(2));
    flights.finish(key(), 1, ProviderAnswer::Done(None), None);
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    flights.finish(key(), 2, ProviderAnswer::Done(Some("/d".to_owned())), None);
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Done(Some("/d".to_owned())))
    );
}

/// Asserts that a success that names the call following it marks that call out in the same
/// turn and gives its number: the following call is then pending, the success is handed over,
/// and the following call's answer is the one its ask is handed. It catches a following call
/// that is not marked out, so the lifecycle's ask sends it a second time, or whose answer is
/// dropped.
#[test]
fn a_success_marks_the_call_that_follows_it_out() {
    let flights = Flights::default();
    assert_eq!(flights.begin(prepared()), Begun::Send(1));
    assert_eq!(
        flights.finish(prepared(), 1, ProviderAnswer::Done(None), Some(key())),
        Some(2)
    );
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    assert_eq!(
        flights.begin(prepared()),
        Begun::Settled(ProviderAnswer::Done(None))
    );
    assert_eq!(flights.finish(key(), 2, directory(), None), None);
    assert_eq!(flights.begin(key()), Begun::Settled(directory()));
}

/// Asserts that nothing follows a refusal, a failure, or the answer of a call that was
/// replaced: no number is given and the following call has no entry, so its ask sends it. It
/// catches a mount sent after a prepare that the provider refused or could not take, or after
/// an answer the table dropped.
#[test]
fn nothing_follows_a_refusal_a_failure_or_a_replaced_call() {
    let flights = Flights::default();
    let refused = ProviderAnswer::Refused("bad".to_owned());
    for (number, answer) in [(1, refused), (2, unavailable())] {
        assert_eq!(flights.begin(prepared()), Begun::Send(number));
        assert_eq!(
            flights.finish(prepared(), number, answer.clone(), Some(key())),
            None
        );
        assert_eq!(flights.begin(prepared()), Begun::Settled(answer));
    }
    assert_eq!(flights.begin(prepared()), Begun::Send(3));
    assert_eq!(
        flights.finish(prepared(), 2, ProviderAnswer::Done(None), Some(key())),
        None,
        "the answer of a call no longer out"
    );
    assert_eq!(flights.begin(key()), Begun::Send(4));
}

/// Asserts that a success does not replace an entry the following call already has: a mount
/// that was asked stays out as it was sent, and its own answer is the one handed over. It
/// catches a following call that is sent over one already out, so the provider is asked twice
/// at once.
#[test]
fn a_following_call_is_not_sent_over_one_of_its_own() {
    let flights = Flights::default();
    assert_eq!(flights.begin(key()), Begun::Send(1));
    assert_eq!(flights.begin(prepared()), Begun::Send(2));
    assert_eq!(
        flights.finish(prepared(), 2, ProviderAnswer::Done(None), Some(key())),
        None
    );
    flights.finish(key(), 1, directory(), None);
    assert_eq!(flights.begin(key()), Begun::Settled(directory()));
}

/// Asserts that a forgotten call out is neither handed over nor followed when it answers, and
/// that a forgotten answer is not handed over: the next asks of both keys send them. It
/// catches a released source whose prepare, still out, is followed by a mount, and a released
/// source's old answer handed to a later ask.
#[test]
fn a_forgotten_call_is_neither_handed_over_nor_followed() {
    let flights = Flights::default();
    assert_eq!(flights.begin(prepared()), Begun::Send(1));
    flights.forget(&[prepared(), key()]);
    assert_eq!(
        flights.finish(prepared(), 1, ProviderAnswer::Done(None), Some(key())),
        None
    );
    assert_eq!(flights.begin(prepared()), Begun::Send(2));
    flights.finish(prepared(), 2, ProviderAnswer::Done(None), Some(key()));
    flights.forget(&[prepared(), key()]);
    assert_eq!(flights.begin(prepared()), Begun::Send(4));
    assert_eq!(flights.begin(key()), Begun::Send(5));
}
