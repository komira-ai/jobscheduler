//! The expectation on one raw answer the client was given, [`super::Expect::answer`], and
//! which of the client's calls an answer is ([`Call`]).
//!
//! It is how a scenario carries what a test of the server's HTTP surface asserted about a
//! status or an error body (a `409` and its `no-capacity`, say): it compares the answer the
//! front kept ([`crate::Answer`]) with the one wanted. Nothing else outside the front names
//! a status, and no invariant reads one.

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

    /// The `nth` acquire (from 1) made for `item`.
    #[must_use]
    pub fn acquire(item: ItemId, nth: usize) -> Self {
        Self::new(item, CallKind::Acquire, nth)
    }

    /// The `nth` release (from 1) made for `item`.
    #[must_use]
    pub fn release(item: ItemId, nth: usize) -> Self {
        Self::new(item, CallKind::Release, nth)
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

/// The lease the answer to `call` named, its body's `id`: what a 201 and a 202 to an
/// acquire name. `None` before the call is made, and for an answer that names none.
pub(crate) fn lease_of(call: &Call, answers: &[Answer]) -> Option<String> {
    let body = call.find(answers).map(|answer| json(&answer.body));
    body.and_then(|body| body["id"].as_str().map(str::to_owned))
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
