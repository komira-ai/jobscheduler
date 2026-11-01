//! The go of a pool: when its first launch since it was last in `Plan` was made, which starts
//! its budget since the go, and what happens to the records of machines that leave the month
//! window.

use crate::caps::{MONTH, billed_until, spent_since};
use crate::pass::Pass;
use crate::record::{Go, Phase};
use crate::settings::{LaunchMode, Settings};

impl Pass<'_> {
    /// Ends the go of every pool the input has that is not live now: in `Plan` mode by its
    /// setting or by its `live_until`. The go began at the pool's first launch after it was
    /// last seen not live (see [`Pass::begin_go`]), so the next one begins a new budget. A
    /// pool this input does not have keeps its go: its absence may be a moment's.
    ///
    /// The failure brake does not end a go. A pool tripped to `Plan` by the brake keeps its go
    /// (the trip is not its `launch` setting), and a reset keeps it too, so the spend since
    /// the go keeps counting across a trip and a reset: a reset never re-opens a cap on it.
    pub(crate) fn end_go(&mut self) {
        for pool in &self.input.pools {
            if pool.settings.mode(self.now) != LaunchMode::Live {
                self.records.go.remove(&pool.settings.pool);
            }
        }
    }

    /// Begins the go of `pool` at this step if it has none: called by the step that makes the
    /// pool's launch, and so written with the record of that launch before the machine is
    /// asked for. The go is thus the first launch of a pool since it was last seen in `Plan`.
    /// No machine exists before it, so the budget since the go loses nothing of what this go
    /// spent. What a machine of an earlier go that still runs accrues before this first launch
    /// is not in the budget since the go (it is in the day and month windows).
    ///
    /// A pool that was switched to `Plan` and back to live while the scaler was down has no
    /// step that saw the switch, so its go stays: the budget since the go then covers both
    /// goes, which errs toward spending less.
    pub(crate) fn begin_go(&mut self, pool: &str) {
        let begun = Go {
            at: self.now,
            settled: 0,
        };
        self.records.go.entry(pool.to_owned()).or_insert(begun);
    }

    /// Notes the provider kind the pool launches with, for the kind totals that outlive the
    /// pool's place in the file. Written with the launch that notes it, and only when it
    /// changes; a pool with no kind notes none.
    pub(crate) fn note_kind(&mut self, settings: &Settings) {
        if !settings.kind.is_empty() {
            let kinds = &mut self.records.pool_kinds;
            if kinds.get(&settings.pool) != Some(&settings.kind) {
                kinds.insert(settings.pool.clone(), settings.kind.clone());
            }
        }
    }

    /// Forgets the records that no longer count and whose billed time has left the month
    /// window (the longest window of any total), and the refused ones. What a forgotten
    /// record cost since its pool's go is added to the go's `settled`, so the budget since
    /// the go does not lose it with the record.
    pub(crate) fn forget(&mut self) {
        let (now, input) = (self.now, self.input);
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.records.instances)
            .into_iter()
            .partition(|record| {
                let timeout = input.boot_timeout(&record.pool);
                let costs = [
                    record.counts(),
                    billed_until(record, timeout, now) > now.back(MONTH),
                ];
                // A refused acquire never was a machine and costs nothing: it goes at
                // once, so a pool that is refused over and over does not grow its
                // records.
                [
                    record.phase != Phase::Refused,
                    costs.into_iter().any(|keep| keep),
                ]
                .into_iter()
                .all(|keep| keep)
            });
        self.records.instances = kept;
        for record in gone {
            if let Some(go) = self.records.go.get_mut(&record.pool) {
                let timeout = input.boot_timeout(&record.pool);
                let spent = spent_since(&record, timeout, now, go.at);
                go.settled = go.settled.saturating_add(spent);
            }
        }
    }
}
