//! A real bundle, built in memory, and the pieces to place it: served by the in-process
//! registry or written as an OCI image-layout directory, sealed by a sealer that only
//! copies, and opened as the store of one agent.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser as _;
use js_agent::Cli;
use js_agent::bundle::{Placer, Settings};
use js_bundle::{BundleConfig, pack_layer};
use js_oci::{Client, Policy};
use js_testkit::oci_registry::Artifact;
use js_treestore::{Owner, SealError, Sealed, Sealer};
use serde_json::{Value, json};

use super::fake::FakeHost;
use crate::support::{Scratch, platform};

/// A tree of a bundle: its name and its files, each as a path, the text and whether it runs.
pub struct Tree {
    pub name: &'static str,
    pub files: Vec<(&'static str, &'static str, bool)>,
}

/// What a bundle is made of. [`Spec::standard`] is the one most tests use.
pub struct Spec {
    pub trees: Vec<Tree>,
    /// `(name, path)`, the path as the config writes it (`${tree:tool}/bin/tool`).
    pub entrypoints: Vec<(&'static str, &'static str)>,
    /// Environment rules, as the config writes them.
    pub env: Vec<Value>,
    pub writable: Vec<&'static str>,
    /// The `network_max` of the config.
    pub network_max: &'static str,
}

impl Spec {
    /// Two trees: `tool`, with a program, a data file and a file that does not run, and
    /// `work`, which is writable. `@bundle/tool` is the program, `@bundle/data` names a file
    /// that cannot run, `@bundle/dir` a directory, `@bundle/absent` a file the tree lacks and
    /// `@bundle/work` a program in the writable tree. `TOOLROOT` points at the first tree and
    /// `WORKROOT` (locked) at the second.
    pub fn standard() -> Self {
        Self {
            trees: vec![
                Tree {
                    name: "tool",
                    files: vec![
                        ("bin/tool", "#!/bin/sh\necho tool\n", true),
                        ("lib/data.txt", "data\n", false),
                    ],
                },
                Tree {
                    name: "work",
                    files: vec![("run.sh", "#!/bin/sh\necho work\n", true)],
                },
            ],
            entrypoints: vec![
                ("tool", "${tree:tool}/bin/tool"),
                ("data", "${tree:tool}/lib/data.txt"),
                ("work", "${tree:work}/run.sh"),
                ("dir", "${tree:tool}/lib"),
                ("absent", "${tree:tool}/bin/absent"),
            ],
            env: vec![
                json!({"name": "TOOLROOT", "op": "set", "values": ["${tree:tool}"], "locked": false}),
                json!({"name": "WORKROOT", "op": "set", "values": ["${tree:work}"], "locked": true}),
            ],
            writable: vec!["work"],
            network_max: "loopback",
        }
    }

    /// The bundle, its trees written below `work` (a directory that exists).
    pub fn build(&self, work: &Path) -> Bundle {
        let mut layers = Vec::new();
        let mut blobs = Vec::new();
        for tree in &self.trees {
            let root = work.join(tree.name);
            for (path, text, runs) in &tree.files {
                let file = root.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(&file, text).unwrap();
                let mode = if *runs { 0o755 } else { 0o644 };
                fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            }
            let packed = pack_layer(&root).unwrap();
            layers.push(json!({
                "tree": tree.name,
                "diff_id": packed.diff_id.as_str(),
                "uncompressed_bytes": packed.uncompressed_bytes,
                "entries": packed.entries,
                "license": "MIT",
                "source_url": "https://example.invalid/source",
            }));
            blobs.push(packed.blob);
        }
        let entrypoints: serde_json::Map<String, Value> = self
            .entrypoints
            .iter()
            .map(|(name, path)| ((*name).to_owned(), json!({ "path": path })))
            .collect();
        let config = json!({
            "schema": 1,
            "os": "darwin",
            "architecture": "arm64",
            "os.version": "14.0",
            "name": "toolchain",
            "version": "1",
            "layers": layers,
            "env": self.env,
            "system_path": false,
            "entrypoints": entrypoints,
            "writable_trees": self.writable,
            "seatbelt": {"reads": [], "network_max": self.network_max},
            "built": {"deployment_target": "14.0", "tool": "tests 1"},
        });
        let bytes = serde_json::to_vec(&config).unwrap();
        let config = BundleConfig::parse(&bytes).expect("a valid config");
        Bundle {
            artifact: Artifact::bundle_with_config(blobs, &String::from_utf8(bytes).unwrap()),
            config,
        }
    }
}

/// A bundle as the registry serves it, and the config it carries.
pub struct Bundle {
    pub artifact: Artifact,
    pub config: BundleConfig,
}

impl Bundle {
    /// The manifest digest, `sha256:<hex>`.
    pub fn digest(&self) -> String {
        self.artifact.digest.to_string()
    }

    /// A job's pin of the bundle.
    pub fn pin(&self) -> String {
        format!("toolchain@{}", self.digest())
    }

    /// The bundle written as an OCI layout directory `name` below `dir`.
    pub fn layout(&self, dir: &Scratch, name: &str) -> PathBuf {
        let root = dir.path(name);
        fs::create_dir_all(&root).unwrap();
        js_testkit::oci_registry::write_layout(&root, &self.artifact);
        root
    }
}

/// `src` copied to `dst`, which must not exist: directories `0700`, files with their own
/// mode. Stands for the cloning of a sealed tree.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir(dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        match entry.file_type()?.is_dir() {
            true => copy_tree(&entry.path(), &to)?,
            false => {
                fs::copy(entry.path(), &to)?;
            }
        }
    }
    Ok(())
}

/// A sealer that seals nothing and clones by copying; the tests run as an ordinary user on
/// a file system with no immutable flag.
pub struct CopySealer;

impl Sealer for CopySealer {
    fn seal(&self, _stage: &Path) -> Result<Sealed, SealError> {
        Ok(Sealed { bytes: 1 })
    }

    fn unseal_for_delete(&self, _tree: &Path) -> Result<(), SealError> {
        Ok(())
    }

    fn clone_writable(&self, src: &Path, dst: &Path, _owner: Owner) -> Result<(), SealError> {
        copy_tree(src, dst).map_err(SealError::from)
    }
}

/// A sealer that panics when it seals: what a bug in the platform's sealer looks like.
pub struct PanicSealer;

impl Sealer for PanicSealer {
    fn seal(&self, _stage: &Path) -> Result<Sealed, SealError> {
        panic!("the sealer panics");
    }

    fn unseal_for_delete(&self, _tree: &Path) -> Result<(), SealError> {
        Ok(())
    }

    fn clone_writable(&self, _src: &Path, _dst: &Path, _owner: Owner) -> Result<(), SealError> {
        Ok(())
    }
}

/// The flags of an agent with a store in `dir`, checked: a store and a scratch root that
/// exist, any digest trusted.
pub fn settings(dir: &Scratch) -> Settings {
    let (store, scratch) = (dir.path("store"), dir.path("scratch"));
    for path in [&store, &scratch] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (store, scratch) = (store.display().to_string(), scratch.display().to_string());
    let args = [
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--scratch-root",
        &scratch,
        "--bundle-store",
        &store,
        "--bundle-budget-gib",
        "10",
        "--bundle-trust",
        "digest",
    ];
    let cli = Cli::try_parse_from(args).unwrap();
    let host = FakeHost::new();
    let mac = platform("macos", "aarch64");
    Settings::from_cli(&cli, &mac, &*host).unwrap().unwrap()
}

/// A client that reaches no registry.
pub fn no_registry() -> Client {
    Client::new(Policy::new(Vec::new()))
}

/// The store of the settings in `dir`, opened with the copying sealer and a volume that
/// always has room, reaching registries through `client`.
pub fn placer(dir: &Scratch, client: Client) -> Arc<Placer> {
    placer_with(dir, Arc::new(CopySealer), client)
}

/// The same, sealed by `sealer`.
pub fn placer_with(dir: &Scratch, sealer: Arc<dyn Sealer>, client: Client) -> Arc<Placer> {
    Placer::open(&settings(dir), sealer, Arc::new(|| 1 << 40), client).unwrap()
}
