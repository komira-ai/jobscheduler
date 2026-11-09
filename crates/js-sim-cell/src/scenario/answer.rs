//! The expectation on one raw answer the client was given, [`super::Expect::answer`], and
//! which of the client's calls an answer is ([`Call`]).
//!
//! It is how a scenario states what the job service answered a call with, a status and
//! when wanted a body (a submit sent again under the same job id answered `201`, say): it
//! compares the answer the front kept ([`crate::Answer`]) with the one wanted. Nothing
//! else outside the front names a status, and no invariant reads one.

use serde_json::Value;

use crate::front::{Answer, CallKind};
use crate::observe::ItemId;
use crate::text;

/// Which of the client's calls an answer is: the item it was made for, what it was for,
/// and which of that item's calls of that kind it was, counted from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    item: ItemId,
    kind: CallKind,
    nth: usize,
}

impl Call {
    /// The `nth` call (from 1) of `kind` made for `item`.
    #[must_use]
    pub fn new(item: ItemId, kind: CallKind, nth: usize) -> Self {
        Self { item, kind, nth }
    }

    /// The call as a statement names it: `item<n> <kind> <nth>`.
    pub(crate) fn name(&self) -> String {
        format!(
            "{} {} {}",
            text::item(self.item),
            self.kind.name(),
            self.nth
        )
    }

    /// Its answer among `answers`, once the call was made.
    fn find<'a>(&self, answers: &'a [Answer]) -> Option<&'a Answer> {
        let mut mine = answers
            .iter()
            .filter(|answer| (answer.item == self.item) & (answer.kind == self.kind));
        mine.nth(self.nth.saturating_sub(1))
    }
}

/// The statement, what it wanted and what it got, of [`super::Expect::answer`]`(call,
/// status, body)` over `answers`: the status, and the body as JSON when one is wanted
/// (compared as JSON, so the order of its keys does not count); `no-answer` before the
/// call is made.
pub(super) fn judged(
    call: &Call,
    status: u16,
    body: Option<&Value>,
    answers: &[Answer],
) -> [String; 3] {
    let with_body = body.is_some();
    let wanted = shown(status, body);
    let found = call.find(answers);
    let got = found.map_or_else(
        || "no-answer".to_owned(),
        |answer| {
            let body = json(&answer.body);
            shown(answer.status, Some(&body).filter(|_| with_body))
        },
    );
    [format!("answer({})", call.name()), wanted, got]
}

/// `status`, and `body` after it when there is one.
fn shown(status: u16, body: Option<&Value>) -> String {
    body.map_or_else(|| status.to_string(), |body| format!("{status} {body}"))
}

/// `body` as JSON, `null` when it is not JSON.
fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    use js_types::sched::FarmTime;

    fn answer(item: u32, kind: CallKind, status: u16, body: &str) -> Answer {
        Answer {
            item: ItemId(item),
            kind,
            at: FarmTime::from_millis(0),
            status,
            body: body.to_owned(),
        }
    }

    /// Asserts that a call is found among the answers by its item, kind and place among
    /// them; that a status alone is compared without the body; that a wanted body is
    /// compared as JSON whatever the order of its keys; that a body that is not JSON reads
    /// as `null`; that a call not made reads `no-answer`; and that the call names itself.
    /// Catches: a call matched to another item's or another kind's answer, a body compared
    /// as text, and a missing call read as an answer.
    #[test]
    fn an_answer_is_judged_by_status_and_body() {
        let answers = [
            answer(0, CallKind::Queue, 201, r#"{"a":1,"b":2}"#),
            answer(1, CallKind::Queue, 409, "not json"),
            answer(0, CallKind::Queue, 200, "{}"),
            answer(0, CallKind::Read, 200, "{}"),
        ];
        let first = Call::new(ItemId(0), CallKind::Queue, 1);
        let second = Call::new(ItemId(0), CallKind::Queue, 2);
        let other = Call::new(ItemId(1), CallKind::Queue, 1);
        let third = Call::new(ItemId(0), CallKind::Queue, 3);
        let status = |call: &Call, status| judged(call, status, None, &answers);
        assert_eq!(
            status(&first, 201),
            ["answer(item0 queue 1)", "201", "201"].map(str::to_owned)
        );
        assert_eq!(status(&second, 201)[2], "200");
        assert_eq!(status(&other, 409)[2], "409");
        let body = serde_json::json!({"b": 2, "a": 1});
        let same = judged(&first, 201, Some(&body), &answers);
        assert_eq!(same[1], same[2]);
        let plain = judged(&other, 409, Some(&Value::Null), &answers);
        assert_eq!(plain[1], plain[2]);
        assert_eq!(
            status(&third, 200),
            ["answer(item0 queue 3)", "200", "no-answer"].map(str::to_owned)
        );
    }
}
