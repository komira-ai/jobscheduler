//! Tests of the uid range the agent is given.

use super::*;

fn refusal(result: Result<Slots, AccountError>) -> String {
    match result {
        Err(AccountError::BadSlots(why)) => why,
        other => panic!("not a refusal of the slots: {other:?}"),
    }
}

/// Asserts that a count of slots from a first uid is that many uids, that the range and the
/// count say so, and that the lowest and the highest range allowed are taken.
#[test]
fn a_count_from_a_first_uid_is_that_many_uids() {
    let slots = Slots::new(700, 100).unwrap();
    assert_eq!(slots.range(), 700..=799);
    assert_eq!(slots.count(), 100);
    assert_eq!(Slots::new(700, 1).unwrap().range(), 700..=700);
    assert_eq!(Slots::new(700, 1).unwrap().count(), 1);
    assert_eq!(Slots::new(501, 1).unwrap().range(), 501..=501);
    assert_eq!(Slots::new(59_999, 2).unwrap().range(), 59_999..=60_000);
    assert_eq!(Slots::new(501, 60_000 - 500).unwrap().count(), 59_500);
}

/// Asserts that a count of 0, a first uid of the system's or the first user's (500 and
/// below, said in the message), a last uid above 60000 and a count that goes past the
/// largest uid are each refused, naming what is wrong.
#[test]
fn a_count_that_leaves_the_allowed_uids_is_refused_in_words() {
    assert_eq!(
        refusal(Slots::new(700, 0)),
        "the count is 0, so there is no slot"
    );
    assert_eq!(
        refusal(Slots::new(500, 10)),
        "the first uid, 500, is below 501: macOS keeps the uids up to 500 for the system and \
         the first user"
    );
    assert!(refusal(Slots::new(0, 10)).contains("the first uid, 0, is below 501"));
    assert_eq!(
        refusal(Slots::new(59_999, 3)),
        "the last uid, 60001, is above 60000"
    );
    assert_eq!(
        refusal(Slots::new(u32::MAX, 2)),
        "2 slots from uid 4294967295 go past the largest uid"
    );
    assert_eq!(
        Slots::new(700, 0).unwrap_err().to_string(),
        "the account uid range: the count is 0, so there is no slot"
    );
}

/// Asserts that `<first>-<last>` reads as both uids included, and that what is not two uids
/// joined by a dash, a first above the last, a first below 501 and a last above 60000 are
/// each refused, naming what is wrong.
#[test]
fn a_range_is_two_uids_both_included() {
    let slots: Slots = "700-799".parse().unwrap();
    assert_eq!(slots, Slots::new(700, 100).unwrap());
    assert_eq!("700-700".parse::<Slots>().unwrap().count(), 1);
    assert_eq!("501-60000".parse::<Slots>().unwrap().range(), 501..=60_000);
    for wrong in [
        "",
        "700",
        "a-b",
        "700-",
        "-700",
        "700-799-800",
        "7 00-799",
        "-1-5",
    ] {
        assert_eq!(
            refusal(wrong.parse()),
            format!("{wrong:?} is not <first>-<last>, two uids"),
            "{wrong}"
        );
    }
    assert_eq!(
        refusal("799-700".parse()),
        "the first uid, 799, is above the last, 700"
    );
    assert!(refusal("500-700".parse()).contains("the first uid, 500, is below 501"));
    assert_eq!(
        refusal("700-60001".parse()),
        "the last uid, 60001, is above 60000"
    );
    assert_eq!(
        refusal("700-4294967296".parse()),
        "\"700-4294967296\" is not <first>-<last>, two uids"
    );
}
