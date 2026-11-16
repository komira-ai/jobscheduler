//! What `--usage-db`, `--usage-off`, `--profile-store-bytes` and `--history-retention-days`
//! (or the file's keys) decide about the usage side, and the seams a caller may fill.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_state::Store;
use js_types::Clock;

use crate::clock::SharedClock;
use crate::error::StartupError;
use crate::file::UsageKeys;
use crate::usage::{HostLimits, UsageSink};
use crate::usage_hub::Usage;
use crate::usage_js::{self, StoreSetup, no_names};

/// What `--usage-db`, `--usage-off`, `--profile-store-bytes` and
/// `--history-retention-days` decided, and the seam a caller may fill: the sink that
/// measures.
pub(crate) struct UsageSetup {
    /// The usage file; `None` when usage is off.
    pub(crate) db: Option<PathBuf>,
    pub(crate) profile_store_bytes: u64,
    pub(crate) history_retention_days: u64,
    /// The sink to use in place of the one that opens `db`.
    pub(crate) sink: Option<Arc<dyn UsageSink>>,
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
        }
    }

    /// The usage side for hosts the pool books as `limits`, or `None` when usage is off.
    /// With a state store `kept` the usage rows are rows of it and no file is opened; else a
    /// usage file that will not open is a refused start naming `--usage-db`. The flag
    /// answers whether the sink keeps its rows in `kept`: a sink the caller supplied
    /// keeps them wherever it likes, so it is false for one.
    pub(crate) fn build(
        self,
        clock: &SharedClock,
        limits: HostLimits,
        kept: Option<Arc<dyn Store>>,
    ) -> Result<(Option<Arc<Usage>>, bool), StartupError> {
        let Some(db) = self.db else {
            return Ok((None, false));
        };
        let in_store = self.sink.is_none() && kept.is_some();
        let sink = match self.sink {
            Some(sink) => sink,
            None => {
                let setup = StoreSetup {
                    db,
                    profile_store_bytes: self.profile_store_bytes,
                    history_retention_days: self.history_retention_days,
                    names: no_names(),
                };
                usage_js::open(&setup, kept, clock.now().millis() / MILLIS_PER_DAY)?
            }
        };
        let usage = Usage::new(sink, clock.clone(), limits);
        Ok((Some(Arc::new(usage)), in_store))
    }
}

/// The usage file `--usage-db`, `--usage-off` or the file's keys name: none when usage is
/// off, else `db` or `usage.db` in the directory of the state file.
fn usage_db(off: bool, db: Option<PathBuf>, state_file: &Path) -> Option<PathBuf> {
    let beside = || state_file.with_file_name("usage.db");
    (!off).then(|| db.unwrap_or_else(beside))
}

/// What the usage settings decide, for the state file `state_file`.
pub(crate) fn usage_setup(keys: UsageKeys, state_file: &Path) -> UsageSetup {
    let db = usage_db(keys.off, keys.db, state_file);
    UsageSetup::new(db, keys.profile_store_bytes, keys.history_retention_days)
}

#[cfg(test)]
mod tests {
    use js_state::fake::{Fake, Flaw};
    use js_types::ManualClock;

    use super::*;
    use crate::usage::NoUsage;
    use crate::usage_map::limits_of;

    fn kept() -> Arc<dyn Store> {
        let clock = Arc::new(js_state::ManualClock::default());
        Arc::new(Fake::with_flaw(clock, Flaw::None))
    }

    fn built(sink: Option<Arc<dyn UsageSink>>, kept: Option<Arc<dyn Store>>) -> (bool, bool) {
        let mut setup = UsageSetup::new(Some(PathBuf::from("usage.db")), 1 << 20, 90);
        setup.sink = sink;
        let clock = SharedClock::new(ManualClock::default());
        let (usage, in_store) = setup.build(&clock, limits_of(1000, 1 << 30), kept).unwrap();
        (usage.is_some(), in_store)
    }

    #[test]
    fn the_sink_the_server_opens_over_the_state_store_is_in_it() {
        assert_eq!(built(None, Some(kept())), (true, true));
    }

    #[test]
    fn a_sink_the_caller_supplies_is_not_in_the_state_store_even_with_one_kept() {
        let sink: Arc<dyn UsageSink> = Arc::new(NoUsage);
        assert_eq!(built(Some(sink), Some(kept())), (true, false));
    }

    #[test]
    fn usage_off_is_in_no_store() {
        let mut setup = UsageSetup::new(None, 1 << 20, 90);
        setup.sink = None;
        let clock = SharedClock::new(ManualClock::default());
        let (usage, in_store) = setup
            .build(&clock, limits_of(1000, 1 << 30), Some(kept()))
            .unwrap();
        assert!(usage.is_none());
        assert!(!in_store);
    }
}
