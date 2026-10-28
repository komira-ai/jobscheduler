//! Retention of the node series.

use std::num::NonZeroUsize;

use js_state::{Cond, ResumeToken, ScanRange, Txn, Version};

use crate::MINUTES_PER_HOUR;
use crate::error::UsageError;
use crate::store::{NODE_PREFIX, NodeSeries, hour_of_key};

/// How long a node minute is kept: 14 days of wall minutes.
pub const NODE_RETENTION_MINUTES: u64 = 14 * 24 * 60;

/// Rows per page of the prune scan.
const PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

/// What one [`NodeSeries::prune`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Node rows looked at.
    pub scanned: u64,
    /// Hour rows deleted.
    pub deleted: u64,
    /// Rows left because they were rewritten between the scan and the delete.
    pub rewritten: u64,
    /// Keys under the prefix that are not hour rows; left alone.
    pub unrecognized: u64,
}

impl NodeSeries<'_> {
    /// Deletes every hour row whose last minute is before `now_wall_minute` less
    /// [`NODE_RETENTION_MINUTES`]: the hour that holds the cutoff stays until it is
    /// entirely older. Run it from the server's tick, about once a day; it does not rely
    /// on key expiry.
    ///
    /// # Errors
    /// [`UsageError::Store`] when a scan or a delete fails; rows already deleted stay so.
    pub fn prune(&self, now_wall_minute: u64) -> Result<PruneStats, UsageError> {
        let cutoff = now_wall_minute.saturating_sub(NODE_RETENTION_MINUTES);
        let mut stats = PruneStats::default();
        let mut doomed: Vec<(String, Version)> = Vec::new();
        let mut token: Option<ResumeToken> = None;
        loop {
            let mut scan = ScanRange::new(NODE_PREFIX, PAGE);
            if let Some(token) = &token {
                scan = scan.resume(token);
            }
            let page = self.store.scan_range(scan)?;
            for (key, value) in page.rows {
                stats.scanned += 1;
                match hour_of_key(&key) {
                    None => stats.unrecognized += 1,
                    Some(hour) => {
                        let last_minute = hour * MINUTES_PER_HOUR + (MINUTES_PER_HOUR - 1);
                        if last_minute < cutoff {
                            doomed.push((key, value.version));
                        }
                    }
                }
            }
            match page.resume {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        if doomed.is_empty() {
            return Ok(stats);
        }
        // One transaction per row: each is judged on its own version, and the batch is one
        // sync.
        let txns: Vec<Txn> = doomed
            .into_iter()
            .map(|(key, version)| Txn::new().delete(key, Cond::Version(version)))
            .collect();
        for result in self.store.apply_batch(txns)? {
            match result {
                Ok(_) => stats.deleted += 1,
                Err(_) => stats.rewritten += 1,
            }
        }
        Ok(stats)
    }
}
