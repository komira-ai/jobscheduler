//! The platform matches of a placement round, and the matches the scheduler keeps between
//! rounds.
//!
//! A round asks, for each platform request and lease kind it looks at, which of its live
//! workers serve the kind and satisfy the platform: the static match, read off each worker's
//! capabilities alone. A [`Memo`] answers it once per request and kind in the round. The
//! scheduler keeps a round's memo, as [`Matches`], for the rounds after it only while its
//! caller holds the matches ([`crate::Scheduler::hold_matches`]), as the placer does over the
//! rounds of one pass, one round per queued job. A later round takes the kept matches only
//! if it has the same live workers in the same order; the scheduler forgets them whenever it
//! writes a worker's capabilities (a registration or a capacity report). Nothing else a round
//! reads changes a match: a placement, a booking, a reading of load, a reservation or a hold
//! changes a worker's room or rank, which every round reads afresh, and a cordon, a phase or
//! a worker gone silent changes the live workers, which the round compares.

use std::collections::BTreeMap;

use js_types::sched::{LeaseKind, WorkerId};

use super::serves;
use crate::input::Request;
use crate::scheduler::Worker;

/// What a match is of: a platform request and a lease kind.
type Key = (js_caps::Request, LeaseKind);

/// The matches of one round's live workers, by platform request and lease kind.
#[derive(Clone, Debug, Default)]
pub(crate) struct Memo {
    /// For each request and kind matched, the indices into the round's live workers of
    /// those that serve the kind and satisfy the request.
    found: Vec<(Key, Vec<usize>)>,
    /// How many times a worker was matched against a request, over every round the memo
    /// was carried through.
    evaluations: u64,
}

impl Memo {
    /// Where the workers of `live` that serve `request`'s kind and satisfy its platform are
    /// listed ([`Memo::listed`]): matched now, against each worker's capabilities in
    /// `workers`, unless the memo has them already.
    pub(crate) fn find(
        &mut self,
        live: &[WorkerId],
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> usize {
        let key = (request.needs.clone(), request.kind);
        if let Some(at) = self.found.iter().position(|(seen, _)| *seen == key) {
            return at;
        }
        let matching = live
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                let caps = &workers[*name].caps;
                serves(caps, request.kind) && request.needs.matches(caps)
            })
            .map(|(i, _)| i)
            .collect();
        let evaluated = u64::try_from(live.len()).unwrap_or(u64::MAX);
        self.evaluations = self.evaluations.saturating_add(evaluated);
        self.found.push((key, matching));
        self.found.len() - 1
    }

    /// The indices into the live workers listed at `at`, a place [`Memo::find`] gave.
    pub(crate) fn listed(&self, at: usize) -> &[usize] {
        &self.found[at].1
    }
}

/// The matches the scheduler keeps between rounds: a round's memo and the live workers it
/// is of.
#[derive(Clone, Debug, Default)]
pub(crate) struct Matches {
    /// The live workers the memo's indices are into, in name order.
    live: Vec<WorkerId>,
    memo: Memo,
}

impl Matches {
    /// The matches a round over `live` and its memo leaves.
    pub(crate) const fn kept(live: Vec<WorkerId>, memo: Memo) -> Self {
        Self { live, memo }
    }

    /// The memo for a round over `live`: these matches if they are of the same live workers,
    /// else an empty one. The count of evaluations is carried either way.
    pub(crate) fn for_round(self, live: &[WorkerId]) -> Memo {
        if self.live == live {
            return self.memo;
        }
        Memo {
            found: Vec::new(),
            evaluations: self.memo.evaluations,
        }
    }

    /// Forgets every match, keeping the count of evaluations.
    pub(crate) fn forget(&mut self) {
        self.live = Vec::new();
        self.memo.found = Vec::new();
    }

    /// How many times a worker was matched against a request so far.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn evaluations(&self) -> u64 {
        self.memo.evaluations
    }
}
