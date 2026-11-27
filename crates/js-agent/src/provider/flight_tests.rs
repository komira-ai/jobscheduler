//! The table of provider calls: a failure handed over during its retry pause, one handed
//! over after it, the answer and pause of a call that a later call of the same key
//! replaced, the call that follows a success, and the entries a release forgets.

use super::{Begun, Flights, Key};
use crate::lifecycle::ProviderAnswer;

fn key() -> Key {
    ("tier".to_owned(), 1, "js.r1".to_owned(), 0)
}

/// The key of a `Prepare`, whose `Mount` is [`key`].
fn prepared() -> Key {
    ("tier".to_owned(), 0, "js.r1".to_owned(), 0)
}

fn directory() -> ProviderAnswer {
    ProviderAnswer::Done(Some("/d".to_owned()))
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
    flights.finish(key(), 1, unavailable(), None);
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
    flights.finish(key(), 1, unavailable(), None);
    flights.rest(&key(), 1);
    assert_eq!(flights.begin(key()), Begun::Retry(unavailable(), 2));
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending)
    );
    let refused = ProviderAnswer::Refused("bad".to_owned());
    flights.finish(key(), 2, refused.clone(), None);
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
    flights.finish(key(), 1, unavailable(), None);
    assert_eq!(flights.begin(key()), Begun::Settled(unavailable()));
    assert_eq!(flights.begin(key()), Begun::Send(2));
    flights.rest(&key(), 1);
    assert_eq!(
        flights.begin(key()),
        Begun::Settled(ProviderAnswer::Pending),
        "the later call is still out"
    );
    flights.finish(key(), 2, unavailable(), None);
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
