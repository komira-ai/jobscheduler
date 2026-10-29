//! An OCI image-layout directory as a source of bundles: it serves what a registry serves,
//! byte for byte, refuses what a registry refuses, and refuses what only a directory can
//! hold wrong (links, an unknown version, missing and altered blobs).

mod support;

use std::path::Path;

use async_trait::async_trait;
use js_oci::{BlobSink, BundleSource, Digest, Error, Kind, Layout, LayoutFault, Repository};
use js_testkit::dir::TestDir;
use support::{Artifact, Fixture, REPO, blob_path, write_index, write_layout};

fn layers() -> Vec<Vec<u8>> {
    // The second is longer than one read of a blob, so the loop runs more than once.
    vec![b"first layer".to_vec(), vec![7u8; 200_000]]
}

/// A sink that records what it is told and can be made to fail.
#[derive(Default)]
struct Recording {
    bytes: Vec<u8>,
    finished: bool,
    fail_write: bool,
    fail_finish: bool,
}

#[async_trait]
impl BlobSink for Recording {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if self.fail_write {
            return Err(std::io::Error::other("disk full"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        if self.fail_finish {
            return Err(std::io::Error::other("flush failed"));
        }
        self.finished = true;
        Ok(())
    }
}

/// One artifact, served by the fixture registry and also written as a layout directory.
struct Both {
    fx: Fixture,
    dir: TestDir,
    artifact: Artifact,
}

impl Both {
    async fn new(artifact: Artifact) -> Self {
        let fx = Fixture::start().await;
        fx.publish(&artifact);
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-oci-layout");
        write_layout(&dir, &artifact);
        Self { fx, dir, artifact }
    }

    async fn bundle() -> Self {
        Self::new(Artifact::bundle(layers())).await
    }

    /// Replaces the bytes served under `digest` in both places.
    fn tamper(&self, digest: &Digest, bytes: &[u8]) {
        self.fx.publish_blob_as(digest, bytes);
        std::fs::write(blob_path(&self.dir, digest), bytes).expect("tamper");
    }

    /// Replaces the manifest served under `digest` in both places.
    fn tamper_manifest(&self, digest: &Digest, bytes: &[u8]) {
        self.fx.publish_manifest_as(digest, bytes);
        std::fs::write(blob_path(&self.dir, digest), bytes).expect("tamper");
    }

    /// Serves `bytes` as a manifest named by their digest, listed by the layout.
    fn put_manifest(&self, bytes: &[u8]) -> Digest {
        let digest = Digest::of(bytes);
        self.fx.publish_manifest_as(&digest, bytes);
        std::fs::write(blob_path(&self.dir, &digest), bytes).expect("manifest");
        write_index(&self.dir, &[&digest]);
        digest
    }
}

/// Runs `check` against the registry and the layout, naming the source in a failure.
macro_rules! each_source {
    ($both:expr, |$name:ident, $source:ident| $body:block) => {{
        let client = $both.fx.client();
        let repo: Repository<'_> = client
            .repository($both.fx.location(REPO))
            .expect("repository");
        let layout = Layout::open($both.dir.path()).await.expect("layout");
        let sources: [(&str, &dyn BundleSource); 2] = [
            ("registry", &repo as &dyn BundleSource),
            ("layout", &layout as &dyn BundleSource),
        ];
        for ($name, $source) in sources {
            $body
        }
    }};
}

async fn manifest_error(source: &dyn BundleSource, digest: &Digest, kind: &Kind) -> Error {
    source
        .fetch_manifest(digest, kind)
        .await
        .expect_err("refused")
}

/// Fetches layer `index` of `both`'s artifact from `source` into a recording sink.
async fn layer(
    source: &dyn BundleSource,
    both: &Both,
    index: usize,
) -> (Result<(), Error>, Recording) {
    let kind = Kind::bundle();
    let manifest = source
        .fetch_manifest(&both.artifact.digest, &kind)
        .await
        .expect("manifest");
    let mut sink = Recording::default();
    let result = source
        .fetch_blob(&manifest.layers()[index], &mut sink)
        .await;
    (result, sink)
}

/// Proves the two sources are interchangeable: the same artifact read from the registry
/// and from a layout directory gives the same manifest (bytes, digest, type, config and
/// layers), the same config bytes and the same layer bytes, and every sink is finished.
#[tokio::test]
async fn the_same_artifact_reads_identically_from_a_registry_and_a_layout() {
    let both = Both::bundle().await;
    let kind = Kind::bundle();
    let mut seen = Vec::new();
    each_source!(both, |name, source| {
        let manifest = source
            .fetch_manifest(&both.artifact.digest, &kind)
            .await
            .expect(name);
        let config = source.fetch_config(&manifest, &kind).await.expect(name);
        let mut blobs = Vec::new();
        for descriptor in manifest.layers() {
            let mut sink = Recording::default();
            source.fetch_blob(descriptor, &mut sink).await.expect(name);
            assert!(sink.finished, "{name}");
            blobs.push(sink.bytes);
        }
        seen.push((
            manifest.digest().clone(),
            manifest.bytes().to_vec(),
            manifest.artifact_type().map(str::to_owned),
            manifest.config().clone(),
            manifest.layers().to_vec(),
            config,
            blobs,
        ));
    });
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
    let (digest, bytes, artifact_type, _, descriptors, config, blobs) = &seen[1];
    assert_eq!(*digest, both.artifact.digest);
    assert_eq!(*bytes, both.artifact.manifest);
    assert_eq!(
        artifact_type.as_deref(),
        Some("application/vnd.komira.js.bundle.v1")
    );
    assert_eq!(descriptors.len(), 2);
    assert_eq!(*config, both.artifact.config);
    assert_eq!(*blobs, both.artifact.layers);
}

/// Proves a manifest is believed only under its own digest, by either source.
#[tokio::test]
async fn a_manifest_that_does_not_hash_to_its_pin_is_refused_by_both() {
    let both = Both::bundle().await;
    let other = Artifact::bundle(vec![b"other".to_vec()]);
    both.tamper_manifest(&both.artifact.digest, &other.manifest);
    each_source!(both, |name, source| {
        let err = manifest_error(source, &both.artifact.digest, &Kind::bundle()).await;
        assert!(matches!(err, Error::ManifestDigest { .. }), "{name}: {err}");
    });
}

/// Proves an index is refused as an index by either source, though its bytes hash to
/// the pin.
#[tokio::test]
async fn an_image_index_is_refused_by_both() {
    let both = Both::bundle().await;
    let index = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
    let digest = both.put_manifest(index);
    each_source!(both, |name, source| {
        let err = manifest_error(source, &digest, &Kind::bundle()).await;
        assert!(matches!(err, Error::Index(_)), "{name}: {err}");
    });
}

/// Proves the artifact type is checked by either source: an image manifest, which has
/// none, is not a bundle.
#[tokio::test]
async fn a_manifest_of_another_artifact_type_is_refused_by_both() {
    let both = Both::new(Artifact::image(layers(), "{}")).await;
    each_source!(both, |name, source| {
        let err = manifest_error(source, &both.artifact.digest, &Kind::bundle()).await;
        assert!(
            matches!(err, Error::ArtifactType { have: None, .. }),
            "{name}: {err}"
        );
    });
}

/// Proves the size limits of the kind hold for either source: a manifest over its limit,
/// a config declared over its limit, and a manifest with more layers than allowed.
#[tokio::test]
async fn manifests_over_the_kind_limits_are_refused_by_both() {
    let both = Both::bundle().await;
    let big_config = format!(
        r#"{{"os":"darwin","architecture":"arm64","pad":"{}"}}"#,
        "x".repeat(2000)
    );
    let big = Both::new(Artifact::bundle_with_config(layers(), &big_config)).await;
    let mut small_manifest = Kind::bundle();
    small_manifest.limits.manifest = 10;
    let mut small_config = Kind::bundle();
    small_config.limits.config = 1000;
    let mut few_layers = Kind::bundle();
    few_layers.limits.layers = 1;
    each_source!(both, |name, source| {
        let err = manifest_error(source, &both.artifact.digest, &small_manifest).await;
        assert!(
            matches!(err, Error::TooLarge { limit: 10, .. }),
            "{name}: {err}"
        );
        let err = manifest_error(source, &both.artifact.digest, &few_layers).await;
        assert!(
            matches!(err, Error::TooLarge { limit: 1, .. }),
            "{name}: {err}"
        );
    });
    each_source!(big, |name, source| {
        let err = manifest_error(source, &big.artifact.digest, &small_config).await;
        assert!(
            matches!(err, Error::TooLarge { limit: 1000, .. }),
            "{name}: {err}"
        );
    });
}

/// Proves a config for another platform is refused by either source.
#[tokio::test]
async fn a_config_for_another_platform_is_refused_by_both() {
    let config = r#"{"os":"linux","architecture":"arm64"}"#;
    let both = Both::new(Artifact::bundle_with_config(layers(), config)).await;
    let kind = Kind::bundle();
    each_source!(both, |name, source| {
        let manifest = source
            .fetch_manifest(&both.artifact.digest, &kind)
            .await
            .expect(name);
        let err = source.fetch_config(&manifest, &kind).await.expect_err(name);
        assert!(matches!(err, Error::Platform { .. }), "{name}: {err}");
    });
}

/// Proves a config that is longer or shorter than the manifest says, or hashes to another
/// digest, is refused by either source.
#[tokio::test]
async fn a_config_with_the_wrong_length_or_digest_is_refused_by_both() {
    let kind = Kind::bundle();
    let config = Artifact::bundle(layers()).config;
    let digest = Digest::of(&config);
    let variants = [
        ("same length, other bytes", b"z".repeat(config.len())),
        ("long", b"x".repeat(config.len() + 1)),
        ("short", config[..config.len() - 1].to_vec()),
    ];
    for (what, bytes) in variants {
        let both = Both::bundle().await;
        both.tamper(&digest, &bytes);
        each_source!(both, |name, source| {
            let manifest = source
                .fetch_manifest(&both.artifact.digest, &kind)
                .await
                .expect(name);
            let err = source.fetch_config(&manifest, &kind).await.expect_err(name);
            match what {
                "same length, other bytes" => {
                    assert!(matches!(err, Error::BlobDigest { .. }), "{name}: {err}");
                }
                _ => assert!(matches!(err, Error::BlobSize { .. }), "{name}: {err}"),
            }
        });
    }
}

/// Proves a layer whose bytes are not its name is refused by either source, with the
/// error a registry gives, and that the sink is never told the blob is whole: same length
/// and other bytes (digest), shorter and longer (size). A sink gets bytes as they are read
/// and holds them as unverified until `finish`; a longer blob is cut off at the declared
/// length, so the sink never receives more than the manifest declared.
#[tokio::test]
async fn a_layer_that_is_not_its_name_is_refused_by_both_and_never_finished() {
    let original = layers()[0].clone();
    let digest = Digest::of(&original);
    let right = original.len();
    let variants = [
        ("same length, other bytes", b"z".repeat(right)),
        ("short", original[..right - 1].to_vec()),
        ("long", [original.clone(), b"!".to_vec()].concat()),
    ];
    for (what, bytes) in variants {
        let both = Both::bundle().await;
        both.tamper(&digest, &bytes);
        each_source!(both, |name, source| {
            let (result, sink) = layer(source, &both, 0).await;
            let err = result.expect_err(name);
            match what {
                "same length, other bytes" => {
                    assert!(matches!(err, Error::BlobDigest { .. }), "{name}: {err}");
                }
                "short" => assert!(
                    matches!(err, Error::BlobSize { declared, got, .. } if got + 1 == declared),
                    "{name}: {err}"
                ),
                _ => assert!(
                    matches!(err, Error::BlobSize { declared, got, .. } if got == declared + 1),
                    "{name}: {err}"
                ),
            }
            assert!(!sink.finished, "{name} {what}");
            assert!(sink.bytes.len() <= right, "{name} {what}");
        });
    }
}

/// Proves a sink that fails is reported as the sink's failure by the layout, on a write
/// and on the final flush.
#[tokio::test]
async fn a_failing_sink_is_reported_by_the_layout() {
    let both = Both::bundle().await;
    let layout = Layout::open(both.dir.path()).await.expect("layout");
    let manifest = layout
        .fetch_manifest(&both.artifact.digest, &Kind::bundle())
        .await
        .expect("manifest");
    for (fail_write, fail_finish) in [(true, false), (false, true)] {
        let mut sink = Recording {
            fail_write,
            fail_finish,
            ..Recording::default()
        };
        let err = layout
            .fetch_blob(&manifest.layers()[0], &mut sink)
            .await
            .expect_err("sink fails");
        assert!(matches!(err, Error::Sink(_)), "{err}");
        assert!(!sink.finished);
    }
}

/// Makes a layout directory of the bundle and returns it; the registry half is unused.
fn layout_of(artifact: Artifact) -> (TestDir, Artifact) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-oci-layout-only");
    write_layout(&dir, &artifact);
    (dir, artifact)
}

fn fault(err: Error) -> LayoutFault {
    match err {
        Error::Layout(fault) => fault,
        other => panic!("not a layout fault: {other}"),
    }
}

async fn open_fault(root: &Path) -> LayoutFault {
    fault(Layout::open(root).await.expect_err("refused"))
}

fn outside(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Proves a layout that states a version other than 1.0.0 is refused, however the marker
/// is wrong: another version, no version, not JSON, or absent.
#[tokio::test]
async fn a_layout_of_an_unknown_version_is_refused() {
    let (dir, _) = layout_of(Artifact::bundle(layers()));
    Layout::open(dir.path()).await.expect("1.0.0 is read");
    let marker = dir.join("oci-layout");
    std::fs::write(&marker, r#"{"imageLayoutVersion":"1.1.0"}"#).expect("write");
    let version = open_fault(&dir).await;
    assert!(
        matches!(&version, LayoutFault::Version(v) if v.contains("1.1.0")),
        "{version}"
    );
    assert!(version.to_string().contains("only version 1.0.0"));
    for body in ["{}", "not json", ""] {
        std::fs::write(&marker, body).expect("write");
        let malformed = open_fault(&dir).await;
        assert!(
            matches!(
                &malformed,
                LayoutFault::Malformed {
                    file: "oci-layout",
                    ..
                }
            ),
            "{body:?}: {malformed}"
        );
        assert!(malformed.to_string().contains("oci-layout"));
    }
    std::fs::remove_file(&marker).expect("remove");
    let absent = open_fault(&dir).await;
    assert!(
        matches!(&absent, LayoutFault::Unreadable { path, source }
            if path == "oci-layout" && source.kind() == std::io::ErrorKind::NotFound),
        "{absent}"
    );
    assert!(absent.to_string().contains("cannot be read"));
}

/// Proves a missing blob is refused with its path: a layer, the config and the manifest.
#[tokio::test]
async fn a_missing_blob_is_refused() {
    let kind = Kind::bundle();
    let (dir, artifact) = layout_of(Artifact::bundle(layers()));
    let layout = Layout::open(dir.path()).await.expect("layout");
    let manifest = layout
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    let layer = &manifest.layers()[0];
    std::fs::remove_file(blob_path(&dir, layer.digest())).expect("remove layer");
    let err = fault(
        layout
            .fetch_blob(layer, &mut Recording::default())
            .await
            .expect_err("missing layer"),
    );
    assert!(
        matches!(&err, LayoutFault::Unreadable { path, source }
            if *path == format!("blobs/sha256/{}", layer.digest().hex())
                && source.kind() == std::io::ErrorKind::NotFound),
        "{err}"
    );
    std::fs::remove_file(blob_path(&dir, &Digest::of(&artifact.config))).expect("remove config");
    let err = fault(
        layout
            .fetch_config(&manifest, &kind)
            .await
            .expect_err("config"),
    );
    assert!(matches!(err, LayoutFault::Unreadable { .. }), "{err}");
    std::fs::remove_file(blob_path(&dir, &artifact.digest)).expect("remove manifest");
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(matches!(err, LayoutFault::Unreadable { .. }), "{err}");
}

/// Proves a blob that is a symlink is not followed, though the file it points at holds
/// exactly the right bytes: a layer, and the manifest.
#[tokio::test]
async fn a_blob_that_is_a_symlink_is_refused() {
    let kind = Kind::bundle();
    let (dir, artifact) = layout_of(Artifact::bundle(layers()));
    let elsewhere = outside("js-oci-elsewhere");
    let layout = Layout::open(dir.path()).await.expect("layout");
    let manifest = layout
        .fetch_manifest(&artifact.digest, &kind)
        .await
        .expect("manifest");
    for (blob, bytes) in [
        (manifest.layers()[0].digest().clone(), layers()[0].clone()),
        (artifact.digest.clone(), artifact.manifest.clone()),
    ] {
        let target = elsewhere.join(blob.hex());
        std::fs::write(&target, bytes).expect("target");
        let path = blob_path(&dir, &blob);
        std::fs::remove_file(&path).expect("remove");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
    }
    let err = fault(
        layout
            .fetch_blob(&manifest.layers()[0], &mut Recording::default())
            .await
            .expect_err("linked layer"),
    );
    assert!(
        matches!(&err, LayoutFault::Symlink(p)
            if *p == format!("blobs/sha256/{}", manifest.layers()[0].digest().hex())),
        "{err}"
    );
    assert!(err.to_string().contains("without following"));
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(matches!(err, LayoutFault::Symlink(_)), "{err}");
}

/// Proves a directory of the blob path that is a symlink out of `blobs/` is not followed,
/// whether it is `blobs` or `blobs/sha256`, though the directory it points at holds every
/// blob.
#[tokio::test]
async fn a_blob_directory_that_leaves_the_layout_is_refused() {
    let kind = Kind::bundle();
    for (name, moved) in [("blobs", "blobs"), ("blobs/sha256", "blobs/sha256")] {
        let (dir, artifact) = layout_of(Artifact::bundle(layers()));
        let elsewhere = outside("js-oci-escaped");
        let target = elsewhere.join("moved");
        std::fs::rename(dir.join(moved), &target).expect("move");
        std::os::unix::fs::symlink(&target, dir.join(name)).expect("symlink");
        let layout = Layout::open(dir.path()).await.expect("layout");
        let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
        assert!(
            matches!(&err, LayoutFault::Symlink(p) if p == name),
            "{name}: {err}"
        );
    }
}

/// Proves what is not a regular file where a blob belongs is refused: a regular file in
/// place of `blobs`, and a directory in place of a blob.
#[tokio::test]
async fn a_blob_path_of_the_wrong_kind_of_file_is_refused() {
    let kind = Kind::bundle();
    let (dir, artifact) = layout_of(Artifact::bundle(layers()));
    let layout = Layout::open(dir.path()).await.expect("layout");
    let manifest_path = blob_path(&dir, &artifact.digest);
    std::fs::remove_file(&manifest_path).expect("remove");
    std::fs::create_dir(&manifest_path).expect("directory");
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(
        matches!(&err, LayoutFault::NotRegular(p) if p.ends_with(artifact.digest.hex())),
        "{err}"
    );
    assert!(err.to_string().contains("not the kind of file"));
    std::fs::remove_dir(&manifest_path).expect("remove directory");
    let blobs = dir.join("blobs");
    std::fs::remove_dir_all(&blobs).expect("remove blobs");
    std::fs::write(&blobs, b"a file").expect("file");
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(
        matches!(&err, LayoutFault::NotRegular(p) if p == "blobs"),
        "{err}"
    );
}

/// Proves `index.json` is the layout's list of what it holds: a manifest it does not list
/// is refused even though its blob is there, and an index that is not JSON, or is too
/// long, is refused.
#[tokio::test]
async fn a_manifest_the_index_does_not_list_is_refused() {
    let kind = Kind::bundle();
    let (dir, artifact) = layout_of(Artifact::bundle(layers()));
    let layout = Layout::open(dir.path()).await.expect("layout");
    let other = Digest::of(b"another manifest");
    write_index(&dir, &[&other]);
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(
        matches!(&err, LayoutFault::NotListed(d) if *d == artifact.digest.to_string()),
        "{err}"
    );
    assert!(err.to_string().contains("not listed in index.json"));
    write_index(&dir, &[]);
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(matches!(err, LayoutFault::NotListed(_)), "{err}");
    std::fs::write(dir.join("index.json"), "[]").expect("write");
    let err = fault(manifest_error(&layout, &artifact.digest, &kind).await);
    assert!(
        matches!(
            &err,
            LayoutFault::Malformed {
                file: "index.json",
                ..
            }
        ),
        "{err}"
    );
    write_index(&dir, &[&artifact.digest]);
    let mut short = Kind::bundle();
    short.limits.manifest = 10;
    let err = manifest_error(&layout, &artifact.digest, &short).await;
    assert!(
        matches!(&err, Error::TooLarge { what, limit: 10 } if what == "index.json"),
        "{err}"
    );
    assert!(format!("{layout:?}").contains("Layout"));
    assert!(format!("{err:?}").contains("TooLarge"));
}
