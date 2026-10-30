//! A fake layer source, a fake sealer, a manual clock and fixtures.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use js_bundle::media::LayerEncoding;
use js_bundle::{DiffId, Limits, pack_layer, pack_tar};
use js_testkit::dir::TestDir;
use js_treestore::{
    BlobRef, BlobSink, Config, Digest, FetchError, LayerSource, SealError, Sealed, Sealer,
    TreeStore, TreeWanted,
};
use sha2::{Digest as _, Sha256};

/// A gate the fake's `fetch` waits at until it is opened.
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    pub fn closed() -> Arc<Gate> {
        Arc::new(Gate {
            open: Mutex::new(false),
            changed: Condvar::new(),
        })
    }

    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    pub fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

#[derive(Default)]
pub struct Src {
    blobs: Mutex<BTreeMap<Digest, Vec<u8>>>,
    gate: Mutex<Option<Arc<Gate>>>,
    pub fetches: AtomicUsize,
    pub entered: AtomicUsize,
    pub fail_fetches: AtomicUsize,
    pub panic_fetches: AtomicUsize,
}

/// The fake source; clones share state.
#[derive(Clone, Default)]
pub struct FakeSource(pub Arc<Src>);

impl FakeSource {
    /// Serves `bytes` for `digest`, whether or not they hash to it.
    pub fn add(&self, digest: &Digest, bytes: Vec<u8>) {
        self.0.blobs.lock().unwrap().insert(digest.clone(), bytes);
    }

    pub fn hold_fetches(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            open: Mutex::new(false),
            changed: Condvar::new(),
        });
        *self.0.gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    pub fn wait_entered(&self, n: usize) {
        while self.0.entered.load(Ordering::SeqCst) < n {
            std::thread::yield_now();
        }
    }

    pub fn fetches(&self) -> usize {
        self.0.fetches.load(Ordering::SeqCst)
    }
}

impl LayerSource for FakeSource {
    fn fetch(&self, blob: &BlobRef, sink: &mut dyn BlobSink) -> Result<(), FetchError> {
        self.0.fetches.fetch_add(1, Ordering::SeqCst);
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        let gate = self.0.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        let panicking = self.0.panic_fetches.load(Ordering::SeqCst);
        if panicking > 0 {
            self.0.panic_fetches.store(panicking - 1, Ordering::SeqCst);
            panic!("the source panicked");
        }
        let failing = self.0.fail_fetches.load(Ordering::SeqCst);
        if failing > 0 {
            self.0.fail_fetches.store(failing - 1, Ordering::SeqCst);
            return Err(FetchError("source down".into()));
        }
        let bytes = self.0.blobs.lock().unwrap().get(&blob.digest).cloned();
        let Some(bytes) = bytes else {
            return Err(FetchError("no such blob".into()));
        };
        for chunk in bytes.chunks(7) {
            sink.write(chunk).map_err(|e| FetchError(e.to_string()))?;
        }
        Ok(())
    }
}

pub const SEAL_OK: u8 = 0;
pub const SEAL_FAILS: u8 = 1;
pub const SEAL_VANISHES: u8 = 2;

#[derive(Clone)]
pub struct FakeSealer {
    pub seals: Arc<AtomicUsize>,
    pub unsealed: Arc<Mutex<Vec<PathBuf>>>,
    pub mode: Arc<AtomicU8>,
    /// Calls that returned with the tree's data and directories synced.
    pub syncs: Arc<AtomicUsize>,
    /// Calls of `unseal_for_delete` that have started.
    pub unsealing: Arc<AtomicUsize>,
    pub hold_unseal: Arc<Mutex<Option<Arc<Gate>>>>,
}

impl FakeSealer {
    pub fn new() -> FakeSealer {
        FakeSealer {
            seals: Arc::new(AtomicUsize::new(0)),
            unsealed: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(AtomicU8::new(SEAL_OK)),
            syncs: Arc::new(AtomicUsize::new(0)),
            unsealing: Arc::new(AtomicUsize::new(0)),
            hold_unseal: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    pub fn seal_count(&self) -> usize {
        self.seals.load(Ordering::SeqCst)
    }
}

pub fn size_of(path: &Path) -> u64 {
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
        // The contract: data and directories are synced before seal returns.
        self.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(Sealed {
            bytes: size_of(stage),
        })
    }

    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError> {
        self.unsealing.fetch_add(1, Ordering::SeqCst);
        let gate = self.hold_unseal.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        self.unsealed.lock().unwrap().push(tree.to_owned());
        Ok(())
    }
}

/// A layer: its diff id, the blob that stores it and how.
pub struct Layer {
    pub diff_id: DiffId,
    pub digest: Digest,
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
        digest: Digest::parse(&packed.blob_digest).unwrap(),
        bytes: packed.blob,
        encoding: LayerEncoding::TarZstd,
    }
}

/// The same tree as a plain tar: the same diff id, another blob.
pub fn tar_layer(files: &[(&str, &[u8])]) -> Layer {
    let dir = write_tree(files);
    let tar = pack_tar(dir.path()).unwrap();
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&tar.bytes)));
    Layer {
        diff_id: tar.diff_id,
        digest: Digest::parse(&digest).unwrap(),
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
        blob: BlobRef {
            digest: layer.digest.clone(),
            size: layer.bytes.len() as u64,
        },
        encoding: layer.encoding,
        limits: Limits {
            entries: 1000,
            bytes: 1 << 24,
            model_bytes: 1 << 28,
        },
        expected_bytes: None,
    }
}

pub type Store = TreeStore<FakeSealer, FakeSource>;

pub struct Rig {
    pub dir: TestDir,
    pub source: FakeSource,
    pub sealer: FakeSealer,
    pub now: Arc<AtomicU64>,
}

impl Rig {
    pub fn new() -> Rig {
        Rig {
            dir: TestDir::under(env!("CARGO_TARGET_TMPDIR"), "treestore"),
            source: FakeSource::default(),
            sealer: FakeSealer::new(),
            now: Arc::new(AtomicU64::new(1_000_000)),
        }
    }

    /// The default config with the rig's manual clock.
    pub fn cfg(&self) -> Config {
        let now = self.now.clone();
        Config {
            clock: Arc::new(move || now.load(Ordering::SeqCst)),
            ..Config::default()
        }
    }

    pub fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("ts")
    }

    /// Adds the layer's blob to the fake source.
    pub fn have(&self, layer: &Layer) {
        self.source.add(&layer.digest, layer.bytes.clone());
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
            self.source.clone(),
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

    /// A tree and its record are in place.
    pub fn present(&self, id: &DiffId) -> bool {
        self.root().join("trees").join(id.hex()).is_dir()
            && self
                .root()
                .join("trees")
                .join(format!("{}.sealed", id.hex()))
                .is_file()
    }
}

pub fn run(text: &str) -> js_treestore::RunId {
    js_treestore::RunId::new(text).unwrap()
}
