//! The free scratch disk after the hello. The hello says the disk once (`disk-gib`); the
//! session asks a [`DiskGauge`] before each heartbeat and tells the server when the answer
//! differs from what the server last took (`Heartbeat::disk_gib`).
//!
//! [`ScratchDisk`] is the gauge the program uses. It says what [`crate::offer`] would say
//! if the hello were made now (the free space of the `--scratch-root` volume, less the
//! unused budget of the bundle store, less `--scratch-low-water-gib`, in whole GiB), and it
//! reads `df` at most once in [`REREAD_MS`], so a heartbeat every few seconds does not
//! start a program every few seconds. A volume that cannot be read gives no answer, which
//! the session does not send: the server keeps the last value it took.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use crate::SharedClock;
use crate::bundle::{BundleHost, Settings, reserve_bytes};
use crate::facts::{CommandRunner, GIB};
use crate::host::offered_on;
use crate::lifecycle::Clock;

/// The least time between two reads of the volume, in milliseconds.
pub const REREAD_MS: u64 = 30_000;

/// What a session asks before a heartbeat: the whole GiB of free scratch disk the host
/// offers now, or `None` when it cannot say.
pub trait DiskGauge {
    /// The free scratch disk in GiB.
    fn gib(&self) -> Option<u64>;
}

/// The bundle store whose unused budget the disk offered leaves out.
pub struct Reserve {
    /// The checked flags of the store.
    pub settings: Settings,
    /// The machine, as the store sees it.
    pub host: Arc<dyn BundleHost>,
}

/// [`DiskGauge`] over the `--scratch-root` volume.
pub struct ScratchDisk<'a> {
    root: PathBuf,
    low_water_gib: Option<u64>,
    runner: &'a dyn CommandRunner,
    reserve: Option<Reserve>,
    clock: SharedClock,
    /// When the volume was last read, and what it said.
    last: Cell<Option<(u64, Option<u64>)>>,
}

impl<'a> ScratchDisk<'a> {
    /// A gauge over `root`, with the `--scratch-low-water-gib` mark `low_water_gib`, that
    /// starts `df` with `runner`, counts [`REREAD_MS`] on `clock`, and leaves out the unused
    /// budget of `reserve` when there is a store.
    pub fn new(
        root: PathBuf,
        low_water_gib: Option<u64>,
        runner: &'a dyn CommandRunner,
        reserve: Option<Reserve>,
        clock: SharedClock,
    ) -> Self {
        Self {
            root,
            low_water_gib,
            runner,
            reserve,
            clock,
            last: Cell::new(None),
        }
    }

    fn read(&self) -> Option<u64> {
        let kept_back = self
            .reserve
            .as_ref()
            .map_or(0, |store| reserve_bytes(&store.settings, &*store.host).0);
        offered_on(self.runner, &self.root, kept_back, self.low_water_gib)
            .ok()
            .map(|bytes| bytes / GIB)
    }
}

impl DiskGauge for ScratchDisk<'_> {
    fn gib(&self) -> Option<u64> {
        let now = self.clock.now_ms();
        match self.last.get() {
            Some((at, said)) if now.saturating_sub(at) < REREAD_MS => said,
            _ => {
                let said = self.read();
                self.last.set(Some((now, said)));
                said
            }
        }
    }
}
