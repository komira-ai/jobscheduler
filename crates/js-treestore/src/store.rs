//! [`TreeStore`]: ensure, pin, evict and sweep.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd as _;
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
    /// process would, leaving what the step left.
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
}

#[derive(Default)]
struct State {
    trees: BTreeMap<DiffId, Entry>,
    flights: BTreeSet<(DiffId, Digest)>,
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

impl<S: Sealer, F: LayerSource> Inner<S, F> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("tree store state poisoned")
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

    fn unique(&self, parent: &str, name: &str) -> PathBuf {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        self.dir.join(parent).join(format!("{name}-{n}"))
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

    /// Evicts, least recently used first, until `incoming` more bytes fit the budget and
    /// the volume is over the low-water mark. A tree that is pinned, kept or used within the
    /// minimum age is not a candidate.
    fn make_room(&self, incoming: u64) -> Result<(), Error> {
        loop {
            let mut state = self.lock();
            let used: u64 = state.trees.values().map(|e| e.bytes).sum();
            let over_budget = used.saturating_add(incoming) > self.cfg.budget_bytes;
            let over_free = (self.cfg.free_space)() < self.cfg.low_water_free_bytes;
            if !(over_budget || over_free) {
                return Ok(());
            }
            let now = self.now();
            let victim = state
                .trees
                .iter()
                .filter(|(id, e)| {
                    e.pins.is_empty()
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
            state.trees.remove(&victim);
            drop(state);
            // A tree that cannot be moved is out of the table either way; the next start's
            // sweep deals with what is left.
            let _ = self.trash_tree(&victim);
        }
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

    /// Fetches the blob into `part` and checks that it is exactly the blob asked for.
    /// Nothing reads the file before this returns.
    fn fetch(&self, blob: &BlobRef, part: &Path) -> Result<File, Error> {
        let mut sink = PartSink {
            file: File::create(part).map_err(ctx("create blob file"))?,
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

    fn build(&self, want: &TreeWanted, part: &Path, stage: &Path) -> Result<(), Error> {
        let blob = self.fetch(&want.blob, part)?;
        self.reached(Step::Fetched)?;
        std::fs::create_dir(stage).map_err(ctx("create stage"))?;
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
        self.make_room(sealed.bytes)?;
        self.place(stage, &want.diff_id)?;
        self.reached(Step::Renamed)?;
        let record = Record {
            diff_id: want.diff_id.as_str().to_owned(),
            blob: want.blob.digest.to_string(),
            extractor_version: self.cfg.extractor_version,
            bytes: sealed.bytes,
        };
        self.write_record(&want.diff_id, &record)?;
        self.reached(Step::Recorded)?;
        let now = self.now();
        let mut state = self.lock();
        state.next_seq += 1;
        let seq = state.next_seq;
        // A racing extraction of the same tree may have entered it first; it is the same tree.
        state.trees.entry(want.diff_id.clone()).or_insert(Entry {
            bytes: sealed.bytes,
            version: self.cfg.extractor_version,
            pins: BTreeSet::new(),
            seq,
            last_use_ms: now,
        });
        Ok(())
    }

    /// The flight's leader: replaces a stale tree, builds, and cleans up after a failure.
    fn lead(&self, want: &TreeWanted, stale: bool) -> Result<(), Error> {
        let stage = self.unique("stage", want.diff_id.hex());
        let part = self.unique("tmp", "part");
        let built = if stale {
            self.trash_tree(&want.diff_id)
                .and_then(|()| self.build(want, &part, &stage))
        } else {
            self.build(want, &part, &stage)
        };
        if matches!(built, Err(Error::Crashed(_))) {
            return built;
        }
        if stage.exists() {
            self.discard(&stage);
        }
        let _ = std::fs::remove_file(&part);
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
    /// Opens the directory and sweeps what a crash left: `tmp/`, `stage/` and `trash/` are
    /// emptied, a tree without a valid record is deleted, pins of runs not in `live` are
    /// dropped. Trees found are ordered by the age of their record for eviction.
    ///
    /// # Errors
    /// A directory cannot be made or read.
    pub fn open(
        dir: &Path,
        source: F,
        sealer: S,
        cfg: Config,
        live: &BTreeSet<RunId>,
    ) -> Result<TreeStore<S, F>, Error> {
        for sub in ["trees", "tmp", "stage", "trash", "pins"] {
            std::fs::create_dir_all(dir.join(sub)).map_err(ctx("create store directory"))?;
        }
        let inner = Inner {
            dir: dir.to_owned(),
            source,
            sealer,
            cfg,
            state: Mutex::new(State::default()),
            done: Condvar::new(),
            next: AtomicU64::new(0),
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
        {
            let mut state = inner.lock();
            for (_, diff_id, record, pins) in found {
                state.next_seq += 1;
                let seq = state.next_seq;
                state.trees.insert(
                    diff_id,
                    Entry {
                        bytes: record.bytes,
                        version: record.extractor_version,
                        pins,
                        seq,
                        last_use_ms: now,
                    },
                );
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
    /// # Errors
    /// The source fails, the blob is not the one asked for, the layer breaks a rule or is
    /// not the one the diff id names, sealing or a file-system call fails, no room can be
    /// made ([`Error::NoSpace`]), or the tree is stale and pinned ([`Error::Busy`]).
    pub fn ensure(&self, want: &TreeWanted) -> Result<Ready, Error> {
        let inner = &self.inner;
        let key = (want.diff_id.clone(), want.blob.digest.clone());
        let stale = loop {
            let mut state = inner.lock();
            let (fresh, pinned) = state.trees.get(&want.diff_id).map_or((false, false), |e| {
                (e.version == inner.cfg.extractor_version, !e.pins.is_empty())
            });
            if fresh {
                state.touch(&want.diff_id, inner.now());
                return Ok(self.ready(&want.diff_id));
            }
            if pinned {
                return Err(Error::Busy);
            }
            if state.flights.contains(&key) {
                state.waiting += 1;
                let mut state = inner.done.wait(state).expect("tree store state poisoned");
                state.waiting -= 1;
                continue;
            }
            state.flights.insert(key.clone());
            break state.trees.remove(&want.diff_id).is_some();
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

    /// Evicts until the budget and the low-water mark hold, as far as it can.
    ///
    /// # Errors
    /// [`Error::NoSpace`] when what is left is pinned, kept or too recently used.
    pub fn trim(&self) -> Result<(), Error> {
        self.inner.make_room(0)
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
        state.touch(diff_id, inner.now());
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
            used: state.trees.values().map(|e| e.bytes).sum(),
            pinned: state.trees.values().filter(|e| !e.pins.is_empty()).count(),
            in_flight: state.flights.len(),
            waiting: state.waiting,
        }
    }
}
