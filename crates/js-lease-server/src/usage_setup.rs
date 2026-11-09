//! What `--usage-db`, `--usage-off`, `--profile-store-bytes` and `--history-retention-days`
//! (or the file's keys) decide about the usage side, and the seams a caller may fill.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_types::Clock;

use crate::adapt::SharedClock;
use crate::error::StartupError;
use crate::file::UsageKeys;
use crate::usage::{HostLimits, KeySource, NoKeys, UsageSink};
use crate::usage_hub::Usage;
use crate::usage_js::{self, StoreSetup, no_names};

/// What `--usage-db`, `--usage-off`, `--profile-store-bytes` and
/// `--history-retention-days` decided, and the seams a caller may fill: the sink that
/// measures, and the source of the key of a run.
pub(crate) struct UsageSetup {
    /// The usage file; `None` when usage is off.
    pub(crate) db: Option<PathBuf>,
    pub(crate) profile_store_bytes: u64,
    pub(crate) history_retention_days: u64,
    /// The sink to use in place of the one that opens `db`.
    pub(crate) sink: Option<Arc<dyn UsageSink>>,
    /// The key source a caller supplied; `None` leaves every run without a key.
    pub(crate) keys: Option<Arc<dyn KeySource>>,
}

/// The default of `--profile-store-bytes`.
pub(crate) const DEFAULT_PROFILE_STORE_BYTES: u64 = 268_435_456;

/// The default of `--history-retention-days`.
pub(crate) const DEFAULT_HISTORY_RETENTION_DAYS: u64 = 90;

/// Milliseconds in a day.
const MILLIS_PER_DAY: u64 = 86_400_000;

impl UsageSetup {
    /// The usage side as `db` (`None` for usage off), `profile_store_bytes` and
    /// `history_retention_days` say, with no seam filled.
    fn new(db: Option<PathBuf>, profile_store_bytes: u64, history_retention_days: u64) -> Self {
        Self {
            db,
            profile_store_bytes,
            history_retention_days,
            sink: None,
            keys: None,
        }
    }

    /// The usage side for hosts the pool books as `limits`, or `None` when usage is off.
    /// A usage file that will not open is a refused start naming `--usage-db`. No run has a
    /// key unless the caller supplied a key source.
    pub(crate) fn build(
        self,
        clock: &SharedClock,
        limits: HostLimits,
    ) -> Result<Option<Arc<Usage>>, StartupError> {
        let Some(db) = self.db else {
            return Ok(None);
        };
        let sink = match self.sink {
            Some(sink) => sink,
            None => {
                let setup = StoreSetup {
                    db,
                    profile_store_bytes: self.profile_store_bytes,
                    history_retention_days: self.history_retention_days,
                    names: no_names(),
                };
                usage_js::open(&setup, clock.now().millis() / MILLIS_PER_DAY)?
            }
        };
        let keys = self.keys.unwrap_or_else(|| Arc::new(NoKeys));
        let usage = Usage::new(sink, keys, clock.clone(), limits);
        Ok(Some(Arc::new(usage)))
    }
}

/// The usage file `--usage-db`, `--usage-off` or the file's keys name: none when usage is
/// off, else `db` or `usage.db` in the directory of the lease file.
fn usage_db(off: bool, db: Option<PathBuf>, state_file: &Path) -> Option<PathBuf> {
    let beside = || state_file.with_file_name("usage.db");
    (!off).then(|| db.unwrap_or_else(beside))
}

/// What the usage settings decide, for the lease file at `state_file`.
pub(crate) fn usage_setup(keys: UsageKeys, state_file: &Path) -> UsageSetup {
    let db = usage_db(keys.off, keys.db, state_file);
    UsageSetup::new(db, keys.profile_store_bytes, keys.history_retention_days)
}
