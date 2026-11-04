//! A start named by its image on a Mac: the image reference is the bundle's name, so it is
//! held to the bundle rules (deny list, allow list, platform, the store, the entrypoint)
//! and, for its registry, to `--image-allow`. The rules run over a fake host; `Wired` and
//! `Served` give the native tests a real store that fetches from a registry in memory. Each
//! test says the defect it catches.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use js_agent::bundle::{
    Admission, BundleHost, Fetch, HostXcode, Needs, Placer, Pull, Registry, Settings, SystemHost,
};
use js_agent::{Cli, Platform, SystemCommands};
use js_agent_wire::{JobSpec, refusal};
use js_bundle::DottedVersion;
use js_oci::{Allow, Digest};
use js_testkit::oci_registry::{Fixture, REGISTRY, REPO};
use js_types::{Arch, Os};
use serde_json::json;

use super::fake::{FakeHost, digest};
use super::fixture::{Bundle, Spec, placer, settings};
use crate::support::{Scratch, platform};

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

const NAME: &str = "ghcr.io/komira-ai/js/toolchain:1";

fn mac() -> Platform {
    platform("macos", "aarch64")
}

/// A start named by `image` that runs `program`: no `native` body and no `bundle`.
fn start(image: &str, program: &str) -> JobSpec {
    serde_json::from_value(json!({
        "run_id": "r1",
        "lease_id": "l1",
        "image": image,
        "run": { "program": program },
        "limits": { "cpu_millis": 1000, "memory_bytes": 1 << 30, "wall_ms": 60000 }
    }))
    .unwrap()
}

fn pinned() -> JobSpec {
    start(&format!("{NAME}@{}", digest('a')), "@bundle/clang")
}

/// The legacy form of the start `pinned` is: a `native` body and a `bundle` pin, no image.
fn legacy(pin: &str) -> JobSpec {
    let mut spec = pinned();
    spec.run = None;
    spec.image = String::new();
    spec.native = Some(serde_json::from_value(json!({"program": "@bundle/clang"})).unwrap());
    spec.bundle = Some(pin.to_owned());
    spec
}

/// A host that holds the bundle `a`, which needs macOS 14.0 and no Xcode, on macOS 14.8, with
/// an allow list that names `a` and `c`.
fn host() -> Arc<FakeHost> {
    let host = FakeHost::new();
    host.file(
        "/etc/js/allow",
        &format!("{}\n{}\n", digest('a'), digest('c')),
    );
    host.set(|s| {
        s.held = Ok(vec![digest('a')]);
        s.needs.insert(
            digest('a'),
            Needs {
                os_version: DottedVersion::parse("14.0").unwrap(),
                xcode: None,
                entrypoints: ["clang".to_owned()].into(),
            },
        );
    });
    host
}

/// The settings of a Mac with a store, trust `list` and the `extra` flags.
fn settings_of(host: &FakeHost, extra: &[&str]) -> Result<Settings, String> {
    let mut args = vec![
        "js-agent",
        "--server",
        "http://127.0.0.1:1",
        "--pool",
        "macs",
        "--bundle-store",
        "/Volumes/work/bundles",
        "--scratch-root",
        "/Volumes/work/scratch",
        "--bundle-budget-gib",
        "10",
        "--bundle-allow-file",
        "/etc/js/allow",
    ];
    args.extend_from_slice(extra);
    let cli = Cli::try_parse_from(args).unwrap();
    Settings::from_cli(&cli, &mac(), host).map(Option::unwrap)
}

/// The admission of `host` with the `extra` flags.
fn admission(host: &Arc<FakeHost>, extra: &[&str]) -> Admission {
    Admission::new(settings_of(host, extra).unwrap(), host.clone())
}

/// The code a start is refused with; panics when it is admitted.
fn refused(admission: &Admission, spec: &JobSpec) -> &'static str {
    admission.admit(spec).unwrap_err().0
}

/// A start named by its image with a `run` is a bundle start on a Mac: it passes when the
/// image is on an `--image-allow` entry, the store holds it and the program is one of its
/// entrypoints (or not the bundle's at all), and it is refused `invalid-program` for an
/// entrypoint the bundle lacks, by the `run`'s program. An image start with no `run`, and a
/// start with neither image start nor bundle, are none of this module's business. It catches
/// an image start waved through unchecked, and an entrypoint read only from `native`.
#[test]
fn an_image_start_is_admitted_as_a_bundle_start() {
    let host = host();
    let admission = admission(&host, &["--image-allow", "ghcr.io/komira-ai/"]);
    assert_eq!(admission.admit(&pinned()), Ok(()));
    let mut host_program = pinned();
    host_program.run.as_mut().unwrap().program = "/usr/bin/true".to_owned();
    assert_eq!(admission.admit(&host_program), Ok(()));
    let mut lacks = pinned();
    lacks.run.as_mut().unwrap().program = "@bundle/gcc".to_owned();
    let (code, why) = admission.admit(&lacks).unwrap_err();
    assert_eq!(code, refusal::INVALID_PROGRAM);
    assert!(why.contains("gcc"), "{why}");
    let mut no_run = pinned();
    no_run.run = None;
    assert_eq!(admission.admit(&no_run), Ok(()));
}

/// An image that is not a pin (which the wire has refused already) is refused
/// `invalid-image`, not `invalid-bundle`, and not a panic. It catches an image start that
/// reports a field it does not have.
#[test]
fn an_image_that_is_not_a_pin_is_refused_as_an_invalid_image() {
    let host = host();
    let admission = admission(&host, &["--image-allow", "ghcr.io/komira-ai/"]);
    let spec = start("toolchain:latest", "@bundle/clang");
    assert_eq!(refused(&admission, &spec), refusal::INVALID_IMAGE);
}

/// An `--image-allow` entry admits its namespace by whole path components, with or without
/// a tag, and nothing beside it: not half a component, another host, another port, a name
/// the registry grammar cannot parse, or any image when no entry is given. A bare host is
/// refused when the agent starts. The refusal is `image-not-allowed` and nothing is fetched.
/// It catches a prefix that matches half a component, a tag read as path, and an empty list
/// that means "any".
#[test]
fn an_image_allow_entry_admits_its_namespace_and_nothing_beside_it() {
    let host = host();
    let ghcr = admission(&host, &["--image-allow", "ghcr.io/komira-ai/js"]);
    let at = |name: &str| start(&format!("{name}@{}", digest('a')), "@bundle/clang");
    for name in [
        "ghcr.io/komira-ai/js/toolchain",
        "ghcr.io/komira-ai/js/toolchain:1.2",
        "ghcr.io/komira-ai/js",
    ] {
        assert_eq!(ghcr.admit(&at(name)), Ok(()), "{name}");
    }
    for name in [
        "ghcr.io/komira-ai/jsx/toolchain",
        "ghcr.io/komira-ai-evil/js/toolchain",
        "ghcr.io/other/js/komira-ai/js",
        "ghcr.io:5000/komira-ai/js/toolchain",
        "docker.io/komira-ai/js/toolchain",
        "ghcr.io/Komira-ai/js/toolchain",
        "toolchain",
    ] {
        let spec = at(name);
        assert_eq!(refused(&ghcr, &spec), refusal::IMAGE_NOT_ALLOWED, "{name}");
    }
    assert!(host.get(|s| s.fetches.is_empty()));
    let none = admission(&host, &[]);
    assert_eq!(refused(&none, &pinned()), refusal::IMAGE_NOT_ALLOWED);
    let bare = settings_of(&host, &["--image-allow", "ghcr.io"]).unwrap_err();
    assert!(bare.starts_with("--image-allow: "), "{bare}");
}

/// The legacy `bundle` pin keeps checking `--bundle-registry-allow*` and never
/// `--image-allow`: a pin outside the bundle registries is refused
/// `bundle-registry-not-allowed` even when an image entry covers it, a pin on a bundle
/// registry passes with no image entry, and the same name as an image on a bundle registry
/// alone is refused `image-not-allowed`. It catches the two lists swapped or merged.
#[test]
fn a_bundle_pin_still_checks_the_bundle_registry_list() {
    let host = host();
    let admission = admission(
        &host,
        &[
            "--bundle-registry-allow",
            "ghcr.io/komira-ai/",
            "--image-allow",
            "docker.io/team/",
        ],
    );
    let native = legacy(&format!("{NAME}@{}", digest('a')));
    assert_eq!(admission.admit(&native), Ok(()));
    let outside = legacy(&format!("docker.io/team/toolchain@{}", digest('a')));
    assert_eq!(
        refused(&admission, &outside),
        refusal::BUNDLE_REGISTRY_NOT_ALLOWED
    );
    let image = start(&format!("{NAME}@{}", digest('a')), "@bundle/clang");
    assert_eq!(refused(&admission, &image), refusal::IMAGE_NOT_ALLOWED);
    let image = start(
        &format!("docker.io/team/toolchain@{}", digest('a')),
        "@bundle/clang",
    );
    assert_eq!(admission.admit(&image), Ok(()));
}

/// An image whose digest is on the deny list is refused `bundle-denied` before its registry
/// is considered, and under trust `list` one off the allow list is refused
/// `bundle-not-allowed`. It catches an image start that skips the revocation list the legacy
/// pin is held to.
#[test]
fn a_deny_list_digest_is_refused_for_an_image_reference() {
    let host = host();
    host.file("/etc/js/deny", &format!("{}\n", digest('a')));
    host.file("/etc/js/allow", &format!("{}\n", digest('b')));
    let denied = admission(
        &host,
        &[
            "--image-allow",
            "ghcr.io/komira-ai/",
            "--bundle-deny-file",
            "/etc/js/deny",
        ],
    );
    assert_eq!(refused(&denied, &pinned()), refusal::BUNDLE_DENIED);
    let outside = start(&format!("docker.io/x/y@{}", digest('a')), "@bundle/clang");
    assert_eq!(refused(&denied, &outside), refusal::BUNDLE_DENIED);
    host.file("/etc/js/allow", &format!("{}\n", digest('b')));
    let listed = admission(&host, &["--image-allow", "ghcr.io/komira-ai/"]);
    assert_eq!(refused(&listed, &pinned()), refusal::BUNDLE_NOT_ALLOWED);
}

/// A pin that names an index whose alias resolves to a manifest on the deny list is refused
/// `bundle-denied`, though the index digest itself is not listed; with the manifest off the
/// list the same pin is not refused for that. It catches a revoked manifest that runs again
/// when a job names it through an index.
#[test]
fn an_index_resolving_to_a_denied_manifest_is_refused() {
    let host = host();
    host.file("/etc/js/deny", &format!("{}\n", digest('a')));
    host.set(|s| {
        s.aliases.insert(digest('c'), digest('a'));
    });
    let denied = admission(
        &host,
        &[
            "--image-allow",
            "ghcr.io/komira-ai/",
            "--bundle-deny-file",
            "/etc/js/deny",
        ],
    );
    let index = start(&format!("{NAME}@{}", digest('c')), "@bundle/clang");
    assert_eq!(refused(&denied, &index), refusal::BUNDLE_DENIED);
    host.file("/etc/js/deny", &format!("{}\n", digest('b')));
    assert_eq!(denied.admit(&index), Ok(()));
}

/// A start that names another platform than darwin/arm64 is refused `platform-mismatch`
/// before the store is asked for anything (no fetch begins), whether it names `os` or
/// `arch`; one that names macOS and arm64, or neither, goes on. A legacy start that names
/// neither is unchanged. It catches a Linux image fetched and run as a bundle, and a check
/// made only after the layers are downloaded.
#[test]
fn a_job_naming_a_different_platform_is_refused() {
    let host = host();
    host.set(|s| s.held = Ok(Vec::new()));
    let admission = admission(
        &host,
        &[
            "--image-allow",
            "ghcr.io/komira-ai/",
            "--bundle-pull",
            "missing",
        ],
    );
    let mut linux = pinned();
    linux.os = Some(Os::Linux);
    let mut amd = pinned();
    amd.arch = Some(Arch::X86_64);
    for spec in [&linux, &amd] {
        assert_eq!(refused(&admission, spec), refusal::PLATFORM_MISMATCH);
    }
    assert!(host.get(|s| s.fetches.is_empty()));
    let mut on_mac = pinned();
    (on_mac.os, on_mac.arch) = (Some(Os::Macos), Some(Arch::Arm64));
    for spec in [&on_mac, &pinned()] {
        assert_eq!(refused(&admission, spec), refusal::BUNDLE_PENDING);
    }
    host.set(|s| s.held = Ok(vec![digest('a')]));
    assert_eq!(
        admission.admit(&legacy(&format!("{NAME}@{}", digest('a')))),
        Ok(())
    );
}

/// A pin that names an index is held through the store's alias: with the alias the bundle
/// is held (the manifest it names is listed), and with `--bundle-pull missing` a pin that the
/// store does not hold begins a fetch keyed on the digest the job named, under the name it
/// gave with its tag. It catches an index digest looked up in the manifest list, which never
/// holds it, and a fetch keyed on the manifest.
#[test]
fn an_image_pin_naming_an_index_is_held_through_its_alias() {
    let host = host();
    let admission = admission(
        &host,
        &[
            "--image-allow",
            "ghcr.io/komira-ai/",
            "--bundle-pull",
            "missing",
        ],
    );
    let index = start(&format!("{NAME}@{}", digest('c')), "@bundle/clang");
    assert_eq!(refused(&admission, &index), refusal::BUNDLE_PENDING);
    assert_eq!(
        host.get(|s| s.fetches.clone()),
        [(NAME.to_owned(), digest('c'))]
    );
    host.set(|s| {
        s.aliases.insert(digest('c'), digest('a'));
    });
    assert_eq!(admission.admit(&index), Ok(()));
    assert_eq!(host.get(|s| s.fetches.len()), 1);
}

/// The bundle registries of a Mac with `--bundle-registry-allow` alone do not admit an
/// image: the entry is built from the settings the same way as the image entries, but the
/// check for an image reads only `--image-allow`. It catches an image start admitted on a
/// legacy registry entry.
#[test]
fn a_bundle_registry_entry_alone_does_not_admit_an_image() {
    let host = host();
    let admission = admission(&host, &["--bundle-registry-allow", "ghcr.io/komira-ai/"]);
    assert_eq!(refused(&admission, &pinned()), refusal::IMAGE_NOT_ALLOWED);
    let mut rules = settings_of(&host, &["--bundle-registry-allow", "ghcr.io/komira-ai/"]).unwrap();
    assert_eq!(
        rules.registries,
        [Registry {
            host: "ghcr.io".to_owned(),
            prefix: "komira-ai".to_owned(),
            private: false
        }]
    );
    rules.image_allow = vec![Allow::parse("ghcr.io/komira-ai/").unwrap()];
    let both = Admission::new(rules, host.clone());
    assert_eq!(both.admit(&pinned()), Ok(()));
}
