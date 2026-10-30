//! Placing a bundle on the host: the agent's side of the tree store.
//!
//! A [`Placer`] owns the store of one agent (`<bundle-store>/v1/objects`: `trees/` of the
//! tree store, `manifests/` and `configs/` beside it). It does three things:
//!
//! - **Ensure.** [`Placer::ensure_registry`] and [`Placer::ensure_layout`] pull the bundle
//!   whose manifest has a given digest through js-oci (every byte checked against a digest
//!   on the way in), read its config, have each layer extracted and sealed as a tree by the
//!   tree store, and only then write the manifest and the config beside the trees. A
//!   manifest in the store therefore means that its trees were there when it was written;
//!   [`Placer::pin`] looks at the trees again, since the store may have evicted some since
//!   (the host's list of held bundles reads the manifests only).
//! - **Fetch.** [`Placer::fetch`] is what the admission of a start calls when
//!   `--bundle-pull missing` and the store lacks the bundle: it begins an ensure on a
//!   thread of its own and answers [`Fetch::Started`] at once, so the lifecycle's worker
//!   never waits for a download. A second call for the digest joins the first; the call
//!   after a failed fetch reports the failure once and then tries again.
//! - **Pin.** [`Placer::pin`] pins every tree of a held bundle for one run and gives back a
//!   [`Pinned`], which unpins when it is dropped: a run that ends, or a start that fails
//!   half way, can never leave a pin behind.
//!
//! The bytes of a layer go from js-oci to the tree store through a bounded channel, so a
//! layer is never held whole in memory and the tree store sees the same sink it has for any
//! source.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use async_trait::async_trait;
use js_bundle::media::LayerEncoding;
use js_bundle::{BundleConfig, DEFAULT_MODEL_BYTES, Limits, TreeName};
use js_oci::{BundleSource, Client, Descriptor, Kind, Layout, Location, Manifest};
use js_treestore::{
    BlobRef, BlobSink, Config, Digest, FetchError, LayerSource, Owner, RunId, SealError, Sealed,
    Sealer, TreeHandle, TreeStore, TreeWanted,
};
use js_types::BundleRef;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

use super::host::Fetch;
use super::settings::Settings;

/// Why a bundle could not be placed or pinned.
#[derive(Debug, thiserror::Error)]
pub enum PlaceError {
    /// The source refused or could not give the manifest, the config or a layer.
    #[error("{0}")]
    Source(#[from] js_oci::Error),
    /// The config is not a valid bundle config.
    #[error("the bundle's config: {0}")]
    Config(#[from] js_bundle::ConfigError),
    /// The tree store could not make, find or pin a tree.
    #[error("{0}")]
    Tree(#[from] js_treestore::Error),
    /// A tree could not be cloned.
    #[error("{0}")]
    Seal(#[from] SealError),
    /// A file or a runtime could not be made.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The manifest and the config describe different layers.
    #[error("the manifest and the config disagree: {0}")]
    Mismatch(&'static str),
    /// The pin is not a bundle reference, or the store holds no usable record of it.
    #[error("{0}")]
    Held(String),
}

/// Where a bundle's bytes come from.
#[derive(Clone)]
enum Origin {
    /// A registry, by the client's policy.
    Registry(Arc<Client>, Location),
    /// An OCI image-layout directory.
    Layout(Arc<Layout>),
}

#[async_trait]
impl BundleSource for Origin {
    async fn fetch_manifest(
        &self,
        digest: &js_oci::Digest,
        kind: &Kind,
    ) -> Result<Manifest, js_oci::Error> {
        match self {
            Origin::Registry(client, at) => {
                client
                    .repository(at.clone())?
                    .fetch_manifest(digest, kind)
                    .await
            }
            Origin::Layout(layout) => layout.fetch_manifest(digest, kind).await,
        }
    }

    async fn fetch_blob(
        &self,
        blob: &Descriptor,
        sink: &mut dyn js_oci::BlobSink,
    ) -> Result<(), js_oci::Error> {
        match self {
            Origin::Registry(client, at) => {
                client.repository(at.clone())?.fetch_blob(blob, sink).await
            }
            Origin::Layout(layout) => layout.fetch_blob(blob, sink).await,
        }
    }
}

/// A layer's digest, the ensures that use it, and where it is read from.
type Table = BTreeMap<String, (usize, Origin, Descriptor)>;

/// The blobs the tree store may ask for, each with the origin it is read from, while an
/// ensure of the bundle that names it runs. Two ensures of bundles that share a layer share
/// its entry, which lives until the last of them is done. The tree store is blocking and
/// calls its source from whatever thread asks for a tree, so each call runs its download on
/// the one runtime of the store.
#[derive(Clone)]
struct Layers {
    table: Arc<Mutex<Table>>,
    runtime: Arc<Runtime>,
}

impl Layers {
    fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            table: Arc::default(),
            runtime,
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn add(&self, origin: &Origin, layers: &[Descriptor]) {
        let mut table = self.table();
        for layer in layers {
            let entry = table
                .entry(layer.digest().as_str().to_owned())
                .or_insert_with(|| (0, origin.clone(), layer.clone()));
            entry.0 += 1;
        }
    }

    /// Takes one ensure's use of each layer back; an entry nobody uses is dropped. Written
    /// without a branch for "the last one" so that a table that is only ever used by one
    /// ensure at a time and one used by two at once are the same code.
    fn remove(&self, layers: &[Descriptor]) {
        let mut table = self.table();
        for layer in layers {
            table
                .entry(layer.digest().as_str().to_owned())
                .and_modify(|entry| entry.0 -= 1);
        }
        table.retain(|_, entry| entry.0 > 0);
    }
}

/// Why a layer could not be read into the tree store.
#[derive(Debug, thiserror::Error)]
enum Pull {
    /// The origin refused or could not give the blob.
    #[error("{0}")]
    Origin(#[from] js_oci::Error),
    /// The tree store's sink refused a chunk.
    #[error("{0}")]
    Sink(#[from] io::Error),
}

/// A sink of js-oci that hands each chunk to the receiving end, which writes it to the tree
/// store's sink (which is not `Send`, so it cannot be the sink of js-oci itself).
struct Chunks(mpsc::Sender<Vec<u8>>);

#[async_trait]
impl js_oci::BlobSink for Chunks {
    async fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.0.send(chunk.to_vec()).await.map_err(io::Error::other)
    }

    async fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads `blob` from `origin` into `sink`. The channel is small, so the download waits for
/// the store's disk; when the store's sink fails the receiving end is dropped and the
/// download ends with it. The store's error is the one reported.
async fn download(origin: Origin, blob: Descriptor, sink: &mut dyn BlobSink) -> Result<(), Pull> {
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(4);
    let pull = async move { Ok(origin.fetch_blob(&blob, &mut Chunks(sender)).await?) };
    let drain = async move {
        while let Some(chunk) = receiver.recv().await {
            sink.write(&chunk)?;
        }
        Ok::<(), Pull>(())
    };
    let (pulled, drained) = tokio::join!(pull, drain);
    drained.and(pulled)
}

impl LayerSource for Layers {
    fn fetch(&self, blob: &BlobRef, sink: &mut dyn BlobSink) -> Result<(), FetchError> {
        let key = blob.digest.as_str();
        let found = self
            .table()
            .get(key)
            .map(|(_, origin, descriptor)| (origin.clone(), descriptor.clone()));
        // Not a closure: nothing asks for a blob no ensure named, so it would never run.
        let (origin, descriptor) = found.ok_or(FetchError(format!("no source names {key}")))?;
        self.runtime
            .block_on(download(origin, descriptor, sink))
            .map_err(|why| FetchError(why.to_string()))
    }
}

/// A [`Sealer`] that is a shared trait object, so the agent's store is one type whatever
/// seals it.
struct Shared(Arc<dyn Sealer>);

impl Sealer for Shared {
    fn seal(&self, stage: &Path) -> Result<Sealed, SealError> {
        self.0.seal(stage)
    }

    fn unseal_for_delete(&self, tree: &Path) -> Result<(), SealError> {
        self.0.unseal_for_delete(tree)
    }

    fn clone_writable(&self, src: &Path, dst: &Path, owner: Owner) -> Result<(), SealError> {
        self.0.clone_writable(src, dst, owner)
    }
}

/// A fetch in the background.
#[derive(Debug, Clone)]
enum Flight {
    /// Running.
    Running,
    /// Ended badly; the next call for the digest reports it and clears it.
    Failed(Fetch),
}

/// What a call that wants a fetch of a digest finds.
#[derive(Debug, PartialEq, Eq)]
enum Begin {
    /// Nothing is in flight for the digest: the caller runs the fetch.
    Run,
    /// A fetch of the digest is running: the caller joins it.
    Running,
    /// The fetch before ended badly; this is its failure, reported once.
    Failed(Fetch),
}

/// The background fetches, by manifest digest.
#[derive(Default)]
struct Flights {
    table: Mutex<BTreeMap<String, Flight>>,
    settled: Condvar,
}

impl Flights {
    fn table(&self) -> MutexGuard<'_, BTreeMap<String, Flight>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims the fetch of `digest` for the caller when none is running, and clears a
    /// failure it reports, so the call after it begins again.
    fn begin(&self, digest: &str) -> Begin {
        let mut table = self.table();
        match table.get(digest) {
            Some(Flight::Running) => return Begin::Running,
            Some(Flight::Failed(failure)) => {
                let failure = failure.clone();
                table.remove(digest);
                return Begin::Failed(failure);
            }
            None => {}
        }
        table.insert(digest.to_owned(), Flight::Running);
        Begin::Run
    }

    /// Ends the fetch of `digest`: a success leaves nothing, a failure waits for the next
    /// call.
    fn end(&self, digest: &str, failure: Option<Fetch>) {
        let mut table = self.table();
        match failure {
            None => table.remove(digest),
            Some(failure) => table.insert(digest.to_owned(), Flight::Failed(failure)),
        };
        drop(table);
        self.settled.notify_all();
    }

    /// Waits until no fetch is running.
    fn settle(&self) {
        let mut table = self.table();
        while table
            .values()
            .any(|flight| matches!(flight, Flight::Running))
        {
            table = self
                .settled
                .wait(table)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// How a background fetch ended, as the answer of the next call for its digest: `None` for
/// a bundle that is held now. A fetch that panicked is a failure like any other.
fn failure_of(ended: thread::Result<Result<(), PlaceError>>) -> Option<Fetch> {
    match ended {
        Ok(Ok(())) => None,
        Ok(Err(PlaceError::Source(js_oci::Error::Index(_)))) => Some(Fetch::Index),
        Ok(Err(why)) => Some(Fetch::Failed(why.to_string())),
        Err(_) => Some(Fetch::Failed("the fetch stopped unexpectedly".to_owned())),
    }
}

/// The store of one agent, and what is done with it.
pub struct Placer {
    root: PathBuf,
    objects: PathBuf,
    trees: TreeStore<Shared, Layers>,
    layers: Layers,
    runtime: Arc<Runtime>,
    sealer: Arc<dyn Sealer>,
    client: Arc<Client>,
    flights: Flights,
}

impl fmt::Debug for Placer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Placer")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// `name` without its tag: a tag follows the last `:` after the last `/`.
fn without_tag(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((before, tag)) if !tag.contains('/') => before,
        _ => name,
    }
}

/// Writes `bytes` as `dir/name` through a part file, so a reader never sees half of it.
fn put(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let part = dir.join(format!("{name}.part"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&part)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&part, dir.join(name))
}

/// The config of the held bundle `digest` under the store at `root`, read through `read`
/// for the manifest (the host's trusted reader) and from the config file beside it.
///
/// # Errors
///
/// The manifest or the config cannot be read or is not what it should be; the text says why.
pub fn load_config(
    root: &Path,
    digest: &str,
    read: &dyn Fn(&Path) -> Result<String, String>,
) -> Result<BundleConfig, String> {
    let hex = hex_of(digest)?;
    let objects = root.join("v1/objects");
    let file = objects.join("manifests").join(format!("{hex}.json"));
    let manifest: serde_json::Value =
        serde_json::from_str(&read(&file)?).map_err(|_| "the manifest is not JSON".to_owned())?;
    let config = manifest
        .pointer("/config/digest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "the manifest names no config".to_owned())?;
    let file = objects
        .join("configs")
        .join(format!("{}.json", hex_of(config)?));
    let bytes = fs::read(&file).map_err(|why| format!("{}: {why}", file.display()))?;
    BundleConfig::parse(&bytes).map_err(|why| why.to_string())
}

/// The `<hex>` of `sha256:<hex>`.
fn hex_of(digest: &str) -> Result<&str, String> {
    let hex = digest.strip_prefix("sha256:").unwrap_or("");
    let ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match ok {
        true => Ok(hex),
        false => Err("not a sha256 digest".to_owned()),
    }
}

impl Placer {
    /// Opens the store of `settings` (its directory is made, and what an earlier process left
    /// half made is swept: no run of an earlier process is live, so every pin is dropped).
    /// `free_space` is the free bytes of the store's volume, asked at every eviction
    /// decision; `client` is what registries are reached with.
    ///
    /// # Errors
    ///
    /// The store directory cannot be made or is held by another store.
    pub fn open(
        settings: &Settings,
        sealer: Arc<dyn Sealer>,
        free_space: Arc<dyn Fn() -> u64 + Send + Sync>,
        client: Client,
    ) -> Result<Arc<Self>, PlaceError> {
        let root = fs::canonicalize(&settings.store)?;
        let objects = root.join("v1/objects");
        let runtime = Arc::new(
            Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
        );
        let layers = Layers::new(Arc::clone(&runtime));
        let config = Config {
            budget_bytes: settings.budget_bytes,
            low_water_free_bytes: settings.low_water_bytes,
            min_age_ms: settings.min_age_min.saturating_mul(60_000),
            free_space,
            ..Config::default()
        };
        let trees = TreeStore::open(
            &objects,
            layers.clone(),
            Shared(Arc::clone(&sealer)),
            config,
            &BTreeSet::new(),
        )?;
        for dir in ["manifests", "configs"] {
            fs::create_dir_all(objects.join(dir))?;
        }
        Ok(Arc::new(Self {
            root,
            objects,
            trees,
            layers,
            runtime,
            sealer,
            client: Arc::new(client),
            flights: Flights::default(),
        }))
    }

    /// The store root: the directory trees are at `<root>/v1/objects/trees/<diff id>`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensures the bundle `digest` from the registry `name` names (a pin's name, with its
    /// tag if it has one; the tag is not used), and waits for it.
    ///
    /// # Errors
    ///
    /// The registry refuses, a byte fails its digest, or the store cannot hold the bundle.
    pub fn ensure_registry(&self, name: &str, digest: &str) -> Result<(), PlaceError> {
        let at = Location::parse(without_tag(name))?;
        let origin = Origin::Registry(Arc::clone(&self.client), at);
        self.ensure(&origin, &js_oci::Digest::parse(digest)?)
    }

    /// Ensures the bundle `digest` from the OCI image-layout directory `dir`.
    ///
    /// # Errors
    ///
    /// The directory is not a layout of that bundle, or the store cannot hold it.
    pub fn ensure_layout(&self, dir: &Path, digest: &str) -> Result<(), PlaceError> {
        let origin = Origin::Layout(Arc::new(self.runtime.block_on(Layout::open(dir))?));
        self.ensure(&origin, &js_oci::Digest::parse(digest)?)
    }

    fn ensure(&self, origin: &Origin, digest: &js_oci::Digest) -> Result<(), PlaceError> {
        let kind = Kind::bundle();
        let manifest = self
            .runtime
            .block_on(origin.fetch_manifest(digest, &kind))?;
        let config_bytes = self
            .runtime
            .block_on(origin.fetch_config(&manifest, &kind))?;
        let config = BundleConfig::parse(&config_bytes)?;
        let wanted = wanted(&manifest, &config)?;
        self.layers.add(origin, manifest.layers());
        let made = wanted
            .iter()
            .try_for_each(|want| self.trees.ensure(want).map(drop));
        self.layers.remove(manifest.layers());
        made?;
        let config_hex = manifest.config().digest().hex();
        put(
            &self.objects.join("configs"),
            &format!("{config_hex}.json"),
            &config_bytes,
        )?;
        put(
            &self.objects.join("manifests"),
            &format!("{}.json", digest.hex()),
            manifest.bytes(),
        )?;
        Ok(())
    }

    /// Begins to ensure the bundle `digest` from the registry `name` and answers at once.
    /// [`Fetch::Started`] is the answer for a fetch that began or is running; the failure
    /// of the one before is the answer once (and clears it, so the call after it begins
    /// again). A fetch that panics ends as a failure.
    pub fn fetch(self: &Arc<Self>, name: &str, digest: &str) -> Fetch {
        match self.flights.begin(digest) {
            Begin::Running => return Fetch::Started,
            Begin::Failed(failure) => return failure,
            Begin::Run => {}
        }
        let (placer, name, digest) = (Arc::clone(self), name.to_owned(), digest.to_owned());
        thread::spawn(move || {
            let ensured =
                panic::catch_unwind(AssertUnwindSafe(|| placer.ensure_registry(&name, &digest)));
            placer.flights.end(&digest, failure_of(ensured));
        });
        Fetch::Started
    }

    /// Waits until no fetch started by [`Placer::fetch`] is running.
    pub fn settle(&self) {
        self.flights.settle();
    }

    /// Pins every tree of the held bundle `pin` (`name[:tag]@sha256:<manifest digest>`) for
    /// `run`, which must be a run id the tree store takes (the lease's name is one).
    ///
    /// # Errors
    ///
    /// The pin is not a bundle reference, the store has no record of the bundle, or a tree is
    /// gone (evicted since the bundle was ensured) or already pinned by `run`; nothing stays
    /// pinned then.
    pub fn pin(self: &Arc<Self>, pin: &str, run: &str) -> Result<Pinned, PlaceError> {
        let pin = BundleRef::parse(pin).map_err(|why| PlaceError::Held(why.to_string()))?;
        let run = RunId::new(run).map_err(js_treestore::Error::from)?;
        let read = |path: &Path| {
            fs::read_to_string(path).map_err(|why| format!("{}: {why}", path.display()))
        };
        let config = load_config(&self.root, pin.digest(), &read).map_err(PlaceError::Held)?;
        let layers = config.layers.clone();
        let mut pinned = Pinned {
            placer: Arc::clone(self),
            config,
            handles: Vec::new(),
        };
        for layer in layers {
            let handle = self.trees.pin(&layer.diff_id, &run)?;
            pinned.handles.push((layer.tree, handle));
        }
        Ok(pinned)
    }
}

/// How a layer is stored. js-oci takes a bundle manifest only when every layer is one of
/// the two bundle layer types, so a layer that is not the plain tar is the zstd one.
fn encoding_of(media_type: &str) -> LayerEncoding {
    match media_type == js_bundle::media::LAYER_TAR {
        true => LayerEncoding::Tar,
        false => LayerEncoding::TarZstd,
    }
}

/// The trees to ensure for `manifest`: one for each layer, with the extraction limits and
/// the diff id the config states for it.
fn wanted(manifest: &Manifest, config: &BundleConfig) -> Result<Vec<TreeWanted>, PlaceError> {
    if manifest.layers().len() != config.layers.len() {
        return Err(PlaceError::Mismatch("the layer counts differ"));
    }
    manifest
        .layers()
        .iter()
        .zip(&config.layers)
        .map(|(blob, layer)| {
            let encoding = encoding_of(blob.media_type());
            let digest =
                Digest::parse(blob.digest().as_str()).map_err(js_treestore::Error::from)?;
            Ok(TreeWanted {
                diff_id: layer.diff_id.clone(),
                blob: BlobRef {
                    digest,
                    size: blob.size(),
                },
                encoding,
                limits: Limits {
                    entries: layer.entries,
                    bytes: layer.uncompressed_bytes,
                    model_bytes: DEFAULT_MODEL_BYTES,
                },
                expected_bytes: Some(layer.uncompressed_bytes),
            })
        })
        .collect()
}

/// The trees of a bundle, pinned for one run. Dropping it unpins them.
pub struct Pinned {
    placer: Arc<Placer>,
    config: BundleConfig,
    handles: Vec<(TreeName, TreeHandle)>,
}

impl fmt::Debug for Pinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pinned")
            .field("bundle", &self.config.name)
            .field("trees", &self.handles.len())
            .finish_non_exhaustive()
    }
}

impl Pinned {
    /// The bundle's config.
    #[must_use]
    pub fn config(&self) -> &BundleConfig {
        &self.config
    }

    /// The store root the trees are under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.placer.root
    }

    /// Clones each of the config's `writable_trees` to `<under>/<tree name>` for `owner`,
    /// and returns where each went. `under` must exist.
    ///
    /// # Errors
    ///
    /// A clone failed; the clone that failed is removed by the sealer, and the ones made
    /// before it stay for the run's directory to take away.
    pub fn clone_writable(
        &self,
        under: &Path,
        owner: Owner,
    ) -> Result<BTreeMap<TreeName, PathBuf>, PlaceError> {
        let mut clones = BTreeMap::new();
        let writable = self
            .handles
            .iter()
            .filter(|(name, _)| self.config.writable_trees.contains(name));
        for (name, handle) in writable {
            let dst = under.join(name.as_str());
            self.placer
                .sealer
                .clone_writable(handle.path(), &dst, owner)?;
            clones.insert(name.clone(), dst);
        }
        Ok(clones)
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        for (_, handle) in self.handles.drain(..) {
            self.placer.trees.unpin(handle);
        }
    }
}
