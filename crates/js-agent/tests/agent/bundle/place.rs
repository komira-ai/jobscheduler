//! The agent's store of bundles: a bundle is ensured from a registry or a layout directory
//! into the tree store, fetched in the background, pinned for one run and cloned where a tool
//! needs to write. Each test says the defect it catches.

use std::fs;
use std::sync::Arc;

use js_agent::bundle::{Fetch, PlaceError};
use js_bundle::{TreeName, media};
use js_oci::Digest;
use js_testkit::oci_registry::{Artifact, Fixture, REGISTRY, REPO};
use js_treestore::Owner;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::fixture::{Bundle, PanicSealer, Spec, no_registry, placer, placer_with};
use crate::support::Scratch;

/// A bundle of the standard spec in `dir`.
fn standard(dir: &Scratch) -> Bundle {
    let src = dir.path("src");
    fs::create_dir_all(&src).unwrap();
    Spec::standard().build(&src)
}

/// The files and directories of `dir`, by name.
fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A registry that serves `bundle`, and the runtime it runs on (kept alive by the caller).
fn served(bundle: &Bundle) -> (tokio::runtime::Runtime, Fixture) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = runtime.block_on(Fixture::start());
    fixture.publish(&bundle.artifact);
    (runtime, fixture)
}

/// A bundle in a layout directory is ensured: its manifest and config are kept beside one
/// tree for each layer, and the trees are pinned for a run and unpinned when the pin is
/// dropped (a run of the same name can pin again). It catches a store that records a
/// bundle whose trees are not there, and a pin that outlives its run.
#[test]
fn a_layout_bundle_is_placed_pinned_and_unpinned() {
    let dir = Scratch::new("place-layout");
    let bundle = standard(&dir);
    let layout = bundle.layout(&dir, "layout");
    let store = placer(&dir, no_registry());
    store.ensure_layout(&layout, &bundle.digest()).unwrap();
    let objects = store.root().join("v1/objects");
    let hex = bundle.digest().replace("sha256:", "");
    assert!(objects.join(format!("manifests/{hex}.json")).is_file());
    assert_eq!(names(&objects.join("configs")).len(), 1);
    for layer in &bundle.config.layers {
        let tree = objects.join("trees").join(layer.diff_id.hex());
        assert!(tree.is_dir(), "{}", layer.tree.as_str());
    }
    let pinned = store.pin(&bundle.pin(), "lease-1").unwrap();
    assert_eq!(pinned.config(), &bundle.config);
    assert_eq!(pinned.root(), store.root());
    assert!(format!("{store:?}").contains("Placer"));
    assert!(format!("{pinned:?}").contains("toolchain"));
    assert!(matches!(
        store.pin(&bundle.pin(), "lease-1"),
        Err(PlaceError::Tree(_))
    ));
    drop(pinned);
    store.pin(&bundle.pin(), "lease-1").unwrap();
}

/// A digest the layout does not hold, and a directory that is no layout, place nothing. It
/// catches a store that records a bundle it never read.
#[test]
fn a_digest_the_layout_lacks_places_nothing() {
    let dir = Scratch::new("place-lacks");
    let bundle = standard(&dir);
    let layout = bundle.layout(&dir, "layout");
    let store = placer(&dir, no_registry());
    let other = format!("sha256:{}", "d".repeat(64));
    let why = store.ensure_layout(&layout, &other).unwrap_err();
    assert!(matches!(why, PlaceError::Source(_)), "{why}");
    let nothing = dir.path("nothing");
    fs::create_dir(&nothing).unwrap();
    let why = store.ensure_layout(&nothing, &bundle.digest()).unwrap_err();
    assert!(matches!(why, PlaceError::Source(_)), "{why}");
    let objects = store.root().join("v1/objects");
    assert!(names(&objects.join("manifests")).is_empty());
    assert!(names(&objects.join("trees")).is_empty());
}

/// A layer whose bytes are not the bytes the manifest names is refused, and the store keeps
/// neither the manifest nor a tree. It catches a bundle trusted for its name and not its
/// bytes.
#[test]
fn an_altered_layer_is_refused_and_nothing_is_kept() {
    let dir = Scratch::new("place-altered");
    let bundle = standard(&dir);
    let layout = bundle.layout(&dir, "layout");
    let layer = &bundle.artifact.layers[0];
    let blob = layout.join("blobs/sha256").join(Digest::of(layer).hex());
    let mut bytes = fs::read(&blob).unwrap();
    bytes.push(0);
    fs::write(&blob, bytes).unwrap();
    let store = placer(&dir, no_registry());
    assert!(store.ensure_layout(&layout, &bundle.digest()).is_err());
    let objects = store.root().join("v1/objects");
    assert!(names(&objects.join("manifests")).is_empty());
}

/// One-entry bundle config for a plain-tar layer `tar`.
fn plain_artifact(tar: &[u8]) -> Artifact {
    let diff_id = format!("sha256:{}", hex::encode(Sha256::digest(tar)));
    let config = json!({
        "schema": 1, "os": "darwin", "architecture": "arm64", "os.version": "14.0",
        "name": "plain", "version": "1",
        "layers": [{
            "tree": "tool", "diff_id": diff_id, "uncompressed_bytes": tar.len() - 1024,
            "entries": 1, "license": "MIT", "source_url": "https://example.invalid/source",
        }],
        "env": [], "system_path": false, "entrypoints": {}, "writable_trees": [],
        "seatbelt": {"reads": [], "network_max": "off"},
        "built": {"deployment_target": "14.0", "tool": "tests 1"},
    });
    Artifact::build(
        media::MANIFEST,
        Some(media::ARTIFACT_TYPE),
        media::CONFIG,
        serde_json::to_vec(&config).unwrap(),
        media::LAYER_TAR,
        vec![tar.to_vec()],
    )
}

/// A tar stream of one regular file `name` with `body`, and its end blocks.
fn tar_of(name: &str, body: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut tar = header.to_vec();
    tar.extend_from_slice(body);
    tar.resize(512 + body.len().div_ceil(512) * 512, 0);
    tar.extend_from_slice(&[0u8; 1024]);
    tar
}

/// A layer with an entry that climbs out of its tree (`../escape`) is refused and nothing
/// is made beside the trees, while the same layer with an ordinary name is placed (so the
/// refusal is for the name). It catches an extraction that follows a path out of the store.
#[test]
fn a_layer_that_climbs_out_of_its_tree_is_refused() {
    let dir = Scratch::new("place-hostile");
    let store = placer(&dir, no_registry());
    let fine = plain_artifact(&tar_of("ok.txt", b"hi\n"));
    let layout = dir.path("fine");
    fs::create_dir(&layout).unwrap();
    js_testkit::oci_registry::write_layout(&layout, &fine);
    store
        .ensure_layout(&layout, &fine.digest.to_string())
        .expect("a layer with an ordinary name");
    let hostile = plain_artifact(&tar_of("../escape", b"hi\n"));
    let layout = dir.path("hostile");
    fs::create_dir(&layout).unwrap();
    js_testkit::oci_registry::write_layout(&layout, &hostile);
    let objects = store.root().join("v1/objects");
    let before = names(&objects.join("trees"));
    assert!(
        store
            .ensure_layout(&layout, &hostile.digest.to_string())
            .is_err()
    );
    assert_eq!(names(&objects.join("trees")), before);
    assert!(!objects.join("escape").exists());
    assert!(!store.root().join("escape").exists());
    let manifests = names(&objects.join("manifests"));
    assert_eq!(manifests, [format!("{}.json", fine.digest.hex())]);
}

/// A config that names an entrypoint outside its tree, and a manifest that has more layers
/// than its config describes, are refused as what they are. It catches a bundle whose
/// config was not checked before its layers were unpacked.
#[test]
fn a_config_that_breaks_the_format_is_refused() {
    let dir = Scratch::new("place-config");
    let store = placer(&dir, no_registry());
    let bundle = standard(&dir);
    let text = String::from_utf8(bundle.artifact.config.clone()).unwrap();
    let climbing = text.replace("${tree:tool}/bin/tool", "${tree:tool}/../../escape");
    assert_ne!(climbing, text);
    let artifact = Artifact::bundle_with_config(bundle.artifact.layers.clone(), &climbing);
    let layout = dir.path("climbing");
    fs::create_dir(&layout).unwrap();
    js_testkit::oci_registry::write_layout(&layout, &artifact);
    let why = store
        .ensure_layout(&layout, &artifact.digest.to_string())
        .unwrap_err();
    assert!(matches!(why, PlaceError::Config(_)), "{why}");
    let fewer = Artifact::bundle_with_config(bundle.artifact.layers[..1].to_vec(), &text);
    let layout = dir.path("fewer");
    fs::create_dir(&layout).unwrap();
    js_testkit::oci_registry::write_layout(&layout, &fewer);
    let why = store
        .ensure_layout(&layout, &fewer.digest.to_string())
        .unwrap_err();
    assert!(matches!(why, PlaceError::Mismatch(_)), "{why}");
    let objects = store.root().join("v1/objects");
    assert!(names(&objects.join("manifests")).is_empty());
}

/// A bundle in a registry is ensured by its manifest digest whatever tag the name carries
/// (or none), and a digest the registry does not hold is refused. It catches a name taken
/// for the digest, and a bundle pulled by tag.
#[test]
fn a_registry_bundle_is_ensured_by_digest_and_an_unknown_one_is_refused() {
    let dir = Scratch::new("place-registry");
    let bundle = standard(&dir);
    let (_runtime, fixture) = served(&bundle);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    let unknown = Digest::of(b"nothing").to_string();
    let why = store.ensure_registry(&name, &unknown).unwrap_err();
    assert!(matches!(why, PlaceError::Source(_)), "{why}");
    store
        .ensure_registry(&format!("{name}:latest"), &bundle.digest())
        .unwrap();
    store.ensure_registry(&name, &bundle.digest()).unwrap();
    store.pin(&bundle.pin(), "lease-1").unwrap();
    let bad = store.ensure_registry("registry.test", &bundle.digest());
    assert!(matches!(bad, Err(PlaceError::Source(_))));
    let bad = store.ensure_registry(&name, "sha256:short");
    assert!(matches!(bad, Err(PlaceError::Source(_))));
}

/// A fetch begins at once and the bundle is held when it ends; one that fails reports its
/// failure to the next call, once, and the call after begins again; a digest that names an
/// index is reported as one. It catches a fetch that blocks the lifecycle's worker, a
/// failure that is lost, and a failure that is reported for ever.
#[test]
fn a_fetch_runs_in_the_background_and_reports_its_failure_once() {
    let dir = Scratch::new("place-fetch");
    let bundle = standard(&dir);
    let (_runtime, fixture) = served(&bundle);
    let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
    let index_digest = Digest::of(index);
    fixture.publish_manifest_as(&index_digest, index);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    assert_eq!(store.fetch(&name, &bundle.digest()), Fetch::Started);
    store.settle();
    store.pin(&bundle.pin(), "lease-1").expect("held");
    let missing = Digest::of(b"nothing").to_string();
    assert_eq!(store.fetch(&name, &missing), Fetch::Started);
    store.settle();
    let Fetch::Failed(why) = store.fetch(&name, &missing) else {
        panic!("the failure is reported");
    };
    assert!(!why.is_empty());
    assert_eq!(store.fetch(&name, &missing), Fetch::Started);
    store.settle();
    assert_eq!(
        store.fetch(&name, &index_digest.to_string()),
        Fetch::Started
    );
    store.settle();
    assert_eq!(store.fetch(&name, &index_digest.to_string()), Fetch::Index);
}

/// A run's clones are of the config's writable trees only, made where it says, and writing
/// in one leaves the store's tree as it was; a clone that cannot be made is an error. It
/// catches a tool that could write into the store, and a clone of a tree nobody asked for.
#[test]
fn only_the_writable_trees_are_cloned_and_the_store_stays_as_it_was() {
    let dir = Scratch::new("place-clone");
    let bundle = standard(&dir);
    let store = placer(&dir, no_registry());
    store
        .ensure_layout(&bundle.layout(&dir, "layout"), &bundle.digest())
        .unwrap();
    let pinned = store.pin(&bundle.pin(), "lease-1").unwrap();
    let under = dir.path("clones");
    fs::create_dir(&under).unwrap();
    let clones = pinned.clone_writable(&under, Owner::caller()).unwrap();
    let work = TreeName::parse("work").unwrap();
    assert_eq!(clones.len(), 1);
    assert_eq!(clones[&work], under.join("work"));
    let script = fs::read_to_string(under.join("work/run.sh")).unwrap();
    assert_eq!(script, "#!/bin/sh\necho work\n");
    assert!(!under.join("tool").exists());
    fs::write(under.join("work/new"), "x").unwrap();
    let layer = bundle
        .config
        .layers
        .iter()
        .find(|layer| layer.tree == work)
        .unwrap();
    let tree = store
        .root()
        .join("v1/objects/trees")
        .join(layer.diff_id.hex());
    assert_eq!(names(&tree), ["run.sh"]);
    let again = pinned.clone_writable(&under, Owner::caller());
    assert!(matches!(again, Err(PlaceError::Seal(_))));
}

/// A pin that is no bundle reference, a bundle the store has no record of and a run id the
/// tree store does not take are each refused, and refused for what they are. It catches a
/// pin that reaches the store's files with text nobody checked.
#[test]
fn a_pin_that_cannot_be_served_is_refused_for_what_it_is() {
    let dir = Scratch::new("place-pin");
    let bundle = standard(&dir);
    let store = placer(&dir, no_registry());
    assert!(matches!(
        store.pin("not a pin", "lease-1"),
        Err(PlaceError::Held(_))
    ));
    assert!(matches!(
        store.pin(&bundle.pin(), "lease-1"),
        Err(PlaceError::Held(_))
    ));
    store
        .ensure_layout(&bundle.layout(&dir, "layout"), &bundle.digest())
        .unwrap();
    assert!(matches!(
        store.pin(&bundle.pin(), "Not A Run"),
        Err(PlaceError::Tree(_))
    ));
}

/// A second call for a digest whose fetch is running joins it: it answers `Started` and
/// begins nothing, and when the fetch ends badly (here the registry goes away while it is
/// waiting for an answer) the failure is reported once. It catches two downloads of one
/// bundle, and a fetch that is never reported when it fails.
#[test]
fn a_second_fetch_of_a_digest_joins_the_one_running() {
    let dir = Scratch::new("place-join");
    let bundle = standard(&dir);
    let (runtime, fixture) = served(&bundle);
    fixture.behave(|behaviour| behaviour.stall_head = true);
    let store = placer(&dir, fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    assert_eq!(store.fetch(&name, &bundle.digest()), Fetch::Started);
    assert_eq!(store.fetch(&name, &bundle.digest()), Fetch::Started);
    drop(runtime);
    store.settle();
    let reported = store.fetch(&name, &bundle.digest());
    assert!(matches!(reported, Fetch::Failed(_)), "{reported:?}");
    store.settle();
}

/// A fetch whose work panics (here a sealer that does) ends as a failure the next call
/// reports, and `settle` returns. It catches a flight that stays running for ever, which
/// would answer `bundle-pending` to every start of the bundle until the agent restarts.
#[test]
fn a_fetch_that_panics_is_reported_as_a_failure() {
    let dir = Scratch::new("place-panic");
    let bundle = standard(&dir);
    let (_runtime, fixture) = served(&bundle);
    let store = placer_with(&dir, Arc::new(PanicSealer), fixture.client());
    let name = format!("{}/{REPO}", fixture.at(REGISTRY));
    assert_eq!(store.fetch(&name, &bundle.digest()), Fetch::Started);
    store.settle();
    assert_eq!(
        store.fetch(&name, &bundle.digest()),
        Fetch::Failed("the fetch stopped unexpectedly".to_owned())
    );
    store.settle();
}
