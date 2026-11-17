//! A real bundle store for the native tests (a macOS build's): `Wired` reads what the store holds
//! from its directories, and `Served` and `rig` give it a registry in memory to fetch from.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use js_agent::SystemCommands;
use js_agent::bundle::{
    Admission, BundleHost, Fetch, HostXcode, Needs, Placer, Pull, Settings, SystemHost,
};
use js_bundle::DottedVersion;
use js_oci::{Allow, Digest};
use js_testkit::oci_registry::{Fixture, REGISTRY, REPO};
use serde_json::json;

use super::fixture::{Bundle, Spec, placer, settings};
use crate::support::Scratch;

/// A bundle host over a real store: what the store holds is read from its directories, and a
/// fetch goes to the store's own `Placer`, whose client reaches an in-memory registry. The
/// volume numbers and the macOS version are fixed, as no Mac is here.
pub struct Wired {
    system: SystemHost<SystemCommands>,
    store: Arc<Placer>,
}

impl Wired {
    /// The host over `store`.
    pub fn new(store: Arc<Placer>) -> Arc<Self> {
        let owner = rustix::process::geteuid().as_raw();
        let system = SystemHost::new(SystemCommands::default()).with_owner(owner);
        Arc::new(Self { system, store })
    }
}

impl BundleHost for Wired {
    fn check_start(&self, _store: &Path, _scratch: &Path) -> Result<(), String> {
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<String, String> {
        self.system.read_file(path)
    }

    fn held(&self, store: &Path) -> Result<Vec<String>, String> {
        self.system.held(store)
    }

    fn resolve(&self, store: &Path, digest: &str) -> String {
        self.system.resolve(store, digest)
    }

    fn needs(&self, store: &Path, digest: &str) -> Result<Needs, String> {
        self.system.needs(store, digest)
    }

    fn os_version(&self) -> Result<DottedVersion, String> {
        DottedVersion::parse("14.8").map_err(|why| why.to_string())
    }

    fn xcodes(&self) -> Vec<HostXcode> {
        Vec::new()
    }

    fn used_bytes(&self, _store: &Path) -> Result<u64, String> {
        Ok(0)
    }

    fn free_bytes(&self, _path: &Path) -> Result<u64, String> {
        Ok(1 << 40)
    }

    fn can_run(&self) -> bool {
        true
    }

    fn fetch(&self, _store: &Path, name: &str, digest: &str) -> Fetch {
        self.store.fetch(name, digest)
    }

    fn open(&self, _settings: &Settings) -> Result<Option<Arc<Placer>>, String> {
        Ok(Some(Arc::clone(&self.store)))
    }
}

/// The digest of a Linux manifest nobody serves.
fn linux(arch: &str) -> Digest {
    Digest::of(format!("linux {arch}").as_bytes())
}

/// An index that lists two Linux manifests nobody serves and `darwin` for darwin/arm64.
fn index_of(darwin: &Digest) -> (Digest, Vec<u8>) {
    let entry = |os: &str, arch: &str, digest: &Digest| {
        json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest.to_string(),
            "size": 1,
            "platform": {"os": os, "architecture": arch},
        })
    };
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            entry("linux", "amd64", &linux("amd64")),
            entry("linux", "arm64", &linux("arm64")),
            entry("darwin", "arm64", darwin),
        ],
    });
    let bytes = serde_json::to_vec(&index).unwrap();
    (Digest::of(&bytes), bytes)
}

/// A registry in memory that serves the standard bundle and an index over it.
pub struct Served {
    _runtime: tokio::runtime::Runtime,
    /// The registry.
    pub fx: Fixture,
    /// The bundle, as built.
    pub bundle: Bundle,
    /// The digest of the index that lists the bundle for darwin/arm64.
    pub index: Digest,
}

impl Served {
    /// The registry, with the bundle built in `dir`.
    pub fn new(dir: &Scratch) -> Self {
        let src = dir.path("src");
        fs::create_dir_all(&src).unwrap();
        let bundle = Spec::standard().build(&src);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fx = Fixture::in_memory();
        fx.publish(&bundle.artifact);
        let (index, bytes) = index_of(&bundle.artifact.digest);
        fx.publish_manifest_as(&index, &bytes);
        Self {
            _runtime: runtime,
            fx,
            bundle,
            index,
        }
    }

    /// The `--image-allow` entry of the registry's `team/` namespace.
    pub fn entry(&self) -> String {
        format!("{}/team", self.fx.at(REGISTRY))
    }

    /// The image reference that names the index.
    pub fn image(&self) -> String {
        format!("{}/{REPO}@{}", self.fx.at(REGISTRY), self.index)
    }
}

/// The store of `dir` over the registry, and the admission that judges starts by it: it
/// fetches what it lacks (`--bundle-pull missing`) from the images the `--image-allow`
/// `entries` allow, and `change` may alter the settings first.
pub fn rig(
    dir: &Scratch,
    served: &Served,
    entries: &[String],
    change: impl FnOnce(&mut Settings),
) -> (Arc<Placer>, Admission) {
    let store = placer(dir, served.fx.client());
    let mut rules = settings(dir);
    rules.pull = Pull::Missing;
    rules.image_allow = entries.iter().map(|e| Allow::parse(e).unwrap()).collect();
    change(&mut rules);
    let admission = Admission::new(rules, Wired::new(Arc::clone(&store)));
    (store, admission)
}
