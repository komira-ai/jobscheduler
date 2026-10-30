//! The go of a pool: when it went live, which starts its budget since the go, and what
//! happens to the records of machines that leave the month window.

use crate::caps::{MONTH, billed_until, spent_since};
use crate::pass::Pass;
use crate::record::{Go, Phase};
use crate::settings::LaunchMode;

impl Pass<'_> {
    /// Notes the go of every pool the input has: a pool that is live now and has no go
    /// begins one at this step; a pool that is in `Plan` mode, by its setting or by its
    /// `live_until`, loses its go, so the next one begins a new budget. A pool this input
    /// does not have keeps its go: its absence may be a moment's.
    ///
    /// The go is an instant of the clock the step is given and is written with the records
    /// before any machine of the go is launched, so a restart reads it back and does not
    /// begin another. A pool that was switched to `Plan` and back to live while the scaler
    /// was down has no step that saw the switch, so its go stays: the budget since the go
    /// then covers both goes, which errs toward spending less.
    pub(crate) fn mark_go(&mut self) {
        for pool in &self.input.pools {
            let name = &pool.settings.pool;
            if pool.settings.mode(self.now) == LaunchMode::Live {
                let begun = Go {
                    at: self.now,
                    settled: 0,
                };
                self.records.go.entry(name.clone()).or_insert(begun);
            } else {
                self.records.go.remove(name);
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
