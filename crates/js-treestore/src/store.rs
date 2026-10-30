//! [`TreeStore`]: ensure, pin, evict and sweep.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::SystemTime;

use js_bundle::media::LayerEncoding;
use js_bundle::{DiffId, Limits, unpack_layer};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Step, ctx};
use crate::record::Record;
use crate::sealer::Sealer;
use crate::source::{BlobRef, BlobSink, Digest, LabelError, LayerSource};

/// The version of the extractor's rules. Tightening a rule bumps it, and trees made under
/// an older version are extracted again.
pub const EXTRACTOR_VERSION: u32 = 1;

/// A run that pins trees: `[a-z0-9][a-z0-9._-]{0,127}`, so it is a file name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(String);

impl RunId {
    /// Validates a run id.
    ///
    /// # Errors
    /// The text is empty, over 128 bytes, starts with `.`, `_` or `-`, or has another
    /// character.
    pub fn new(text: &str) -> Result<RunId, LabelError> {
        let first = text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
        let rest = text
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
        if first && rest && text.len() <= 128 {
            Ok(RunId(text.to_owned()))
        } else {
            Err(LabelError {
                what: "run id",
                text: text.to_owned(),
            })
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a tree store behaves.
#[derive(Clone)]
pub struct Config {
    /// The most bytes trees may take together.
    pub budget_bytes: u64,
    /// Eviction also runs while the volume's free bytes are under this.
    pub low_water_free_bytes: u64,
    /// A tree used within this many milliseconds is not evicted, so one submitter cannot
    /// force the eviction of another's warm trees.
    pub min_age_ms: u64,
    /// The extractor version trees must have.
    pub extractor_version: u32,
    /// Trees that are never evicted.
    pub keep: BTreeSet<DiffId>,
    /// Milliseconds on a clock the caller owns; recency and the minimum age use it.
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// The free bytes of the volume the directory is on.
    pub free_space: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Asked after each step of making a tree; `true` stops the operation there as a dying
    /// process would, leaving what the step left, torn on purpose. For tests only: nothing
    /// in production sets it.
    #[doc(hidden)]
    pub crash: Option<Arc<dyn Fn(Step) -> bool + Send + Sync>>,
}

impl Default for Config {
    /// No budget, no low-water mark, no minimum age, the wall clock.
    fn default() -> Config {
        Config {
            budget_bytes: u64::MAX,
            low_water_free_bytes: 0,
            min_age_ms: 0,
            extractor_version: EXTRACTOR_VERSION,
            keep: BTreeSet::new(),
            clock: Arc::new(|| {
                let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
                since.unwrap_or_default().as_millis() as u64
            }),
            free_space: Arc::new(|| u64::MAX),
            crash: None,
        }
    }
}

/// A tree to have: its diff id, the blob it comes from and how to read it.
#[derive(Debug, Clone)]
pub struct TreeWanted {
    /// The digest of the uncompressed layer, which names the tree.
    pub diff_id: DiffId,
    /// The layer blob.
    pub blob: BlobRef,
    /// How the blob is stored.
    pub encoding: LayerEncoding,
    /// The ceilings the layer is extracted under.
    pub limits: Limits,
    /// The bytes the tree is expected to take (the layer's declared file bytes), when the
    /// caller knows them. A tree that cannot fit the budget is refused before the fetch.
    pub expected_bytes: Option<u64>,
}

/// A tree that is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The tree's diff id.
    pub diff_id: DiffId,
    /// Its directory.
    pub path: PathBuf,
}

/// A run's pin on a tree. Only [`TreeStore::pin`] makes one; [`TreeStore::unpin`] ends it.
#[derive(Debug, PartialEq, Eq)]
pub struct TreeHandle {
    diff_id: DiffId,
    run: RunId,
    path: PathBuf,
}

impl TreeHandle {
    /// The tree's directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The tree's diff id.
    pub fn diff_id(&self) -> &DiffId {
        &self.diff_id
    }
}

/// Counters, for observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    /// Trees present.
    pub trees: usize,
    /// Bytes they take.
    pub used: u64,
    /// Trees with at least one pin.
    pub pinned: usize,
    /// Extractions running.
    pub in_flight: usize,
    /// Callers waiting for an extraction of the same blob.
    pub waiting: usize,
}

/// A tree found at start: when its record was made, its id, record and live pins.
type Found = (SystemTime, DiffId, Record, BTreeSet<RunId>);

struct Entry {
    bytes: u64,
    version: u32,
    pins: BTreeSet<RunId>,
    /// Recency: higher is more recent.
    seq: u64,
    last_use_ms: u64,
    /// Being deleted: not a candidate again, not pinnable, not a hit.
    doomed: bool,
}

#[derive(Default)]
struct State {
    trees: BTreeMap<DiffId, Entry>,
    flights: BTreeSet<(DiffId, Digest)>,
    /// Trees whose placement (rename, record, table entry) is in progress.
    placing: BTreeSet<DiffId>,
    /// Bytes promised to trees that are made but not yet in the table.
    reserved: u64,
    waiting: usize,
    next_seq: u64,
}

impl State {
    fn touch(&mut self, diff_id: &DiffId, now: u64) {
        self.next_seq += 1;
        let seq = self.next_seq;
        let _ = self.trees.get_mut(diff_id).map(|e| {
            e.seq = seq;
            e.last_use_ms = now;
        });
    }
}

struct Inner<S: Sealer, F: LayerSource> {
    dir: PathBuf,
    source: F,
    sealer: S,
    cfg: Config,
    state: Mutex<State>,
    done: Condvar,
    next: AtomicU64,
    /// Held for the life of the store: a second open of the directory is refused.
    _lock: File,
}

/// Sealed trees extracted from layer blobs, within a budget.
pub struct TreeStore<S: Sealer, F: LayerSource> {
    inner: Arc<Inner<S, F>>,
}

/// A file the blob is written to, hashed as it arrives.
struct PartSink {
    file: File,
    hasher: Sha256,
    got: u64,
    cap: u64,
}

impl BlobSink for PartSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.got += data.len() as u64;
        if self.got > self.cap {
            return Err(std::io::Error::other("blob over its size cap"));
        }
        self.hasher.update(data);
        self.file.write_all(data)
    }
}

/// Bytes promised to a tree on its way into the table; given back when dropped.
struct Reservation<'a, S: Sealer, F: LayerSource> {
    inner: &'a Inner<S, F>,
    bytes: u64,
}

impl<S: Sealer, F: LayerSource> Reservation<'_, S, F> {
    /// The bytes, for a caller that turns them into a table entry under the same lock.
    fn take(&mut self) -> u64 {
        std::mem::take(&mut self.bytes)
    }
}

impl<S: Sealer, F: LayerSource> Drop for Reservation<'_, S, F> {
    fn drop(&mut self) {
        self.inner.lock_in_drop().reserved -= self.bytes;
    }
}

/// A flight in progress: whatever way its leader ends, even by a panic in the source or the
/// sealer, the key is removed and the waiters are woken to try for themselves.
struct Flight<'a, S: Sealer, F: LayerSource> {
    inner: &'a Inner<S, F>,
    key: (DiffId, Digest),
    stage: PathBuf,
    part: PathBuf,
}

impl<S: Sealer, F: LayerSource> Drop for Flight<'_, S, F> {
    fn drop(&mut self) {
        let mut state = self.inner.lock_in_drop();
        if std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.stage);
            let _ = std::fs::remove_file(&self.part);
        }
        state.flights.remove(&self.key);
        drop(state);
        self.inner.done.notify_all();
    }
}

/// A deletion in progress. If the sealer panics in the middle of it, the tree is either still
/// in place (and is usable again) or already moved out (and leaves the table), never stuck
/// as being deleted.
struct Deletion<'a, S: Sealer, F: LayerSource> {
    inner: &'a Inner<S, F>,
    diff_id: DiffId,
}

impl<S: Sealer, F: LayerSource> Drop for Deletion<'_, S, F> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let in_place = self.inner.tree_path(&self.diff_id).exists();
            let mut state = self.inner.lock_in_drop();
            if in_place {
                let _ = state.trees.get_mut(&self.diff_id).map(|e| e.doomed = false);
            } else {
                state.trees.remove(&self.diff_id);
            }
            drop(state);
            self.inner.done.notify_all();
        }
    }
}

/// A placement in progress: the diff id leaves `placing` however the placement ends. If it
/// panics after the rename and before the tree is entered in the table, the tree is moved
/// out of the way so that a retry can place it.
struct Placing<'a, S: Sealer, F: LayerSource> {
    inner: &'a Inner<S, F>,
    diff_id: DiffId,
    /// The stage was renamed to the tree's place and the table does not hold it yet.
    unentered: bool,
}

impl<S: Sealer, F: LayerSource> Drop for Placing<'_, S, F> {
    fn drop(&mut self) {
        if std::thread::panicking() & self.unentered {
            let dest = self.inner.unique("trash", self.diff_id.hex());
            let _ = std::fs::rename(self.inner.tree_path(&self.diff_id), dest);
        }
        self.inner.lock_in_drop().placing.remove(&self.diff_id);
        self.inner.done.notify_all();
    }
}

/// `path` as a directory with exactly `mode`, whatever the umask.
fn make_dir(path: &Path, mode: u32) -> Result<(), Error> {
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .and_then(|()| std::fs::set_permissions(path, Permissions::from_mode(mode)))
        .map_err(ctx("create store directory"))
}

fn sync_dir(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(ctx("sync directory"))
}

impl<S: Sealer, F: LayerSource> Inner<S, F> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("tree store state poisoned")
    }

    /// The lock for a guard's `drop`. A panic in a caller's closure (the clock, the
    /// free-space reading) while the lock is held poisons it; the guards that clean up as
    /// that panic unwinds take it anyway, since a second panic in a drop would abort the
    /// process. Everything they change is a count or a set that stays consistent.
    fn lock_in_drop(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn now(&self) -> u64 {
        (self.cfg.clock)()
    }

    fn tree_path(&self, diff_id: &DiffId) -> PathBuf {
        self.dir.join("trees").join(diff_id.hex())
    }

    fn record_path(&self, diff_id: &DiffId) -> PathBuf {
        self.dir
            .join("trees")
            .join(format!("{}.sealed", diff_id.hex()))
    }

    /// A name no other store or process uses: the process id and a counter.
    fn unique(&self, parent: &str, name: &str) -> PathBuf {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        self.dir.join(parent).join(format!("{name}-{pid}-{n}"))
    }

    fn reached(&self, step: Step) -> Result<(), Error> {
        if self.cfg.crash.as_ref().is_some_and(|stop| stop(step)) {
            Err(Error::Crashed(step))
        } else {
            Ok(())
        }
    }

    /// Clears immutability and deletes; failures leave the rest for the next start's sweep.
    fn discard(&self, path: &Path) {
        let _ = self.sealer.unseal_for_delete(path);
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
    }

    /// Removes a tree's record and pin directory.
    fn forget_files(&self, diff_id: &DiffId) {
        let _ = std::fs::remove_file(self.record_path(diff_id));
        let _ = std::fs::remove_dir_all(self.dir.join("pins").join(diff_id.hex()));
    }

    /// Moves a tree out of its place and deletes it with its record and pins. A tree that is
    /// already gone counts as moved. A tree that will not move is unsealed (an immutable
    /// directory may refuse to move) and moved again; if it still will not move, or could
    /// not be unsealed, it is sealed again, so that a tree that stays in place stays
    /// immutable, and the error says so if that fails too. Once the move has happened what
    /// is left of the deletion is for the next start's sweep.
    fn trash_tree(&self, diff_id: &DiffId) -> Result<(), Error> {
        let tree = self.tree_path(diff_id);
        let dest = self.unique("trash", diff_id.hex());
        if std::fs::rename(&tree, &dest).is_err() {
            if !tree.exists() {
                self.forget_files(diff_id);
                return Ok(());
            }
            self.sealer
                .unseal_for_delete(&tree)
                .map_err(Error::Seal)
                .and_then(|()| {
                    std::fs::rename(&tree, &dest).map_err(|source| Error::Io {
                        what: "move tree to trash",
                        source,
                    })
                })
                .map_err(|error| self.seal_again(&tree, error))?;
        }
        self.forget_files(diff_id);
        self.discard(&dest);
        Ok(())
    }

    /// Puts the seal back on a tree that stays in place after `error`: that error, or the
    /// two if the seal fails too.
    fn seal_again(&self, tree: &Path, error: Error) -> Error {
        match self.sealer.seal(tree) {
            Ok(_) => error,
            Err(again) => Error::Cleanup {
                cause: Box::new(error),
                cleanup: Box::new(Error::Seal(again)),
            },
        }
    }

    /// Deletes a tree and, if that succeeds, takes it out of the table.
    fn delete_tree(&self, diff_id: &DiffId) -> Result<(), Error> {
        let _deletion = Deletion {
            inner: self,
            diff_id: diff_id.clone(),
        };
        let outcome = self.trash_tree(diff_id);
        self.finish_trash(diff_id, outcome)
    }

    /// Ends a deletion: the entry leaves the table only if the tree is gone, and stays
    /// counted otherwise.
    fn finish_trash(&self, diff_id: &DiffId, outcome: Result<(), Error>) -> Result<(), Error> {
        let mut state = self.lock();
        match &outcome {
            Ok(()) => {
                state.trees.remove(diff_id);
            }
            Err(_) => {
                let _ = state.trees.get_mut(diff_id).map(|e| e.doomed = false);
            }
        }
        drop(state);
        self.done.notify_all();
        outcome
    }

    /// Makes room for `incoming` more bytes and reserves them, evicting, least recently used
    /// first, until they fit the budget and the volume is over the low-water mark. Trees that
    /// are pinned, kept, used within the minimum age or already being deleted are not
    /// candidates. Bytes reserved by trees on their way in count as used.
    fn make_room(&self, incoming: u64) -> Result<Reservation<'_, S, F>, Error> {
        loop {
            let mut state = self.lock();
            let used = state.trees.values().map(|e| e.bytes).sum::<u64>() + state.reserved;
            let over_budget = used.saturating_add(incoming) > self.cfg.budget_bytes;
            let over_free = (self.cfg.free_space)() < self.cfg.low_water_free_bytes;
            if !(over_budget || over_free) {
                state.reserved += incoming;
                return Ok(Reservation {
                    inner: self,
                    bytes: incoming,
                });
            }
            let now = self.now();
            let victim = state
                .trees
                .iter()
                .filter(|(id, e)| {
                    e.pins.is_empty()
                        && !e.doomed
                        && !self.cfg.keep.contains(*id)
                        && now.saturating_sub(e.last_use_ms) >= self.cfg.min_age_ms
                })
                .min_by_key(|(_, e)| e.seq)
                .map(|(id, _)| id.clone());
            let Some(victim) = victim else {
                return Err(Error::NoSpace {
                    needed: incoming,
                    used,
                    budget: self.cfg.budget_bytes,
                });
            };
            let _ = state.trees.get_mut(&victim).map(|e| e.doomed = true);
            drop(state);
            self.delete_tree(&victim)?;
        }
    }

    /// Adds `run`'s pin to a present, current tree; the caller holds the state lock.
    fn pin_locked(
        &self,
        state: &mut State,
        diff_id: &DiffId,
        run: &RunId,
    ) -> Result<TreeHandle, Error> {
        let version = self.cfg.extractor_version;
        let entry = state
            .trees
            .get_mut(diff_id)
            .filter(|e| (e.version == version) & !e.doomed)
            .ok_or_else(|| Error::Absent(diff_id.as_str().to_owned()))?;
        if entry.pins.contains(run) {
            return Err(Error::AlreadyPinned);
        }
        let dir = self.dir.join("pins").join(diff_id.hex());
        make_dir(&dir, 0o700)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(run.as_str()))
            .map_err(ctx("create pin"))?;
        file.sync_all().map_err(ctx("sync pin"))?;
        sync_dir(&dir)?;
        entry.pins.insert(run.clone());
        state.touch(diff_id, self.now());
        Ok(TreeHandle {
            diff_id: diff_id.clone(),
            run: run.clone(),
            path: self.tree_path(diff_id),
        })
    }

    fn write_record(&self, diff_id: &DiffId, record: &Record) -> Result<(), Error> {
        let final_path = self.record_path(diff_id);
        let temp = self.unique("tmp", "record");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(ctx("create record"))?;
        file.write_all(&record.to_bytes())
            .map_err(ctx("write record"))?;
        file.sync_all().map_err(ctx("sync record"))?;
        std::fs::rename(&temp, &final_path).map_err(ctx("place record"))?;
        sync_dir(&self.dir.join("trees"))
    }

    /// Fetches the blob into `part` and checks that it is exactly the blob asked for.
    /// Nothing reads the file before this returns.
    fn fetch(&self, blob: &BlobRef, part: &Path) -> Result<File, Error> {
        let mut sink = PartSink {
            file: OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(part)
                .map_err(ctx("create blob file"))?,
            hasher: Sha256::new(),
            got: 0,
            cap: blob.size,
        };
        self.source.fetch(blob, &mut sink)?;
        sink.file.sync_all().map_err(ctx("sync blob file"))?;
        let got = format!("sha256:{}", hex::encode(sink.hasher.finalize()));
        if sink.got != blob.size || got != blob.digest.as_str() {
            return Err(Error::Blob {
                digest: blob.digest.to_string(),
                got,
                got_size: sink.got,
            });
        }
        File::open(part).map_err(ctx("open blob file"))
    }

    /// Places the sealed stage as the tree, records it and enters it in the table, one
    /// placement of a diff id at a time. A tree already in place is accepted only when this
    /// store holds it as current (another extraction of the same layer won); anything else
    /// there is not ours to take for the new content.
    fn commit(
        &self,
        want: &TreeWanted,
        stage: &Path,
        sealed_bytes: u64,
        reservation: &mut Reservation<'_, S, F>,
        run: Option<&RunId>,
    ) -> Result<Option<TreeHandle>, Error> {
        let diff_id = &want.diff_id;
        let mut state = self.lock();
        while state.placing.contains(diff_id) {
            state.waiting += 1;
            state = self.done.wait(state).expect("tree store state poisoned");
            state.waiting -= 1;
        }
        state.placing.insert(diff_id.clone());
        drop(state);
        let mut placing = Placing {
            inner: self,
            diff_id: diff_id.clone(),
            unentered: false,
        };
        self.place(want, stage, sealed_bytes, reservation, run, &mut placing)
    }

    /// The steps between the rename and the table entry. A failure other than a simulated
    /// death leaves nothing in place.
    fn record_placed(&self, want: &TreeWanted, sealed_bytes: u64) -> Result<(), Error> {
        let diff_id = &want.diff_id;
        sync_dir(&self.dir.join("trees"))?;
        self.reached(Step::Renamed)?;
        let record = Record {
            diff_id: diff_id.as_str().to_owned(),
            blob: want.blob.digest.to_string(),
            extractor_version: self.cfg.extractor_version,
            bytes: sealed_bytes,
        };
        self.write_record(diff_id, &record)?;
        self.reached(Step::Recorded)
    }

    fn place(
        &self,
        want: &TreeWanted,
        stage: &Path,
        sealed_bytes: u64,
        reservation: &mut Reservation<'_, S, F>,
        run: Option<&RunId>,
        placing: &mut Placing<'_, S, F>,
    ) -> Result<Option<TreeHandle>, Error> {
        let diff_id = &want.diff_id;
        let version = self.cfg.extractor_version;
        match std::fs::rename(stage, self.tree_path(diff_id)) {
            Ok(()) => {
                placing.unentered = true;
                match self.record_placed(want, sealed_bytes) {
                    Ok(()) => {}
                    Err(Error::Crashed(step)) => return Err(Error::Crashed(step)),
                    Err(error) => {
                        // Out of the way, with its record, so that a retry can place it.
                        return Err(match self.trash_tree(diff_id) {
                            Ok(()) => error,
                            Err(cleanup) => Error::Cleanup {
                                cause: Box::new(error),
                                cleanup: Box::new(cleanup),
                            },
                        });
                    }
                }
                let mut state = self.lock();
                placing.unentered = false;
                state.reserved -= reservation.take();
                state.next_seq += 1;
                let seq = state.next_seq;
                let last_use_ms = self.now();
                state.trees.insert(
                    diff_id.clone(),
                    Entry {
                        bytes: sealed_bytes,
                        version,
                        pins: BTreeSet::new(),
                        seq,
                        last_use_ms,
                        doomed: false,
                    },
                );
                run.map(|run| self.pin_locked(&mut state, diff_id, run))
                    .transpose()
            }
            Err(_) if self.tree_path(diff_id).exists() => {
                let mut state = self.lock();
                let ours = state
                    .trees
                    .get(diff_id)
                    .is_some_and(|e| (e.version == version) & !e.doomed);
                if !ours {
                    return Err(Error::Conflict(diff_id.as_str().to_owned()));
                }
                let pinned = run
                    .map(|run| self.pin_locked(&mut state, diff_id, run))
                    .transpose();
                drop(state);
                self.discard(stage);
                pinned
            }
            Err(source) => Err(Error::Io {
                what: "place tree",
                source,
            }),
        }
    }

    fn build(
        &self,
        want: &TreeWanted,
        part: &Path,
        stage: &Path,
        run: Option<&RunId>,
    ) -> Result<Option<TreeHandle>, Error> {
        let blob = self.fetch(&want.blob, part)?;
        self.reached(Step::Fetched)?;
        make_dir(stage, 0o700)?;
        self.reached(Step::Staged)?;
        let root = File::open(stage).map_err(ctx("open stage"))?;
        unpack_layer(
            blob,
            want.encoding,
            root.as_fd(),
            &want.diff_id,
            &want.limits,
        )?;
        self.reached(Step::Unpacked)?;
        let sealed = self.sealer.seal(stage)?;
        self.reached(Step::Sealed)?;
        let mut reservation = self.make_room(sealed.bytes)?;
        self.commit(want, stage, sealed.bytes, &mut reservation, run)
    }

    /// Replaces a stale tree: it is deleted, and stays in the table (counted, stale) if
    /// the deletion fails.
    fn replace_stale(&self, want: &TreeWanted, stale: bool) -> Result<(), Error> {
        if stale {
            self.delete_tree(&want.diff_id)
        } else {
            Ok(())
        }
    }

    /// A tree that cannot fit the budget at all is refused before anything is fetched.
    fn check_fits(&self, state: &State, want: &TreeWanted) -> Result<(), Error> {
        let expected = want.expected_bytes.unwrap_or(0);
        if expected > self.cfg.budget_bytes {
            return Err(Error::NoSpace {
                needed: expected,
                used: state.trees.values().map(|e| e.bytes).sum(),
                budget: self.cfg.budget_bytes,
            });
        }
        Ok(())
    }

    /// The flight's leader: replaces a stale tree, builds, and cleans up after a failure.
    fn lead(
        &self,
        want: &TreeWanted,
        stale: bool,
        run: Option<&RunId>,
        stage: &Path,
        part: &Path,
    ) -> Result<Option<TreeHandle>, Error> {
        let built = self
            .replace_stale(want, stale)
            .and_then(|()| self.build(want, part, stage, run));
        if matches!(built, Err(Error::Crashed(_))) {
            return built;
        }
        if stage.exists() {
            self.discard(stage);
        }
        let _ = std::fs::remove_file(part);
        built
    }

    /// One name under `trees/` at start, with the record's age for the recency order.
    fn sweep_one(&self, name: &str, live: &BTreeSet<RunId>) -> Result<Option<Found>, Error> {
        let path = self.dir.join("trees").join(name);
        let Some(hex) = name.strip_suffix(".sealed") else {
            return self.sweep_tree(name, live);
        };
        // A record is kept for its tree, which is handled under the tree's own name.
        if !self.dir.join("trees").join(hex).is_dir() {
            let _ = std::fs::remove_file(path);
        }
        Ok(None)
    }

    fn sweep_tree(&self, name: &str, live: &BTreeSet<RunId>) -> Result<Option<Found>, Error> {
        let path = self.dir.join("trees").join(name);
        let diff_id = DiffId::parse(&format!("sha256:{name}"));
        let record = diff_id
            .as_ref()
            .ok()
            .and_then(|id| Record::read(&self.record_path(id), id.as_str()));
        let (Ok(diff_id), Some(record)) = (diff_id, record) else {
            // Junk, a torn tree (renamed, never recorded) or a record that does not parse:
            // refetching is cheaper than proving it complete.
            let _ = self.sealer.unseal_for_delete(&path);
            let dest = self.unique("trash", name);
            std::fs::rename(&path, &dest).map_err(ctx("move torn tree to trash"))?;
            let _ = std::fs::remove_file(self.dir.join("trees").join(format!("{name}.sealed")));
            self.discard(&dest);
            return Ok(None);
        };
        let pin_dir = self.dir.join("pins").join(diff_id.hex());
        let mut pins = BTreeSet::new();
        for pin in names(&pin_dir).unwrap_or_default() {
            match RunId::new(&pin) {
                Ok(run) if live.contains(&run) => {
                    pins.insert(run);
                }
                _ => {
                    let _ = std::fs::remove_file(pin_dir.join(&pin));
                }
            }
        }
        let made = std::fs::metadata(self.record_path(&diff_id))
            .and_then(|meta| meta.modified())
            .map_err(ctx("read record time"))?;
        Ok(Some((made, diff_id, record, pins)))
    }
}

fn names(dir: &Path) -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(ctx("read directory"))? {
        let entry = entry.map_err(ctx("read directory entry"))?;
        found.push(entry.file_name().to_string_lossy().into_owned());
    }
    found.sort();
    Ok(found)
}

impl<S: Sealer, F: LayerSource> TreeStore<S, F> {
    /// Opens the directory, which one store at a time may hold (a second open is refused
    /// with [`Error::Locked`]), and sweeps what a crash left: `tmp/`, `stage/` and `trash/`
    /// are emptied, a tree without a valid record is deleted, pins of runs not in `live` and
    /// pin directories of trees that are gone are dropped.
    ///
    /// Trees found are ordered by the age of their record (when it was made, not when the
    /// tree was last used) and all count as just used, so none is evictable for the minimum
    /// age after a start. Until then a store over its budget, or started with a smaller one,
    /// answers [`Error::NoSpace`] to a new tree rather than evict.
    ///
    /// # Errors
    /// A directory cannot be made or read, or the directory is held by another store.
    pub fn open(
        dir: &Path,
        source: F,
        sealer: S,
        cfg: Config,
        live: &BTreeSet<RunId>,
    ) -> Result<TreeStore<S, F>, Error> {
        std::fs::create_dir_all(dir).map_err(ctx("create store directory"))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("lock"))
            .map_err(ctx("open lock file"))?;
        if lock.try_lock().is_err() {
            return Err(Error::Locked(dir.display().to_string()));
        }
        make_dir(&dir.join("trees"), 0o711)?;
        for sub in ["tmp", "stage", "trash", "pins"] {
            make_dir(&dir.join(sub), 0o700)?;
        }
        let inner = Inner {
            dir: dir.to_owned(),
            source,
            sealer,
            cfg,
            state: Mutex::new(State::default()),
            done: Condvar::new(),
            next: AtomicU64::new(0),
            _lock: lock,
        };
        for scratch in ["tmp", "stage", "trash"] {
            for name in names(&dir.join(scratch))? {
                inner.discard(&dir.join(scratch).join(name));
            }
        }
        let mut found = Vec::new();
        for name in names(&dir.join("trees"))? {
            found.extend(inner.sweep_one(&name, live)?);
        }
        found.sort_by_key(|(made, ..)| *made);
        let now = inner.now();
        let mut kept = BTreeSet::new();
        {
            let mut state = inner.lock();
            for (_, diff_id, record, pins) in found {
                state.next_seq += 1;
                let seq = state.next_seq;
                kept.insert(diff_id.hex().to_owned());
                state.trees.insert(
                    diff_id,
                    Entry {
                        bytes: record.bytes,
                        version: record.extractor_version,
                        pins,
                        seq,
                        last_use_ms: now,
                        doomed: false,
                    },
                );
            }
        }
        for name in names(&dir.join("pins"))? {
            if !kept.contains(&name) {
                let _ = std::fs::remove_dir_all(dir.join("pins").join(name));
            }
        }
        Ok(TreeStore {
            inner: Arc::new(inner),
        })
    }

    /// The tree for `want`, extracted from its blob if it is not present, or was made by an
    /// older extractor. Callers for the same (diff id, blob) share one extraction; a caller
    /// whose extraction failed fails alone, and a waiting caller then tries for itself.
    ///
    /// Trees are named by their diff id alone: a tree that is present answers whatever blob
    /// digest and limits the caller gives, as the content is the same by definition of the
    /// diff id. The path is not pinned; use [`TreeStore::ensure_pinned`] to use it.
    ///
    /// # Errors
    /// The source fails, the blob is not the one asked for, the layer breaks a rule or is
    /// not the one the diff id names, sealing or a file-system call fails, no room can be
    /// made ([`Error::NoSpace`]), the tree is stale and pinned ([`Error::Busy`]), or a tree
    /// this store does not hold is in the way ([`Error::Conflict`]).
    pub fn ensure(&self, want: &TreeWanted) -> Result<Ready, Error> {
        self.run_ensure(want, None)?;
        Ok(self.ready(&want.diff_id))
    }

    /// As [`TreeStore::ensure`], and pins the tree for `run` in the same step, so it cannot
    /// be evicted between the two.
    ///
    /// # Errors
    /// As [`TreeStore::ensure`], and [`Error::AlreadyPinned`] when the run pins it.
    pub fn ensure_pinned(&self, want: &TreeWanted, run: &RunId) -> Result<TreeHandle, Error> {
        let handle = self.run_ensure(want, Some(run))?;
        Ok(handle.expect("a pin was asked for"))
    }

    fn run_ensure(
        &self,
        want: &TreeWanted,
        run: Option<&RunId>,
    ) -> Result<Option<TreeHandle>, Error> {
        let inner = &self.inner;
        let key = (want.diff_id.clone(), want.blob.digest.clone());
        let stale = loop {
            let mut state = inner.lock();
            let version = inner.cfg.extractor_version;
            let (current, pinned, doomed) = state
                .trees
                .get(&want.diff_id)
                .map_or((false, false, false), |e| {
                    (e.version == version, !e.pins.is_empty(), e.doomed)
                });
            if current & !doomed {
                state.touch(&want.diff_id, inner.now());
                return run
                    .map(|run| inner.pin_locked(&mut state, &want.diff_id, run))
                    .transpose();
            }
            if pinned {
                return Err(Error::Busy);
            }
            if doomed | state.flights.contains(&key) {
                state.waiting += 1;
                let mut state = inner.done.wait(state).expect("tree store state poisoned");
                state.waiting -= 1;
                continue;
            }
            inner.check_fits(&state, want)?;
            state.flights.insert(key.clone());
            // A stale tree stays in the table, counted, until its deletion succeeds.
            break state
                .trees
                .get_mut(&want.diff_id)
                .map(|e| e.doomed = true)
                .is_some();
        };
        let stage = inner.unique("stage", want.diff_id.hex());
        let part = inner.unique("tmp", "part");
        let _flight = Flight {
            inner,
            key,
            stage: stage.clone(),
            part: part.clone(),
        };
        inner.lead(want, stale, run, &stage, &part)
    }

    fn ready(&self, diff_id: &DiffId) -> Ready {
        Ready {
            diff_id: diff_id.clone(),
            path: self.inner.tree_path(diff_id),
        }
    }

    /// Evicts until the budget and the low-water mark hold, as far as it can.
    ///
    /// # Errors
    /// [`Error::NoSpace`] when what is left is pinned, kept or too recently used, or an
    /// eviction fails.
    pub fn trim(&self) -> Result<(), Error> {
        self.inner.make_room(0).map(|_| ())
    }

    /// Pins a present tree for a run, durably, before its path is given to the run.
    ///
    /// # Errors
    /// [`Error::Absent`] when the tree is not present, is stale or is being deleted,
    /// [`Error::AlreadyPinned`] when the run pins it, or the pin file cannot be made.
    pub fn pin(&self, diff_id: &DiffId, run: &RunId) -> Result<TreeHandle, Error> {
        let mut state = self.inner.lock();
        self.inner.pin_locked(&mut state, diff_id, run)
    }

    /// Ends a run's pin.
    pub fn unpin(&self, tree: TreeHandle) {
        let mut state = self.inner.lock();
        let _ = state
            .trees
            .get_mut(&tree.diff_id)
            .map(|entry| entry.pins.remove(&tree.run));
        let pin = self
            .inner
            .dir
            .join("pins")
            .join(tree.diff_id.hex())
            .join(tree.run.as_str());
        let _ = std::fs::remove_file(pin);
    }

    /// Counters.
    pub fn stats(&self) -> TreeStats {
        let state = self.inner.lock();
        TreeStats {
            trees: state.trees.len(),
            used: state.trees.values().map(|e| e.bytes).sum(),
            pinned: state.trees.values().filter(|e| !e.pins.is_empty()).count(),
            in_flight: state.flights.len(),
            waiting: state.waiting,
        }
    }
}
