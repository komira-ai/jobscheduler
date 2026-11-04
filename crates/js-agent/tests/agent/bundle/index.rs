//! A pin that names an image index: the store records the platform manifest the index
//! resolved to, and a pin by the index digest or by the manifest digest reaches the same
//! trees. Each test says the defect it catches.

use std::fs;
use std::path::Path;

use js_agent::SystemCommands;
use js_agent::bundle::{BundleHost, PlaceError, SystemHost, manifest_of};
use js_oci::Digest;
use js_testkit::oci_registry::{Fixture, REGISTRY, REPO, blob_path, write_index};
use serde_json::json;

use super::fixture::{Bundle, Spec, no_registry, placer};
use crate::support::Scratch;

/// A bundle of the standard spec in `dir`.
fn standard(dir: &Scratch) -> Bundle {
    let src = dir.path("src");
    fs::create_dir_all(&src).unwrap();
    Spec::standard().build(&src)
}

/// The entries of `dir`, by name.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The host of the real store, whose list files are owned by the user running the tests.
fn host() -> SystemHost<SystemCommands> {
    SystemHost::new(SystemCommands::default()).with_owner(rustix::process::geteuid().as_raw())
}

/// The digest of a Linux manifest nobody serves.
fn linux(arch: &str) -> Digest {
    Digest::of(format!("linux {arch}").as_bytes())
}

/// An index that lists a Linux manifest for amd64 and arm64 and `darwin` for darwin/arm64.
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

/// A pin of the bundle by `digest`.
fn pin(digest: &Digest) -> String {
    format!("toolchain@{digest}")
}

/// A registry that serves `bundle` and an index over it, and the runtime it runs on.
fn served(bundle: &Bundle) -> (tokio::runtime::Runtime, Fixture, Digest) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = runtime.block_on(Fixture::start());
    fixture.publish(&bundle.artifact);
    let (digest, bytes) = index_of(&bundle.artifact.digest);
    fixture.publish_manifest_as(&digest, &bytes);
    (runtime, fixture, digest)
}

/// After one fetch by an index digest the store holds the darwin manifest and the alias
/// `indexes/<index digest>` naming it, and pulled neither Linux child; a pin by the index
/// digest and a pin by the manifest digest give the same bundle (the host's list stays the
/// manifest digests); and once the manifest is gone the alias is dangling: the bundle is not
/// held, and the pin by either digest is refused with nothing pinned. Catches an alias that
/// is written before the manifest, a pin by an index that reaches another manifest, a listing
/// of the index digest as a bundle, and a dangling alias that still pins.
#[test]
fn an_index_pin_is_held_after_the_fetch_and_resolves_to_the_darwin_manifest() {
    let dir = Scratch::new("index-pin");
    let bundle = standard(&dir);
    let (_runtime, fixture, index) = served(&bundle);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    store.ensure_registry(&name, &index.to_string()).unwrap();

    let objects = store.root().join("v1/objects");
    let manifest = bundle.artifact.digest.clone();
    assert_eq!(
        names(&objects.join("manifests")),
        [format!("{}.json", manifest.hex())]
    );
    assert_eq!(names(&objects.join("indexes")), [index.hex()]);
    let alias = fs::read_to_string(objects.join("indexes").join(index.hex())).unwrap();
    assert_eq!(alias, manifest.to_string());
    let log = fixture.requests();
    assert!(log.iter().any(|line| line.contains(manifest.hex())));
    for arch in ["amd64", "arm64"] {
        assert!(log.iter().all(|line| !line.contains(linux(arch).hex())));
    }

    let host = host();
    assert_eq!(
        host.resolve(store.root(), &index.to_string()),
        manifest.to_string()
    );
    assert_eq!(
        host.resolve(store.root(), &manifest.to_string()),
        manifest.to_string()
    );
    assert_eq!(host.held(store.root()), Ok(vec![manifest.to_string()]));

    let by_index = store.pin(&pin(&index), "lease-1").unwrap();
    let by_manifest = store.pin(&pin(&manifest), "lease-2").unwrap();
    assert_eq!(by_index.config(), &bundle.config);
    assert_eq!(by_manifest.config(), by_index.config());
    drop((by_index, by_manifest));

    fs::remove_file(objects.join(format!("manifests/{}.json", manifest.hex()))).unwrap();
    assert_eq!(host.held(store.root()), Ok(Vec::new()));
    for digest in [&index, &manifest] {
        let why = store.pin(&pin(digest), "lease-3").unwrap_err();
        assert!(matches!(why, PlaceError::Held(_)), "{why}");
    }
    // Nothing was left pinned by the refusals: the run id is free to pin again.
    fs::write(
        objects.join(format!("manifests/{}.json", manifest.hex())),
        &bundle.artifact.manifest,
    )
    .unwrap();
    store.pin(&pin(&index), "lease-3").unwrap();
}

/// An alias that is not a digest (or is not there) leaves the pin standing for itself, which
/// names no held manifest, so the bundle is not held and nothing is pinned; a digest that is
/// not even a digest stands for itself too, and an alias with a trailing newline is read.
/// Catches a damaged alias that pins some other manifest or panics, and a reader that trusts
/// the alias text unchecked.
#[test]
fn a_damaged_alias_makes_the_bundle_not_held() {
    let dir = Scratch::new("index-damaged");
    let bundle = standard(&dir);
    let (_runtime, fixture, index) = served(&bundle);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    store.ensure_registry(&name, &index.to_string()).unwrap();
    let alias = store.root().join("v1/objects/indexes").join(index.hex());
    let host = host();
    for damaged in ["not a digest", "sha256:abc", ""] {
        fs::write(&alias, damaged).unwrap();
        assert_eq!(
            host.resolve(store.root(), &index.to_string()),
            index.to_string(),
            "{damaged}"
        );
        let why = store.pin(&pin(&index), "lease-1").unwrap_err();
        assert!(matches!(why, PlaceError::Held(_)), "{why}");
    }
    fs::write(&alias, format!("{}\n", bundle.artifact.digest)).unwrap();
    let read = |path: &Path| fs::read_to_string(path).map_err(|why| why.to_string());
    assert_eq!(
        manifest_of(store.root(), &index.to_string(), &read),
        bundle.digest()
    );
    fs::remove_file(&alias).unwrap();
    assert_eq!(
        manifest_of(store.root(), &index.to_string(), &read),
        index.to_string()
    );
    assert_eq!(manifest_of(store.root(), "nonsense", &read), "nonsense");
}

/// A pin that names the manifest itself writes no alias, and a pin that names an index in an
/// OCI layout directory is resolved the same way as one in a registry. Catches an alias for
/// a manifest pin (a file that would name the manifest by itself), and a layout pull that
/// cannot take an index.
#[test]
fn a_manifest_pin_writes_no_alias_and_a_layout_index_pin_writes_one() {
    let dir = Scratch::new("index-layout");
    let bundle = standard(&dir);
    let store = placer(&dir, no_registry());
    let objects = store.root().join("v1/objects");
    let layout = bundle.layout(&dir, "plain");
    store.ensure_layout(&layout, &bundle.digest()).unwrap();
    assert!(names(&objects.join("indexes")).is_empty());

    let other = Scratch::new("index-layout-index");
    let store = placer(&other, no_registry());
    let layout = bundle.layout(&other, "indexed");
    let (index, bytes) = index_of(&bundle.artifact.digest);
    fs::write(blob_path(&layout, &index), bytes).unwrap();
    write_index(&layout, &[&index]);
    store.ensure_layout(&layout, &index.to_string()).unwrap();
    let objects = store.root().join("v1/objects");
    assert_eq!(names(&objects.join("indexes")), [index.hex()]);
    let alias = fs::read_to_string(objects.join("indexes").join(index.hex())).unwrap();
    assert_eq!(alias, bundle.digest());
    let pinned = store.pin(&pin(&index), "lease-1").unwrap();
    assert_eq!(pinned.config(), &bundle.config);
}

/// A layer the registry serves with one byte changed makes the fetch fail, by the manifest
/// digest and by the index digest alike, and the store keeps no tree, no manifest, no config
/// and no alias for it. Catches a store that records a bundle (or the index that led to it)
/// whose bytes did not hash to what the manifest says.
#[test]
fn a_bundle_blob_that_fails_its_digest_leaves_nothing_in_the_store() {
    let dir = Scratch::new("index-flipped");
    let bundle = standard(&dir);
    let (_runtime, fixture, index) = served(&bundle);
    let layer = &bundle.artifact.layers[0];
    let mut flipped = layer.clone();
    flipped[0] ^= 1;
    fixture.publish_blob_as(&Digest::of(layer), &flipped);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    for pin in [index.to_string(), bundle.digest()] {
        let why = store.ensure_registry(&name, &pin).unwrap_err();
        assert!(matches!(why, PlaceError::Tree(_)), "{why}");
    }
    let objects = store.root().join("v1/objects");
    for dir in ["trees", "manifests", "configs", "indexes"] {
        assert!(names(&objects.join(dir)).is_empty(), "{dir}");
    }
}
