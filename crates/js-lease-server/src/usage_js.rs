//! The sink the server runs on when nothing else is given: the one place that decides
//! how usage is measured and kept.
//!
//! This build links no store, so the sink is [`NoUsage`]: heartbeats are served exactly
//! as with a store, every reading is taken and forgotten, and the read routes answer
//! empty. Linking the usage crate's recorder, node series and profile store changes this
//! file alone: [`open`] builds a sink over them (and returns the startup error of a
//! store that will not open, naming `--usage-db`), and nothing else in the server knows.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::info;

use crate::usage::{NoUsage, UsageSink};

/// Where the usage store is and how large the profile part of it may grow.
#[derive(Debug, Clone)]
pub(crate) struct StoreSetup {
    pub(crate) db: PathBuf,
    pub(crate) profile_store_bytes: u64,
}

/// The sink over the store `setup` names.
pub(crate) fn open(setup: &StoreSetup) -> Arc<dyn UsageSink> {
    info!(
        db = %setup.db.display(),
        profile_store_bytes = setup.profile_store_bytes,
        "this build links no usage store: usage is not recorded"
    );
    Arc::new(NoUsage)
}
