//! The seam between the bundle rules and the machine: [`BundleHost`].

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use js_bundle::{BundleConfig, DottedVersion, XcodeComponent, XcodeNeed};

use super::list::is_digest;
use crate::facts::{CommandRunner, free_bytes};

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
}

/// The machine's own host.
#[derive(Debug, Clone, Copy)]
pub struct SystemHost<R> {
    runner: R,
    /// The user that must own a list file and the directory it is in: root.
    owner: u32,
}

impl<R: CommandRunner + Send + Sync> SystemHost<R> {
    /// A host that runs its helpers (`sw_vers`, `df`) with `runner`.
    pub fn new(runner: R) -> Self {
        Self { runner, owner: 0 }
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

/// The `<hex>` of `sha256:<hex>` when the text is a digest, as a file name.
fn hex_of(digest: &str) -> Result<&str, String> {
    match is_digest(digest) {
        true => Ok(&digest["sha256:".len()..]),
        false => Err("not a sha256 digest".to_owned()),
    }
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

impl<R: CommandRunner + Send + Sync> BundleHost for SystemHost<R> {
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
        let hex = hex_of(digest)?;
        let file = manifests(store).join(format!("{hex}.json"));
        let manifest = self.read_file(&file)?;
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest).map_err(|_| "the manifest is not JSON".to_owned())?;
        let config = manifest
            .pointer("/config/digest")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "the manifest names no config".to_owned())?;
        let config = hex_of(config)?;
        let file = store.join(format!("v1/objects/configs/{config}.json"));
        let bytes = fs::read(&file).map_err(|why| format!("{}: {why}", file.display()))?;
        let config = BundleConfig::parse(&bytes).map_err(|why| why.to_string())?;
        Ok(Needs {
            os_version: config.os_version,
            xcode: config.xcode,
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
        // There is no bundle runtime yet.
        false
    }

    fn fetch(&self, _store: &Path, _name: &str, _digest: &str) -> Fetch {
        Fetch::Failed("this agent has no bundle fetcher yet".to_owned())
    }
}
