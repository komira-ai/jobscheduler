//! [`TreeStore`]: ensure, pin, evict and sweep.

use std::collections::btree_map::Entry as MapEntry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use js_bundle::media::LayerEncoding;
use js_bundle::{DiffId, Limits, unpack_layer};

use crate::blobstore::{
    AccountHandle, BlobKey, BlobStore, Busy, Holder, LabelError, Release, Wanted,
};
use crate::error::{Error, Step, ctx};
use crate::record::Record;
use crate::sealer::Sealer;

/// The version of the extractor's rules. Tightening a rule bumps it, and trees made under
/// an older version are extracted again.
pub const EXTRACTOR_VERSION: u32 = 1;

/// The priority trees are registered with. Blobs are 0, so under pressure a blob goes
/// before a tree made from it.
pub const TREE_PRIORITY: u8 = 10;

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
    /// The priority trees are registered with.
    pub priority: u8,
    /// The extractor version trees must have.
    pub extractor_version: u32,
    /// Trees that are never released.
    pub keep: BTreeSet<DiffId>,
    /// Asked after each step of making a tree; `true` stops the operation there as a dying
    /// process would, leaving what the step left.
    pub crash: Option<Arc<dyn Fn(Step) -> bool + Send + Sync>>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            priority: TREE_PRIORITY,
            extractor_version: EXTRACTOR_VERSION,
            keep: BTreeSet::new(),
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
    pub blob: Wanted,
    /// How the blob is stored.
    pub encoding: LayerEncoding,
    /// The ceilings the layer is extracted under.
    pub limits: Limits,
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
    /// Trees with at least one pin.
    pub pinned: usize,
    /// Extractions running.
    pub in_flight: usize,
    /// Callers waiting for an extraction of the same blob.
    pub waiting: usize,
}

struct Entry {
    bytes: u64,
    version: u32,
    pins: BTreeSet<RunId>,
    handle: Arc<dyn AccountHandle>,
}

#[derive(Default)]
struct State {
    trees: BTreeMap<DiffId, Entry>,
    flights: BTreeSet<(DiffId, BlobKey)>,
    waiting: usize,
}

struct Inner<S: Sealer, B: BlobStore> {
    dir: PathBuf,
    blobs: Arc<B>,
    sealer: S,
    cfg: Config,
    state: Mutex<State>,
    done: Condvar,
    next: AtomicU64,
}

/// Sealed trees extracted from layer blobs, registered in the blob store's budget.
pub struct TreeStore<S: Sealer, B: BlobStore> {
    inner: Arc<Inner<S, B>>,
}

/// What the blob store calls to take a tree's bytes back.
struct Reclaim<S: Sealer, B: BlobStore> {
    inner: Weak<Inner<S, B>>,
    diff_id: DiffId,
}

impl<S: Sealer, B: BlobStore> Release for Reclaim<S, B> {
    fn release(&self) -> Result<u64, Busy> {
        match self.inner.upgrade() {
            Some(inner) => inner.reclaim(&self.diff_id),
            None => Ok(0),
        }
    }
}

fn short(text: &str) -> String {
    text.bytes()
        .filter(u8::is_ascii_alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

impl<S: Sealer, B: BlobStore> Inner<S, B> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("tree store state poisoned")
    }

    fn tree_path(&self, diff_id: &DiffId) -> PathBuf {
        self.dir.join("trees").join(diff_id.hex())
    }

    fn record_path(&self, diff_id: &DiffId) -> PathBuf {
        self.dir
            .join("trees")
            .join(format!("{}.sealed", diff_id.hex()))
    }

    fn unique(&self, parent: &str, hex: &str) -> PathBuf {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        self.dir.join(parent).join(format!("{hex}-{n}"))
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
        let _ = self.sealer.unseal(path);
        let _ = std::fs::remove_dir_all(path).or_else(|_| std::fs::remove_file(path));
    }

    /// Moves a tree out of its place and deletes it with its record and pins.
    fn trash_tree(&self, diff_id: &DiffId) -> Result<(), Error> {
        let dest = self.unique("trash", diff_id.hex());
        std::fs::rename(self.tree_path(diff_id), &dest).map_err(ctx("move tree to trash"))?;
        let _ = std::fs::remove_file(self.record_path(diff_id));
        let _ = std::fs::remove_dir_all(self.dir.join("pins").join(diff_id.hex()));
        self.discard(&dest);
        Ok(())
    }

    fn reclaim(&self, diff_id: &DiffId) -> Result<u64, Busy> {
        let mut state = self.lock();
        let entry = match state.trees.entry(diff_id.clone()) {
            MapEntry::Vacant(_) => return Err(Busy),
            MapEntry::Occupied(held)
                if !held.get().pins.is_empty() || self.cfg.keep.contains(diff_id) =>
            {
                return Err(Busy);
            }
            MapEntry::Occupied(held) => held.remove(),
        };
        drop(state);
        match self.trash_tree(diff_id) {
            Ok(()) => Ok(entry.bytes),
            Err(_) => {
                self.lock().trees.insert(diff_id.clone(), entry);
                Err(Busy)
            }
        }
    }
}

impl<S: Sealer, B: BlobStore> Inner<S, B> {
    /// Registers a tree's bytes, unless a racing extraction of the same tree did first.
    fn register(
        self: &Arc<Self>,
        diff_id: &DiffId,
        bytes: u64,
        version: u32,
        pins: BTreeSet<RunId>,
    ) -> Result<(), Error> {
        let owner = Holder::new("treestore")?;
        let release = Arc::new(Reclaim {
            inner: Arc::downgrade(self),
            diff_id: diff_id.clone(),
        });
        let handle = self
            .blobs
            .account(&owner, "tree", bytes, self.cfg.priority, release)?;
        match self.lock().trees.entry(diff_id.clone()) {
            MapEntry::Vacant(slot) => {
                slot.insert(Entry {
                    bytes,
                    version,
                    pins,
                    handle,
                });
            }
            MapEntry::Occupied(_) => handle.drop_entry(),
        }
        Ok(())
    }

    fn write_record(&self, diff_id: &DiffId, record: &Record) -> Result<(), Error> {
        let final_path = self.record_path(diff_id);
        let temp = final_path.with_extension("tmp");
        let mut file = File::create(&temp).map_err(ctx("create record"))?;
        file.write_all(&record.to_bytes())
            .map_err(ctx("write record"))?;
        file.sync_all().map_err(ctx("sync record"))?;
        std::fs::rename(&temp, &final_path).map_err(ctx("place record"))?;
        File::open(self.dir.join("trees"))
            .and_then(|dir| dir.sync_all())
            .map_err(ctx("sync trees"))
    }

    /// Renames the stage to the tree's place. A tree already there (another extraction of
    /// the same diff id won) is the same content: the stage is discarded.
    fn place(&self, stage: &Path, diff_id: &DiffId) -> Result<(), Error> {
        match std::fs::rename(stage, self.tree_path(diff_id)) {
            Ok(()) => Ok(()),
            Err(_) if self.tree_path(diff_id).exists() => {
                self.discard(stage);
                Ok(())
            }
            Err(source) => Err(Error::Io {
                what: "place tree",
                source,
            }),
        }
    }

    fn build(
        self: &Arc<Self>,
        want: &TreeWanted,
        holder: &Holder,
        stage: &Path,
    ) -> Result<(), Error> {
        let pin = self.blobs.ensure(&want.blob, holder)?;
        self.reached(Step::Pinned)?;
        let blob = self.blobs.open_verified(&pin)?;
        std::fs::create_dir(stage).map_err(ctx("create stage"))?;
        self.reached(Step::Staged)?;
        let root = File::open(stage).map_err(ctx("open stage"))?;
        unpack_layer(
            File::from(blob.fd),
            want.encoding,
            root.as_fd(),
            &want.diff_id,
            &want.limits,
        )?;
        self.reached(Step::Unpacked)?;
        let sealed = self.sealer.seal(stage)?;
        self.reached(Step::Sealed)?;
        self.place(stage, &want.diff_id)?;
        self.reached(Step::Renamed)?;
        let record = Record {
            diff_id: want.diff_id.as_str().to_owned(),
            blob: want.blob.key.to_string(),
            extractor_version: self.cfg.extractor_version,
            bytes: sealed.bytes,
        };
        self.write_record(&want.diff_id, &record)?;
        self.reached(Step::Recorded)?;
        self.register(
            &want.diff_id,
            sealed.bytes,
            self.cfg.extractor_version,
            BTreeSet::new(),
        )?;
        self.reached(Step::Registered)
    }

    /// The flight's leader: replaces a stale tree, builds, and lets go of the blob pin.
    fn lead(self: &Arc<Self>, want: &TreeWanted, stale: Option<Entry>) -> Result<(), Error> {
        let hex = want.diff_id.hex();
        let holder = Holder::new(&format!("ts.{}.{}", short(hex), short(&want.blob.key.hex)))?;
        let stage = self.unique("stage", hex);
        let built = match stale {
            Some(old) => {
                old.handle.drop_entry();
                self.trash_tree(&want.diff_id)
                    .and_then(|()| self.build(want, &holder, &stage))
            }
            None => self.build(want, &holder, &stage),
        };
        if matches!(built, Err(Error::Crashed(_))) {
            return built;
        }
        if stage.exists() {
            self.discard(&stage);
        }
        let _ = self.blobs.release(&holder);
        built
    }
}

impl<S: Sealer, B: BlobStore> TreeStore<S, B> {
    /// Opens the directory, sweeps what a crash left and registers the bytes of the trees
    /// that are there. Call the blob store's `reap_pins` and `start_gc` after it.
    ///
    /// # Errors
    /// A directory cannot be made or read, or the blob store refuses a registration.
    pub fn open(
        dir: &Path,
        blobs: Arc<B>,
        sealer: S,
        cfg: Config,
        live: &BTreeSet<RunId>,
    ) -> Result<TreeStore<S, B>, Error> {
        for sub in ["trees", "stage", "trash", "pins"] {
            std::fs::create_dir_all(dir.join(sub)).map_err(ctx("create store directory"))?;
        }
        let inner = Arc::new(Inner {
            dir: dir.to_owned(),
            blobs,
            sealer,
            cfg,
            state: Mutex::new(State::default()),
            done: Condvar::new(),
            next: AtomicU64::new(0),
        });
        for scratch in ["stage", "trash"] {
            for path in names(&dir.join(scratch))? {
                inner.discard(&dir.join(scratch).join(path));
            }
        }
        for name in names(&dir.join("trees"))? {
            inner.sweep_one(&name, live)?;
        }
        Ok(TreeStore { inner })
    }

    /// The tree for `want`, extracted from its blob if it is not present, or was made by an
    /// older extractor. Callers for the same (diff id, blob) share one extraction; a caller
    /// whose extraction failed fails alone, and a waiting caller then tries for itself.
    ///
    /// # Errors
    /// The blob store refuses, the layer breaks a rule or is not the one the diff id names,
    /// sealing or a file-system call fails, or the tree is stale and pinned ([`Error::Busy`]).
    pub fn ensure(&self, want: &TreeWanted) -> Result<Ready, Error> {
        let inner = &self.inner;
        let key = (want.diff_id.clone(), want.blob.key.clone());
        let stale = loop {
            let mut state = inner.lock();
            let hit = state.trees.get(&want.diff_id);
            if let Some(entry) = hit.filter(|e| e.version == inner.cfg.extractor_version) {
                let handle = entry.handle.clone();
                drop(state);
                handle.touch();
                return Ok(self.ready(&want.diff_id));
            }
            if hit.is_some_and(|e| !e.pins.is_empty()) {
                return Err(Error::Busy);
            }
            if state.flights.contains(&key) {
                state.waiting += 1;
                let mut state = inner.done.wait(state).expect("tree store state poisoned");
                state.waiting -= 1;
                continue;
            }
            state.flights.insert(key.clone());
            break state.trees.remove(&want.diff_id);
        };
        let result = inner.lead(want, stale);
        inner.lock().flights.remove(&key);
        inner.done.notify_all();
        result.map(|()| self.ready(&want.diff_id))
    }

    fn ready(&self, diff_id: &DiffId) -> Ready {
        Ready {
            diff_id: diff_id.clone(),
            path: self.inner.tree_path(diff_id),
        }
    }

    /// Pins a present tree for a run, durably, before its path is given to the run.
    ///
    /// # Errors
    /// [`Error::Absent`] when the tree is not present or is stale, [`Error::AlreadyPinned`]
    /// when the run pins it, or the pin file cannot be made.
    pub fn pin(&self, diff_id: &DiffId, run: &RunId) -> Result<TreeHandle, Error> {
        let inner = &self.inner;
        let mut state = inner.lock();
        let entry = state
            .trees
            .get_mut(diff_id)
            .filter(|e| e.version == inner.cfg.extractor_version)
            .ok_or_else(|| Error::Absent(diff_id.as_str().to_owned()))?;
        if entry.pins.contains(run) {
            return Err(Error::AlreadyPinned);
        }
        let dir = inner.dir.join("pins").join(diff_id.hex());
        std::fs::create_dir_all(&dir).map_err(ctx("create pin directory"))?;
        File::create_new(dir.join(run.as_str())).map_err(ctx("create pin"))?;
        entry.pins.insert(run.clone());
        let handle = entry.handle.clone();
        drop(state);
        handle.touch();
        Ok(TreeHandle {
            diff_id: diff_id.clone(),
            run: run.clone(),
            path: inner.tree_path(diff_id),
        })
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
            pinned: state.trees.values().filter(|e| !e.pins.is_empty()).count(),
            in_flight: state.flights.len(),
            waiting: state.waiting,
        }
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

impl<S: Sealer, B: BlobStore> Inner<S, B> {
    /// One name under `trees/` at start.
    fn sweep_one(self: &Arc<Self>, name: &str, live: &BTreeSet<RunId>) -> Result<(), Error> {
        let path = self.dir.join("trees").join(name);
        let Some(hex) = name.strip_suffix(".sealed") else {
            return self.sweep_tree(name, live);
        };
        // A record is kept for its tree, which is handled under the tree's own name.
        if !self.dir.join("trees").join(hex).is_dir() {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    fn sweep_tree(self: &Arc<Self>, name: &str, live: &BTreeSet<RunId>) -> Result<(), Error> {
        let path = self.dir.join("trees").join(name);
        let diff_id = DiffId::parse(&format!("sha256:{name}"));
        let record = diff_id
            .as_ref()
            .ok()
            .and_then(|id| Record::read(&self.record_path(id), id.as_str()));
        let (Ok(diff_id), Some(record)) = (diff_id, record) else {
            // Junk, a torn tree (renamed, never recorded) or a record that does not parse:
            // refetching is cheaper than proving it complete.
            let dest = self.unique("trash", name);
            std::fs::rename(&path, &dest).map_err(ctx("move torn tree to trash"))?;
            let _ = std::fs::remove_file(self.dir.join("trees").join(format!("{name}.sealed")));
            self.discard(&dest);
            return Ok(());
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
        self.register(&diff_id, record.bytes, record.extractor_version, pins)
    }
}
