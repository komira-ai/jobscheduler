//! The js-oci client as the image source of js-contain's store.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use js_agent::image::{CONFIG_LIMIT_BYTES, RegistryImages};
use js_contain::oci::image::{FetchError, ImageSource, PinnedImage};
use js_oci::{Digest, media};
use js_testkit::dir::TestDir;
use js_testkit::oci_registry::{Artifact, Fixture, REGISTRY};

use crate::config::config_json;

/// The layers a test publishes: distinct, and not tars, since the adapter does not look
/// inside them (unpacking is the image store's).
fn layers(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|n| format!("layer {n} ").repeat(50 + n).into_bytes())
        .collect()
}

/// A gzip-layered image of `layers` for amd64 with an `Entrypoint`.
fn image(layers: Vec<Vec<u8>>) -> Artifact {
    let config = config_json(r#"{"Entrypoint":["/usr/bin/kbf-task"]}"#, layers.len());
    Artifact::image(layers, &config)
}

/// An image whose layers all have the media type `layer_type`.
fn image_of_type(layer_type: &str, layers: Vec<Vec<u8>>) -> Artifact {
    let config = config_json("{}", layers.len());
    Artifact::build(
        media::OCI_MANIFEST,
        None,
        media::OCI_CONFIG,
        config.into_bytes(),
        layer_type,
        layers,
    )
}

/// The image `artifact` names, in a repository the test client may pull from.
fn pin(fx: &Fixture, artifact: &Artifact) -> PinnedImage {
    PinnedImage {
        reference: format!("{}/team/app@{}", fx.at(REGISTRY), artifact.digest),
        digest: artifact.digest.to_string(),
    }
}

/// The source over the fixture's client, for the Rust architecture `arch`.
fn source(fx: &Fixture, arch: &str) -> RegistryImages {
    RegistryImages::new(fx.client(), arch).expect("a platform")
}

/// The names in `dir`, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn failed_text(result: Result<Vec<String>, FetchError>) -> String {
    match result.expect_err("refused") {
        FetchError::Failed(why) => why,
        FetchError::Index => panic!("named an index"),
    }
}

/// The layers come out base layer first, as `<n>.tar.gz` for gzip layers, and each file
/// holds exactly the bytes that were published (the ones the client verified against the
/// manifest); nothing else is in the directory. It catches layers in registry-arrival or
/// hash order, a wrong extension for the unpack mode, and a file that is not the blob.
#[tokio::test]
async fn layers_are_written_base_first_as_the_verified_bytes() {
    let fx = Fixture::start().await;
    let published = layers(3);
    let artifact = image(published.clone());
    fx.publish(&artifact);
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-order");
    let names = source(&fx, "x86_64")
        .fetch(&pin(&fx, &artifact), blobs.path())
        .await
        .expect("fetches");
    assert_eq!(names, ["0.tar.gz", "1.tar.gz", "2.tar.gz"]);
    assert_eq!(listing(blobs.path()), names);
    for (name, bytes) in names.iter().zip(&published) {
        assert_eq!(
            &std::fs::read(blobs.join(name)).expect("layer"),
            bytes,
            "{name}"
        );
    }
    // Private until the image store has used them: owner read and write only.
    for name in &names {
        let mode = std::fs::metadata(blobs.join(name))
            .expect("layer")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

/// Uncompressed layers are `<n>.tar`; Docker's gzip media type is `.tar.gz` like OCI's.
#[tokio::test]
async fn the_extension_follows_the_media_type_of_the_layer() {
    let fx = Fixture::start().await;
    for (layer_type, expected) in [
        (media::LAYER_TAR, ["0.tar", "1.tar"]),
        (media::LAYER_TAR_GZIP, ["0.tar.gz", "1.tar.gz"]),
        (media::DOCKER_LAYER_GZIP, ["0.tar.gz", "1.tar.gz"]),
    ] {
        let artifact = image_of_type(layer_type, layers(2));
        fx.publish(&artifact);
        let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-ext");
        let names = source(&fx, "x86_64")
            .fetch(&pin(&fx, &artifact), blobs.path())
            .await
            .expect("fetches");
        assert_eq!(names, expected, "{layer_type}");
    }
}

/// The adapter is an `ImageSource` as the image store holds it, and its `Debug` (which the
/// trait requires) names the platform and prints no client state.
#[tokio::test]
async fn it_is_an_image_source_that_names_its_platform() {
    let fx = Fixture::start().await;
    let images = source(&fx, "aarch64");
    let as_source: &dyn ImageSource = &images;
    let shown = format!("{as_source:?}");
    assert!(shown.contains("RegistryImages"), "{shown}");
    assert!(shown.contains("arm64"), "{shown}");
}

/// The platform is the Rust architecture's: an `x86_64` source takes an amd64 image and
/// refuses an arm64 one, an `aarch64` source the reverse, and an architecture with no
/// image name cannot make a source. It catches a source pinned to one platform and one
/// that runs the wrong machine's binaries.
#[tokio::test]
async fn an_image_for_another_architecture_is_refused() {
    let fx = Fixture::start().await;
    let amd64 = image(layers(1));
    let arm64 = Artifact::image(layers(1), &config_json("{}", 1).replace("amd64", "arm64"));
    fx.publish(&amd64);
    fx.publish(&arm64);
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-arch");
    for (arch, ok, other) in [("x86_64", &amd64, &arm64), ("aarch64", &arm64, &amd64)] {
        let images = source(&fx, arch);
        images
            .fetch(&pin(&fx, ok), blobs.path())
            .await
            .expect("this architecture's image");
        for name in listing(blobs.path()) {
            std::fs::remove_file(blobs.join(name)).expect("remove");
        }
        let why = failed_text(images.fetch(&pin(&fx, other), blobs.path()).await);
        assert!(why.contains("the artifact is for linux/"), "{arch}: {why}");
    }
    let unknown = RegistryImages::new(fx.client(), "riscv64").expect_err("no platform");
    assert_eq!(
        unknown.to_string(),
        "the architecture riscv64 has no image platform name"
    );
}

/// A pin that names an image index is `FetchError::Index`, which the image store reports
/// as an index and not as a failed pull.
#[tokio::test]
async fn an_index_digest_is_reported_as_an_index() {
    let fx = Fixture::start().await;
    let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
    let digest = Digest::of(index);
    fx.publish_manifest_as(&digest, index);
    let pinned = PinnedImage {
        reference: format!("{}/team/app@{digest}", fx.at(REGISTRY)),
        digest: digest.to_string(),
    };
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-index");
    let refused = source(&fx, "x86_64")
        .fetch(&pinned, blobs.path())
        .await
        .expect_err("an index");
    assert_eq!(refused, FetchError::Index);
}

/// A layer that fails the client's checks, after some of its bytes were written, leaves no
/// file: not the partial one and not the layers that came before it. The failures are a
/// blob that is the wrong bytes, and one that is shorter than the manifest says. It
/// catches a half layer that the unpack would later take for a whole one.
#[tokio::test]
async fn a_layer_that_fails_leaves_no_file_behind() {
    let published = layers(3);
    let wrong = published[1].iter().map(|b| b ^ 1).collect::<Vec<u8>>();
    let short = published[1][..published[1].len() / 2].to_vec();
    for (case, served, text) in [
        ("wrong bytes", wrong, "hashes to"),
        ("short", short, "the manifest says"),
    ] {
        let fx = Fixture::start().await;
        let artifact = image(published.clone());
        fx.publish(&artifact);
        fx.publish_blob_as(&Digest::of(&published[1]), &served);
        let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-partial");
        let why = failed_text(
            source(&fx, "x86_64")
                .fetch(&pin(&fx, &artifact), blobs.path())
                .await,
        );
        assert!(why.contains(text), "{case}: {why}");
        assert_eq!(listing(blobs.path()), Vec::<String>::new(), "{case}");
    }
}

/// A file that is already in the directory is not written over and not removed: the
/// fetch fails, and the file is as it was.
#[tokio::test]
async fn a_file_already_there_is_neither_overwritten_nor_removed() {
    let fx = Fixture::start().await;
    let artifact = image(layers(2));
    fx.publish(&artifact);
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-exists");
    std::fs::write(blobs.join("0.tar.gz"), b"mine").expect("write");
    let why = failed_text(
        source(&fx, "x86_64")
            .fetch(&pin(&fx, &artifact), blobs.path())
            .await,
    );
    assert!(why.contains("exists"), "{why}");
    assert_eq!(
        std::fs::read(blobs.join("0.tar.gz")).expect("read"),
        b"mine"
    );
    assert_eq!(listing(blobs.path()), ["0.tar.gz"]);
}

/// A config that lists another number of layers than the manifest is refused before any
/// layer is fetched.
#[tokio::test]
async fn a_config_with_another_layer_count_is_refused() {
    let fx = Fixture::start().await;
    let artifact = Artifact::image(layers(2), &config_json("{}", 1));
    fx.publish(&artifact);
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-count");
    let why = failed_text(
        source(&fx, "x86_64")
            .fetch(&pin(&fx, &artifact), blobs.path())
            .await,
    );
    assert_eq!(why, "the image config lists 1 layers, the manifest 2");
    assert_eq!(listing(blobs.path()), Vec::<String>::new());
    assert!(
        fx.requests()
            .iter()
            .all(|request| !request.contains("/blobs/")
                || request.contains(&Digest::of(artifact.config.as_slice()).to_string())),
        "a layer was requested: {:?}",
        fx.requests()
    );
}

/// A config that is not an image config (no `rootfs`) is refused, and so is one declared
/// larger than the limit, before it is downloaded.
#[tokio::test]
async fn a_config_that_is_not_one_or_is_too_large_is_refused() {
    let fx = Fixture::start().await;
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-config");
    let not_a_config = Artifact::image(layers(1), r#"{"architecture":"amd64","os":"linux"}"#);
    fx.publish(&not_a_config);
    let why = failed_text(
        source(&fx, "x86_64")
            .fetch(&pin(&fx, &not_a_config), blobs.path())
            .await,
    );
    assert!(why.starts_with("the image config is not a config"), "{why}");

    let padding = "x".repeat(usize::try_from(CONFIG_LIMIT_BYTES).expect("fits"));
    let large = Artifact::image(
        layers(1),
        &config_json(&format!(r#"{{"Labels":{{"pad":"{padding}"}}}}"#), 1),
    );
    fx.publish(&large);
    let why = failed_text(
        source(&fx, "x86_64")
            .fetch(&pin(&fx, &large), blobs.path())
            .await,
    );
    assert!(
        why.ends_with(&format!("is larger than {CONFIG_LIMIT_BYTES} bytes")),
        "{why}"
    );
    assert_eq!(listing(blobs.path()), Vec::<String>::new());
}

/// A pin whose reference is not `<repository>@<digest>` of its own digest, a digest or a
/// repository that is not well formed, a registry off the allow-list and a manifest the
/// registry does not have are each a failed fetch that says why, and write nothing.
#[tokio::test]
async fn a_pin_that_cannot_be_pulled_is_a_failed_fetch_with_the_reason() {
    let fx = Fixture::start().await;
    let artifact = image(layers(1));
    fx.publish(&artifact);
    let good = pin(&fx, &artifact);
    let missing = Digest::of(b"no such manifest").to_string();
    let elsewhere = RegistryImages::new(
        fx.client_for(&[&format!("{}/other", fx.at(REGISTRY))], true),
        "x86_64",
    )
    .expect("a platform");
    let cases = [
        (
            "no digest",
            PinnedImage {
                reference: format!("{}/team/app", fx.at(REGISTRY)),
                ..good.clone()
            },
            "is not <repository>@",
        ),
        (
            "another digest",
            PinnedImage {
                digest: missing.clone(),
                ..good.clone()
            },
            "is not <repository>@",
        ),
        (
            "bad digest",
            PinnedImage {
                reference: format!("{}/team/app@sha256:zz", fx.at(REGISTRY)),
                digest: "sha256:zz".to_owned(),
            },
            "is not sha256:",
        ),
        (
            "bad repository",
            PinnedImage {
                reference: format!("{}/Team/App@{}", fx.at(REGISTRY), good.digest),
                ..good.clone()
            },
            "is not a well formed registry reference",
        ),
        (
            "no such manifest",
            PinnedImage {
                reference: format!("{}/team/app@{missing}", fx.at(REGISTRY)),
                digest: missing,
            },
            "the registry answered 404",
        ),
    ];
    let blobs = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-pin");
    let ours = source(&fx, "x86_64");
    for (case, pinned, text) in cases {
        let why = failed_text(ours.fetch(&pinned, blobs.path()).await);
        assert!(why.contains(text), "{case}: {why}");
    }
    let why = failed_text(elsewhere.fetch(&good, blobs.path()).await);
    assert!(why.contains("is not on the registry allow-list"), "{why}");
    assert_eq!(listing(blobs.path()), Vec::<String>::new());
}
