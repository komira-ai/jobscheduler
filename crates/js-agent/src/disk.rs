//! The free scratch disk after the hello. The hello says the disk once (`disk-gib`); the
//! session asks a [`DiskGauge`] before each heartbeat and tells the server when the answer
//! differs from what the server last took (`Heartbeat::disk_gib`).
//!
//! [`ScratchDisk`] is the gauge the program uses. It says what [`crate::offer`] would say
//! if the hello were made now (the free space of the `--scratch-root` volume, less the
//! unused budget of the bundle store, less the unused image budget, less
//! `--scratch-low-water-gib`, in whole GiB), and it
//! reads `df` at most once in [`REREAD_MS`], so a heartbeat every few seconds does not
//! start a program every few seconds. A volume that cannot be read gives no answer, which
//! the session does not send: the server keeps the last value it took.
//!
//! **The image budget** (`--image-budget-gib`, the crun runtime's store of unpacked images,
//! which the runtime keeps within the budget by evicting the least recently used image) is
//! left out of the disk offered as the bundle budget is: only the part of it the store does
//! not hold yet ([`image_kept_back`]). The images the store holds are on the scratch volume
//! (the store is in the directory above the scratch root, on the same file system), so the
//! free space read already leaves them out; leaving out the whole budget as well would count
//! them twice. So the bytes the store holds win: a store at or over its budget leaves nothing
//! more out, and an empty one leaves out the whole budget.

use std::cell::Cell;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::SharedClock;
use crate::bundle::{BundleHost, Settings, reserve_bytes};
use crate::cli_runtime::CRUN_STORE_DIR;
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

/// The directory of the crun runtime's unpacked images under its store.
pub const IMAGES_DIR: &str = "oci-images";

/// The image store whose unused budget the disk offered leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReserve {
    /// `--image-budget-gib`, in bytes.
    pub budget_bytes: u64,
    /// The directory of the unpacked images ([`image_store`]).
    pub store: PathBuf,
}

impl ImageReserve {
    /// The reserve of a budget of `budget` bytes (none without one) over the image store of
    /// the scratch root `root` ([`image_store`]).
    #[must_use]
    pub fn of(budget: Option<u64>, root: &Path) -> Option<Self> {
        budget.map(|budget_bytes| Self {
            budget_bytes,
            store: image_store(root),
        })
    }
}

/// `--image-budget-gib`, checked, in bytes: none when it is not given (the image store has
/// no budget, nothing is evicted and nothing is left out of the disk offered), and above 0
/// when it is. A value that is not a whole number is refused by the parser, as
/// `--bundle-budget-gib` is.
///
/// # Errors
/// The flag is 0.
pub fn image_budget_bytes(gib: Option<u64>) -> Result<Option<u64>, String> {
    match gib {
        Some(0) => Err("--image-budget-gib: above 0 (leave it out for no budget)".to_owned()),
        gib => Ok(gib.map(|gib| gib.saturating_mul(GIB))),
    }
}

/// The bytes of the image budget the disk offered leaves out: the part of `budget` the
/// image store does not hold yet (`held`), 0 without a budget. See the [module](self)
/// documentation for why the bytes held are not left out again.
#[must_use]
pub fn image_kept_back(budget: Option<u64>, held: u64) -> u64 {
    budget.map_or(0, |budget| budget.saturating_sub(held))
}

/// The directory of the unpacked images of a crun agent whose scratch root is `root`: the
/// store is in the directory above the root (`--scratch-root`).
#[must_use]
pub fn image_store(root: &Path) -> PathBuf {
    root.parent()
        .unwrap_or(root)
        .join(CRUN_STORE_DIR)
        .join(IMAGES_DIR)
}

/// What the image budget leaves out of the disk offered now, and a warning when the store
/// cannot be read (its whole budget is left out then).
#[must_use]
pub fn image_reserve_bytes(reserve: &ImageReserve) -> (u64, Option<String>) {
    match held_images(&reserve.store) {
        Ok(held) => {
            let bytes = held.iter().map(|(_, bytes)| bytes).sum();
            (image_kept_back(Some(reserve.budget_bytes), bytes), None)
        }
        Err(why) => (
            reserve.budget_bytes,
            Some(format!(
                "--image-budget-gib: cannot read the bytes the image store {} holds ({why}); \
                 its whole budget is kept back from jobs",
                reserve.store.display()
            )),
        ),
    }
}

/// The disk `dir` and everything below it holds, links not followed: for each entry the
/// larger of the blocks it has (512 bytes each) and its length, so that an entry a file
/// system gives no blocks (a directory on some) still counts, and a sparse file counts by
/// its length. A file with several links counts once for each. An entry that goes, or that
/// cannot be read, while it is measured counts for nothing.
///
/// # Errors
/// `dir` itself cannot be read.
pub(crate) fn tree_bytes(dir: &Path) -> io::Result<u64> {
    let mut total = entry_bytes(&std::fs::symlink_metadata(dir)?);
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        let entries = std::fs::read_dir(&next).into_iter().flatten().flatten();
        for (path, meta) in entries.filter_map(|e| e.metadata().ok().map(|m| (e.path(), m))) {
            total = total.saturating_add(entry_bytes(&meta));
            if meta.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(total)
}

/// The disk one entry holds, as [`tree_bytes`] counts it.
fn entry_bytes(meta: &std::fs::Metadata) -> u64 {
    meta.blocks().saturating_mul(512).max(meta.len())
}

/// The complete images in the store `dir` (a directory named by 64 lowercase hex digits
/// with a `rootfs` directory in it, links not followed), by digest (`sha256:<hex>`), each
/// with the disk it holds ([`tree_bytes`]); anything else there (an unpack under way, a
/// removal set aside) is not an image. A store that is not there holds none.
///
/// # Errors
/// The store cannot be listed.
pub(crate) fn held_images(dir: &Path) -> io::Result<Vec<(String, u64)>> {
    let entries = match std::fs::read_dir(dir) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let is_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let mut held: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .filter(|hex| is_dir(&dir.join(hex)) && is_dir(&dir.join(hex).join("rootfs")))
        .map(|hex| {
            let bytes = tree_bytes(&dir.join(&hex)).unwrap_or(0);
            (format!("sha256:{hex}"), bytes)
        })
        .collect();
    held.sort();
    Ok(held)
}

/// [`DiskGauge`] over the `--scratch-root` volume.
pub struct ScratchDisk<'a> {
    root: PathBuf,
    low_water_gib: Option<u64>,
    runner: &'a dyn CommandRunner,
    reserve: Option<Reserve>,
    images: Option<ImageReserve>,
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
            images: None,
            clock,
            last: Cell::new(None),
        }
    }

    /// This gauge leaving out, as well, the unused budget of the image store `images`.
    #[must_use]
    pub fn with_images(mut self, images: Option<ImageReserve>) -> Self {
        self.images = images;
        self
    }

    fn read(&self) -> Option<u64> {
        let bundle = self
            .reserve
            .as_ref()
            .map_or(0, |store| reserve_bytes(&store.settings, &*store.host).0);
        let images = self
            .images
            .as_ref()
            .map_or(0, |store| image_reserve_bytes(store).0);
        let kept_back = bundle.saturating_add(images);
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

#[cfg(test)]
#[path = "disk_tests.rs"]
mod tests;
