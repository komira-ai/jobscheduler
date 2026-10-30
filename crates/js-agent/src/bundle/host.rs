//! The seam between the bundle rules and the machine: [`BundleHost`].

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use js_bundle::{DottedVersion, XcodeComponent, XcodeNeed};
use js_oci::{Allow, Client, Policy};
use js_treestore::Sealer;
#[cfg(target_os = "macos")]
use js_treestore::mac::{Clonefile, Fchflags, MacSealer, Owner};

use super::list::is_digest;
use super::place::{Placer, load_config};
use super::settings::Settings;
use crate::facts::{CommandRunner, SystemCommands, free_bytes};

/// `sw_vers`, by its absolute path like every helper the agent runs.
const SW_VERS: &str = "/usr/bin/sw_vers";

/// The most bytes a list file or a manifest may have: far above a list of 100,000 digests.
pub const MAX_LIST_BYTES: u64 = 8 << 20;

/// What a bundle needs of the host, read from its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needs {
    /// The lowest macOS the bundle's tools run on.
    pub os_version: DottedVersion,
    /// The Xcode it calls into, when it does.
    pub xcode: Option<XcodeNeed>,
    /// The names of its entrypoints, which a job names as `@bundle/<name>`.
    pub entrypoints: BTreeSet<String>,
}

/// An Xcode the host has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostXcode {
    /// The build id, such as `16C5032a`.
    pub build: String,
    /// Its version.
    pub version: DottedVersion,
    /// The components it has.
    pub components: Vec<XcodeComponent>,
}

/// How an attempt to begin a fetch went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// The fetch is running; the bundle is not there yet.
    Started,
    /// The digest names an index, not a manifest.
    Index,
    /// The fetch cannot run or did not verify; the text says why.
    Failed(String),
}

/// What the agent reads from and asks of the machine for bundles. Every method answers from
/// the machine as it is now: nothing is cached, so a change to a list file or to the store
/// is seen by the next start.
pub trait BundleHost: Send + Sync {
    /// Checks the store's volume and the kernel's securelevel against the scratch root's
    /// volume (`js_treestore::mac::check_volumes` and `check_securelevel`).
    fn check_start(&self, store: &Path, scratch: &Path) -> Result<(), String>;

    /// The text of a list file, or why it cannot be read. A list file is the host's trust
    /// root: the real host reads it only when it is a regular file of root (reached with no
    /// link), writable by nobody else, in a directory that is likewise, and of a bounded size.
    fn read_file(&self, path: &Path) -> Result<String, String>;

    /// The manifest digests the store holds (`sha256:<hex>`), ascending. A store with no
    /// manifests yet holds none.
    fn held(&self, store: &Path) -> Result<Vec<String>, String>;

    /// What the held bundle `digest` needs of the host.
    fn needs(&self, store: &Path, digest: &str) -> Result<Needs, String>;

    /// The host's macOS version.
    fn os_version(&self) -> Result<DottedVersion, String>;

    /// The Xcode builds the host has.
    fn xcodes(&self) -> Vec<HostXcode>;

    /// The bytes the store's trees take, the trees waiting for deletion included.
    fn used_bytes(&self, store: &Path) -> Result<u64, String>;

    /// The free bytes of the volume `path` is on.
    fn free_bytes(&self, path: &Path) -> Result<u64, String>;

    /// Whether this agent can run a bundle it holds. The hello and the `Caps` frame
    /// advertise bundles only when it can: a server that placed work by `bundle` on a host
    /// that cannot run one would see every start end in a refusal.
    fn can_run(&self) -> bool;

    /// Begins to fetch the bundle `name` (a pin's name, with its tag if it has one) with
    /// manifest digest `digest` into the store.
    fn fetch(&self, store: &Path, name: &str, digest: &str) -> Fetch;

    /// Opens the store `settings` names, for an agent that runs native jobs, and gives the
    /// [`Placer`] the runtime pins bundles through; afterwards [`BundleHost::can_run`] and
    /// [`BundleHost::fetch`] answer from it. `None` is a host that cannot place bundles (and
    /// so cannot run one).
    ///
    /// # Errors
    ///
    /// The store cannot be opened or the registries cannot be turned into a policy; the text
    /// says why.
    fn open(&self, settings: &Settings) -> Result<Option<Arc<Placer>>, String>;
}

/// The machine's own host.
#[derive(Clone)]
pub struct SystemHost<R> {
    runner: R,
    /// The user that must own a list file and the directory it is in: root.
    owner: u32,
    /// What seals the trees of the store; none on a host that has no sealer.
    sealer: Option<Arc<dyn Sealer>>,
    /// The store, once [`BundleHost::open`] has opened it.
    placer: OnceLock<Arc<Placer>>,
}

impl SystemHost<SystemCommands> {
    /// The machine's own host: its helpers are the system's, and on a Mac its trees are
    /// sealed by the Mac's sealer (the agent runs as root, trees end `root:wheel` with
    /// `uchg`, writable copies are `clonefile`s). A host that is not a Mac has no sealer, so
    /// it opens no store.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn system() -> Self {
        let sealer = MacSealer::new(Owner::root(), Fchflags, Clonefile);
        Self::new(SystemCommands::default()).with_sealer(Arc::new(sealer))
    }

    /// The machine's own host: its helpers are the system's. A host that is not a Mac has
    /// no sealer, so it opens no store.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub fn system() -> Self {
        Self::new(SystemCommands::default())
    }
}

impl<R: CommandRunner + Send + Sync> SystemHost<R> {
    /// A host that runs its helpers (`sw_vers`, `df`) with `runner`.
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            owner: 0,
            sealer: None,
            placer: OnceLock::new(),
        }
    }

    /// The same host, with `sealer` to seal the trees of its store (the Mac's own, which
    /// runs as root, or a test's).
    #[must_use]
    pub fn with_sealer(self, sealer: Arc<dyn Sealer>) -> Self {
        Self {
            sealer: Some(sealer),
            ..self
        }
    }

    /// The same host, with list files owned by `owner` instead of root (for a test that
    /// cannot be root).
    #[must_use]
    pub fn with_owner(self, owner: u32) -> Self {
        Self { owner, ..self }
    }
}

/// The directory of the manifests under a store.
fn manifests(store: &Path) -> PathBuf {
    store.join("v1/objects/manifests")
}

/// The bytes below `dir` by allocated blocks, links not followed. A missing directory is
/// empty.
fn blocks_below(dir: &Path) -> io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry?;
        let meta = entry.metadata()?;
        total = total.saturating_add(meta.blocks().saturating_mul(512));
        if meta.is_dir() {
            total = total.saturating_add(blocks_below(&entry.path())?);
        }
    }
    Ok(total)
}

/// Whether `meta` is of `owner` and writable by nobody else.
fn trusted(meta: &fs::Metadata, owner: u32) -> bool {
    meta.uid() == owner && meta.mode() & 0o022 == 0
}

/// Reads a file the host trusts: opened without following a link and without blocking, then
/// looked at through the descriptor (so the file looked at is the file read). It must be a
/// regular file of `owner`, writable by nobody else, no larger than [`MAX_LIST_BYTES`], in a
/// directory that is likewise. A link, a pipe, a device, a file a pool user could change
/// and a file of unbounded size are all refused: the allow list is the host's trust root.
fn read_trusted(path: &Path, owner: u32) -> io::Result<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    if !trusted(&meta, owner) {
        return Err(io::Error::other(
            "not owned by the expected user, or writable by group or others",
        ));
    }
    if meta.len() > MAX_LIST_BYTES {
        return Err(io::Error::other("larger than the size cap"));
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    if !trusted(&fs::metadata(parent)?, owner) {
        return Err(io::Error::other(
            "its directory is not owned by the expected user, or is writable by group or others",
        ));
    }
    let mut text = String::new();
    file.take(MAX_LIST_BYTES).read_to_string(&mut text)?;
    Ok(text)
}

impl<R: CommandRunner + Clone + Send + Sync + 'static> BundleHost for SystemHost<R> {
    #[cfg(target_os = "macos")]
    fn check_start(&self, store: &Path, scratch: &Path) -> Result<(), String> {
        js_treestore::mac::start_checks(store, scratch).map_err(|why| why.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    fn check_start(&self, _store: &Path, _scratch: &Path) -> Result<(), String> {
        Err("the store checks run on macOS only".to_owned())
    }

    fn read_file(&self, path: &Path) -> Result<String, String> {
        read_trusted(path, self.owner).map_err(|why| format!("{}: {why}", path.display()))
    }

    fn held(&self, store: &Path) -> Result<Vec<String>, String> {
        let dir = manifests(store);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("{}: {err}", dir.display())),
        };
        let mut held = Vec::new();
        // An entry that cannot be read is not a manifest the store can show.
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let digest = name
                .strip_suffix(".json")
                .map(|hex| format!("sha256:{hex}"));
            held.extend(digest.filter(|digest| is_digest(digest)));
        }
        held.sort();
        Ok(held)
    }

    fn needs(&self, store: &Path, digest: &str) -> Result<Needs, String> {
        let config = load_config(store, digest, &|path| self.read_file(path))?;
        Ok(Needs {
            os_version: config.os_version,
            xcode: config.xcode,
            entrypoints: config.entrypoints.keys().cloned().collect(),
        })
    }

    fn os_version(&self) -> Result<DottedVersion, String> {
        let text = self.runner.run(SW_VERS, &["-productVersion"])?;
        let text = text.trim();
        // `sw_vers` prints `15` for a release with no minor number.
        let text = match text.contains('.') {
            true => text.to_owned(),
            false => format!("{text}.0"),
        };
        DottedVersion::parse(&text).map_err(|_| "sw_vers printed no version".to_owned())
    }

    fn xcodes(&self) -> Vec<HostXcode> {
        // The Xcode catalog arrives with the profile builder; until then a host reports
        // none, and a bundle that needs Xcode is refused `bundle-xcode-mismatch`.
        Vec::new()
    }

    fn used_bytes(&self, store: &Path) -> Result<u64, String> {
        let below = |dir: &str| blocks_below(&store.join(dir));
        let both = below("v1/objects/trees")
            .and_then(|trees| below("v1/trash").map(|trash| trees.saturating_add(trash)));
        both.map_err(|why| format!("{}: {why}", store.display()))
    }

    fn free_bytes(&self, path: &Path) -> Result<u64, String> {
        free_bytes(&self.runner, path)
    }

    fn can_run(&self) -> bool {
        self.placer.get().is_some()
    }

    fn open(&self, settings: &Settings) -> Result<Option<Arc<Placer>>, String> {
        if let Some(placer) = self.placer.get() {
            return Ok(Some(Arc::clone(placer)));
        }
        let Some(sealer) = &self.sealer else {
            return Ok(None);
        };
        let allowed = settings.registries.iter().map(|registry| {
            let entry = Allow::parse(&format!("{}/{}", registry.host, registry.prefix))?;
            Ok(if registry.private {
                entry.private()
            } else {
                entry
            })
        });
        let allowed = allowed.collect::<Result<Vec<_>, js_oci::Error>>();
        let allowed = allowed.map_err(|why| format!("--bundle-registry-allow: {why}"))?;
        // The volume's free space is asked at every eviction decision, so it is read again
        // each time.
        let (runner, store) = (self.runner.clone(), settings.store.clone());
        let free: Arc<dyn Fn() -> u64 + Send + Sync> =
            Arc::new(move || free_bytes(&runner, &store).unwrap_or(0));
        let client = Client::new(Policy::new(allowed));
        let opened = Placer::open(settings, Arc::clone(sealer), free, client);
        let placer = opened.map_err(|why| format!("{}: {why}", settings.store.display()))?;
        Ok(Some(Arc::clone(self.placer.get_or_init(|| placer))))
    }

    fn fetch(&self, _store: &Path, name: &str, digest: &str) -> Fetch {
        match self.placer.get() {
            Some(placer) => placer.fetch(name, digest),
            None => Fetch::Failed("this agent has no bundle store to fetch into".to_owned()),
        }
    }
}
