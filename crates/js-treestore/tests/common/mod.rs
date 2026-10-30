//! A fake blob store (the contract of its section 2.2, including the callback rules), a
//! fake sealer and fixtures.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write as _};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};

use js_bundle::media::LayerEncoding;
use js_bundle::{DiffId, Limits, pack_layer, pack_tar};
use js_testkit::dir::TestDir;
use js_treestore::blobstore::{
    AccountHandle, BlobKey, BlobStore, Busy, Holder, PinSet, Pinned, Principal, Release, Source,
    Stats, StoreError, UpstreamError, VerifiedBlob, Wanted,
};
use js_treestore::{Config, SealError, Sealed, Sealer, TreeStore, TreeWanted};
use sha2::{Digest, Sha256};

thread_local! {
    static IN_RELEASE: Cell<bool> = const { Cell::new(false) };
}

fn reentrant() -> bool {
    IN_RELEASE.with(Cell::get)
}

struct Accounted {
    holder: Holder,
    bytes: u64,
    priority: u8,
    tick: u64,
    release: Arc<dyn Release>,
}

#[derive(Default)]
struct FakeInner {
    blobs: BTreeMap<BlobKey, Vec<u8>>,
    pins: BTreeMap<Holder, Vec<Pinned>>,
    entries: BTreeMap<u64, Accounted>,
    next_id: u64,
    tick: u64,
    files: u64,
    gc_started: bool,
    admitted: BTreeMap<BlobKey, BTreeSet<Principal>>,
}

/// A gate the fake's `ensure` waits at until it is opened.
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

pub struct FakeStore {
    inner: Arc<Mutex<FakeInner>>,
    scratch: PathBuf,
    pub ensures: AtomicUsize,
    pub entered: AtomicUsize,
    gate: Mutex<Option<Arc<Gate>>>,
    barrier: Mutex<Option<Arc<Barrier>>>,
    pub fail_ensures: AtomicUsize,
    pub fail_account: AtomicBool,
    pub fail_release: AtomicBool,
}

struct Handle {
    inner: Arc<Mutex<FakeInner>>,
    id: u64,
}

impl AccountHandle for Handle {
    fn update(&self, bytes: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.entries.get_mut(&self.id) {
            entry.bytes = bytes;
        }
    }

    fn touch(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(entry) = inner.entries.get_mut(&self.id) {
            entry.tick = tick;
        }
    }

    fn drop_entry(&self) {
        self.inner.lock().unwrap().entries.remove(&self.id);
    }
}

impl FakeStore {
    pub fn new(scratch: &Path) -> Arc<FakeStore> {
        std::fs::create_dir_all(scratch).unwrap();
        Arc::new(FakeStore {
            inner: Arc::new(Mutex::new(FakeInner::default())),
            scratch: scratch.to_owned(),
            ensures: AtomicUsize::new(0),
            entered: AtomicUsize::new(0),
            gate: Mutex::new(None),
            barrier: Mutex::new(None),
            fail_ensures: AtomicUsize::new(0),
            fail_account: AtomicBool::new(false),
            fail_release: AtomicBool::new(false),
        })
    }

    pub fn add_blob(&self, key: &BlobKey, bytes: Vec<u8>) {
        self.inner.lock().unwrap().blobs.insert(key.clone(), bytes);
    }

    /// Makes `ensure` wait until the returned gate is opened.
    pub fn hold_ensures(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            open: Mutex::new(false),
            changed: Condvar::new(),
        });
        *self.gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Makes `account` wait for `n` callers to arrive.
    pub fn meet_at_account(&self, n: usize) {
        *self.barrier.lock().unwrap() = Some(Arc::new(Barrier::new(n)));
    }

    pub fn wait_entered(&self, n: usize) {
        while self.entered.load(Ordering::SeqCst) < n {
            std::thread::yield_now();
        }
    }

    pub fn accounted(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        inner.entries.values().map(|e| e.bytes).sum()
    }

    pub fn entries(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn priorities(&self) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        inner.entries.values().map(|e| e.priority).collect()
    }

    pub fn holders_pinning(&self) -> Vec<Holder> {
        let inner = self.inner.lock().unwrap();
        inner
            .pins
            .iter()
            .filter(|(_, pins)| !pins.is_empty())
            .map(|(holder, _)| holder.clone())
            .collect()
    }

    pub fn start_gc_now(&self) {
        self.start_gc();
    }

    pub fn admissions_of(&self, principal: &Principal) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .admitted
            .values()
            .filter(|set| set.contains(principal))
            .count()
    }

    pub fn gc_started(&self) -> bool {
        self.inner.lock().unwrap().gc_started
    }

    pub fn first_release(&self) -> Arc<dyn Release> {
        let inner = self.inner.lock().unwrap();
        inner.entries.values().next().unwrap().release.clone()
    }

    /// The start-only call of the real store: drops the pins of holders that are not live.
    pub fn reap_pins(&self, live: &BTreeSet<Holder>) {
        let mut inner = self.inner.lock().unwrap();
        inner.pins.retain(|holder, _| live.contains(holder));
    }

    /// A process restart: accounted entries live in memory and are gone; stale pins are
    /// reaped; GC is off again.
    pub fn restart(&self, live: &BTreeSet<Holder>) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.entries.clear();
            inner.gc_started = false;
        }
        self.reap_pins(live);
    }

    /// Evicts, as the store's cleaner does, until the accounted bytes are within `budget`:
    /// lower priority first, then least recently used, a `Busy` answer skipping to the next
    /// candidate, each callback run outside the lock. Returns the bytes over budget at the end.
    pub fn pressure(&self, budget: u64) -> u64 {
        let mut skipped = BTreeSet::new();
        loop {
            let (id, release) = {
                let inner = self.inner.lock().unwrap();
                let total: u64 = inner.entries.values().map(|e| e.bytes).sum();
                if total <= budget {
                    return 0;
                }
                let next = inner
                    .entries
                    .iter()
                    .filter(|(id, _)| !skipped.contains(*id))
                    .min_by_key(|(_, e)| (e.priority, e.tick));
                match next {
                    Some((id, e)) => (*id, e.release.clone()),
                    None => return total - budget,
                }
            };
            IN_RELEASE.with(|flag| flag.set(true));
            let answer = release.release();
            IN_RELEASE.with(|flag| flag.set(false));
            match answer {
                Ok(_) => {
                    self.inner.lock().unwrap().entries.remove(&id);
                }
                Err(Busy) => {
                    skipped.insert(id);
                }
            }
        }
    }
}

impl BlobStore for FakeStore {
    fn ensure(&self, want: &Wanted, holder: &Holder) -> Result<Pinned, StoreError> {
        if reentrant() {
            return Err(StoreError::Reentrant);
        }
        self.ensures.fetch_add(1, Ordering::SeqCst);
        self.entered.fetch_add(1, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        let failing = self.fail_ensures.load(Ordering::SeqCst);
        if failing > 0 {
            self.fail_ensures.store(failing - 1, Ordering::SeqCst);
            return Err(StoreError::Forbidden);
        }
        let mut inner = self.inner.lock().unwrap();
        inner
            .admitted
            .entry(want.key.clone())
            .or_default()
            .insert(want.principal.clone());
        let size = match inner.blobs.get(&want.key) {
            Some(bytes) => bytes.len() as u64,
            None => return Err(UpstreamError("no such blob".into()).into()),
        };
        let pin = Pinned {
            key: want.key.clone(),
            size,
        };
        inner
            .pins
            .entry(holder.clone())
            .or_default()
            .push(pin.clone());
        Ok(pin)
    }

    fn ensure_all(&self, wants: &[Wanted], holder: &Holder) -> Result<PinSet, StoreError> {
        let mut pins = Vec::new();
        for want in wants {
            pins.push(self.ensure(want, holder)?);
        }
        Ok(PinSet { pins })
    }

    fn put(
        &self,
        key: &BlobKey,
        size: u64,
        r: &mut dyn Read,
        holder: &Holder,
        principal: &Principal,
    ) -> Result<Pinned, StoreError> {
        if reentrant() {
            return Err(StoreError::Reentrant);
        }
        self.inner
            .lock()
            .unwrap()
            .admitted
            .entry(key.clone())
            .or_default()
            .insert(principal.clone());
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        self.add_blob(key, bytes);
        let pin = Pinned {
            key: key.clone(),
            size,
        };
        let mut inner = self.inner.lock().unwrap();
        inner
            .pins
            .entry(holder.clone())
            .or_default()
            .push(pin.clone());
        Ok(pin)
    }

    fn open_verified(&self, pin: &Pinned) -> Result<VerifiedBlob, StoreError> {
        let (bytes, n) = {
            let mut inner = self.inner.lock().unwrap();
            inner.files += 1;
            (inner.blobs.get(&pin.key).cloned().unwrap(), inner.files)
        };
        let path = self.scratch.join(format!("open-{n}"));
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&bytes)?;
        let file = std::fs::File::open(&path)?;
        Ok(VerifiedBlob {
            fd: OwnedFd::from(file),
            size: pin.size,
        })
    }

    fn release(&self, holder: &Holder) -> Result<(), StoreError> {
        if reentrant() {
            return Err(StoreError::Reentrant);
        }
        if self.fail_release.load(Ordering::SeqCst) {
            return Err(StoreError::NotReady);
        }
        self.inner.lock().unwrap().pins.remove(holder);
        Ok(())
    }

    fn end_principal(&self, principal: &Principal) {
        let mut inner = self.inner.lock().unwrap();
        for set in inner.admitted.values_mut() {
            set.remove(principal);
        }
        inner.admitted.retain(|_, set| !set.is_empty());
    }

    fn stats(&self) -> Stats {
        Stats {
            accounted: self.accounted(),
            ..Stats::default()
        }
    }

    fn account(
        &self,
        holder: &Holder,
        _ns: &str,
        bytes: u64,
        priority: u8,
        r: Arc<dyn Release>,
    ) -> Result<Arc<dyn AccountHandle>, StoreError> {
        if reentrant() {
            return Err(StoreError::Reentrant);
        }
        let barrier = self.barrier.lock().unwrap().clone();
        if let Some(barrier) = barrier {
            barrier.wait();
        }
        if self.fail_account.load(Ordering::SeqCst) {
            return Err(StoreError::NoSpace {
                used: 0,
                pinned: 0,
                budget: 0,
                incoming: bytes,
            });
        }
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        inner.tick += 1;
        let id = inner.next_id;
        let tick = inner.tick;
        inner.entries.insert(
            id,
            Accounted {
                holder: holder.clone(),
                bytes,
                priority,
                tick,
                release: r,
            },
        );
        Ok(Arc::new(Handle {
            inner: self.inner.clone(),
            id,
        }))
    }

    fn start_gc(&self) {
        self.inner.lock().unwrap().gc_started = true;
    }
}

/// What the fake sealer does with a stage.
pub const SEAL_OK: u8 = 0;
pub const SEAL_FAILS: u8 = 1;
pub const SEAL_VANISHES: u8 = 2;

#[derive(Clone)]
pub struct FakeSealer {
    pub seals: Arc<AtomicUsize>,
    pub unsealed: Arc<Mutex<Vec<PathBuf>>>,
    pub mode: Arc<AtomicU8>,
}

impl FakeSealer {
    pub fn new() -> FakeSealer {
        FakeSealer {
            seals: Arc::new(AtomicUsize::new(0)),
            unsealed: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(AtomicU8::new(SEAL_OK)),
        }
    }

    pub fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    pub fn seal_count(&self) -> usize {
        self.seals.load(Ordering::SeqCst)
    }
}

fn size_of(path: &Path) -> u64 {
    let meta = std::fs::symlink_metadata(path).unwrap();
    if meta.is_dir() {
        std::fs::read_dir(path)
            .unwrap()
            .map(|e| size_of(&e.unwrap().path()))
            .sum()
    } else {
        meta.len()
    }
}

impl Sealer for FakeSealer {
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError> {
        self.seals.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode.load(Ordering::SeqCst);
        if mode == SEAL_FAILS {
            return Err(SealError("refused".into()));
        }
        if mode == SEAL_VANISHES {
            std::fs::remove_dir_all(stage).unwrap();
            return Ok(Sealed { bytes: 1 });
        }
        Ok(Sealed {
            bytes: size_of(stage),
        })
    }

    fn unseal(&self, tree: &Path) -> Result<(), SealError> {
        self.unsealed.lock().unwrap().push(tree.to_owned());
        Ok(())
    }
}

/// A layer: its diff id, the blob that stores it and how.
pub struct Layer {
    pub diff_id: DiffId,
    pub key: BlobKey,
    pub bytes: Vec<u8>,
    pub encoding: LayerEncoding,
}

fn write_tree(files: &[(&str, &[u8])]) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "layer-src");
    for (name, data) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
    dir
}

/// A compressed layer of the files.
pub fn layer(files: &[(&str, &[u8])]) -> Layer {
    let dir = write_tree(files);
    let packed = pack_layer(dir.path()).unwrap();
    Layer {
        diff_id: packed.diff_id,
        key: BlobKey::parse(&packed.blob_digest).unwrap(),
        bytes: packed.blob,
        encoding: LayerEncoding::TarZstd,
    }
}

/// The same tree as a plain tar: the same diff id, another blob.
pub fn tar_layer(files: &[(&str, &[u8])]) -> Layer {
    let dir = write_tree(files);
    let tar = pack_tar(dir.path()).unwrap();
    let key = BlobKey::parse(&format!(
        "sha256:{}",
        hex::encode(Sha256::digest(&tar.bytes))
    ))
    .unwrap();
    Layer {
        diff_id: tar.diff_id,
        key,
        bytes: tar.bytes,
        encoding: LayerEncoding::Tar,
    }
}

/// A layer whose single file is `n` bytes of `fill`, so its tree takes `n` bytes.
pub fn sized(name: &str, n: usize, fill: u8) -> Layer {
    layer(&[(name, &vec![fill; n])])
}

pub fn wanted(layer: &Layer) -> TreeWanted {
    TreeWanted {
        diff_id: layer.diff_id.clone(),
        blob: Wanted {
            key: layer.key.clone(),
            size: Some(layer.bytes.len() as u64),
            max_size: 1 << 30,
            source: Source("fixture".into()),
            principal: Principal("lease-1".into()),
        },
        encoding: layer.encoding,
        limits: Limits {
            entries: 1000,
            bytes: 1 << 24,
            model_bytes: 1 << 28,
        },
    }
}

pub type Store = TreeStore<FakeSealer, FakeStore>;

pub struct Rig {
    pub dir: TestDir,
    pub fake: Arc<FakeStore>,
    pub sealer: FakeSealer,
}

impl Rig {
    pub fn new() -> Rig {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "treestore");
        let fake = FakeStore::new(&dir.path().join("fake"));
        Rig {
            dir,
            fake,
            sealer: FakeSealer::new(),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("ts")
    }

    /// Adds the layer's blob to the fake store.
    pub fn have(&self, layer: &Layer) {
        self.fake.add_blob(&layer.key, layer.bytes.clone());
    }

    pub fn open(&self, cfg: Config) -> Store {
        self.open_live(cfg, &BTreeSet::new())
    }

    pub fn open_live(&self, cfg: Config, live: &BTreeSet<js_treestore::RunId>) -> Store {
        self.try_open(cfg, live).unwrap()
    }

    pub fn try_open(
        &self,
        cfg: Config,
        live: &BTreeSet<js_treestore::RunId>,
    ) -> Result<Store, js_treestore::Error> {
        TreeStore::open(
            &self.root(),
            self.fake.clone(),
            self.sealer.clone(),
            cfg,
            live,
        )
    }

    pub fn names(&self, sub: &str) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(self.root().join(sub))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }
}

pub fn run(text: &str) -> js_treestore::RunId {
    js_treestore::RunId::new(text).unwrap()
}
