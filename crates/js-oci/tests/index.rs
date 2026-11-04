//! An image index as a pin: with a kind that selects a platform, both sources (a
//! registry and a layout directory) take the darwin/arm64 manifest out of it, check every
//! byte on the way, and refuse what is ambiguous, absent, nested or altered.

mod support;

use async_trait::async_trait;
use js_oci::media::{DOCKER_LIST, OCI_INDEX, OCI_MANIFEST};
use js_oci::{BlobSink, BundleSource, Digest, Error, Kind, Layout, LayoutFault, Repository};
use js_testkit::dir::TestDir;
use support::{Artifact, BUNDLE_CONFIG, Fixture, REPO, blob_path, index_of, write_index};

const DARWIN: Option<(&str, &str)> = Some(("darwin", "arm64"));
const LINUX_AMD64: Option<(&str, &str)> = Some(("linux", "amd64"));
const LINUX_ARM64: Option<(&str, &str)> = Some(("linux", "arm64"));

/// A bundle whose layers are `tag`, so that two bundles have different digests.
fn darwin(tag: &str) -> Artifact {
    Artifact::bundle(vec![tag.as_bytes().to_vec(), vec![3u8; 70_000]])
}

/// A Linux image whose layer is `tag`.
fn linux(tag: &str) -> Artifact {
    Artifact::image(
        vec![tag.as_bytes().to_vec()],
        r#"{"os":"linux","architecture":"amd64"}"#,
    )
}

/// A sink that keeps what it is told.
#[derive(Default)]
struct Keeping {
    bytes: Vec<u8>,
}

#[async_trait]
impl BlobSink for Keeping {
    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An index, served by a registry and written as a layout directory whose `index.json`
/// lists it, with the artifacts the test says are in both places.
struct World {
    fx: Fixture,
    dir: TestDir,
    index: Vec<u8>,
    digest: Digest,
}

impl World {
    /// `index` is published with the artifacts of `both` in the registry and the layout,
    /// and those of `registry_only` in the registry alone.
    fn new(index: Vec<u8>, both: &[&Artifact], registry_only: &[&Artifact]) -> Self {
        let fx = Fixture::in_memory();
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-oci-index");
        std::fs::create_dir_all(dir.join("blobs").join("sha256")).expect("blobs");
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#)
            .expect("oci-layout");
        let digest = Digest::of(&index);
        write_index(&dir, &[&digest]);
        std::fs::write(blob_path(&dir, &digest), &index).expect("index blob");
        fx.publish_manifest_as(&digest, &index);
        for artifact in both {
            fx.publish(artifact);
            let blobs = [&artifact.manifest, &artifact.config]
                .into_iter()
                .chain(&artifact.layers);
            for blob in blobs {
                std::fs::write(blob_path(&dir, &Digest::of(blob)), blob).expect("blob");
            }
        }
        for artifact in registry_only {
            fx.publish(artifact);
        }
        Self {
            fx,
            dir,
            index,
            digest,
        }
    }

    /// Replaces the manifest named `digest` in both places by `bytes`.
    fn tamper_manifest(&self, digest: &Digest, bytes: &[u8]) {
        self.fx.publish_manifest_as(digest, bytes);
        std::fs::write(blob_path(&self.dir, digest), bytes).expect("tamper");
    }
}

/// Runs the body against the registry and the layout of `$world`, naming the source.
macro_rules! each_source {
    ($world:expr, |$name:ident, $source:ident| $body:block) => {{
        let client = $world.fx.client();
        let repo: Repository<'_> = client
            .repository($world.fx.location(REPO))
            .expect("repository");
        let layout = Layout::open($world.dir.path()).await.expect("layout");
        let sources: [(&str, &dyn BundleSource); 2] = [
            ("registry", &repo as &dyn BundleSource),
            ("layout", &layout as &dyn BundleSource),
        ];
        for ($name, $source) in sources {
            $body
        }
    }};
}

/// The requests the registry has had since `before` of them.
fn asked_since(world: &World, before: usize) -> Vec<String> {
    world.fx.requests()[before..].to_vec()
}

/// Asserts that a pin naming an index of linux/amd64, linux/arm64 and darwin/arm64
/// resolves, from either source, to the darwin manifest: its digest, the index it came
/// through, its config and its layers' bytes. The request log shows the index, the
/// darwin manifest and its blobs, and no request for either Linux child (the layout does
/// not even hold them). Catches a selection that takes another entry, a manifest that
/// keeps the index's digest, or Linux children fetched along the way.
#[tokio::test]
async fn an_index_selects_the_darwin_arm64_manifest() {
    let (amd, arm, mac) = (linux("amd"), linux("arm"), darwin("mac"));
    let index = index_of(&[
        (OCI_MANIFEST, &amd.digest, LINUX_AMD64),
        (OCI_MANIFEST, &arm.digest, LINUX_ARM64),
        (OCI_MANIFEST, &mac.digest, DARWIN),
    ]);
    let world = World::new(index, &[&mac], &[&amd, &arm]);
    let kind = Kind::bundle_from_index();
    each_source!(world, |name, source| {
        let before = world.fx.requests().len();
        let selected = source
            .fetch_selected(&world.digest, &kind)
            .await
            .expect(name);
        assert_eq!(selected.index.as_ref(), Some(&world.digest), "{name}");
        assert_eq!(selected.manifest.digest(), &mac.digest, "{name}");
        assert_eq!(selected.manifest.bytes(), &mac.manifest[..], "{name}");
        let config = source
            .fetch_config(&selected.manifest, &kind)
            .await
            .expect(name);
        assert_eq!(config, BUNDLE_CONFIG.as_bytes(), "{name}");
        for (descriptor, want) in selected.manifest.layers().iter().zip(&mac.layers) {
            let mut sink = Keeping::default();
            source.fetch_blob(descriptor, &mut sink).await.expect(name);
            assert_eq!(&sink.bytes, want, "{name}");
        }
        let asked = asked_since(&world, before);
        for child in [&amd, &arm] {
            let digest = child.digest.to_string();
            let layer = Digest::of(&child.layers[0]).to_string();
            let config = Digest::of(&child.config).to_string();
            assert!(
                !asked.iter().any(|r| [&digest, &layer, &config]
                    .iter()
                    .any(|d| r.contains(d.as_str()))),
                "{name}: {asked:?}"
            );
        }
        if name == "registry" {
            assert_eq!(asked.len(), 2 + 1 + 2, "{asked:?}");
            assert!(asked[0].contains(world.digest.as_str()), "{asked:?}");
            assert!(asked[1].contains(mac.digest.as_str()), "{asked:?}");
        } else {
            assert!(asked.is_empty(), "the layout asks no registry");
        }
    });
}

/// Asserts the child manifest is requested with the kind's `Accept` header, as the index
/// was. Catches a child request that
/// drops the header, so a registry answers it with another representation or a 404.
#[tokio::test]
async fn the_child_manifest_is_requested_with_the_kinds_accept_header() {
    let mac = darwin("mac");
    let index = index_of(&[(OCI_MANIFEST, &mac.digest, DARWIN)]);
    let world = World::new(index, &[&mac], &[]);
    let client = world.fx.client();
    let repo = client.repository(world.fx.location(REPO)).expect("repo");
    repo.fetch_manifest(&world.digest, &Kind::bundle_from_index())
        .await
        .expect("manifest");
    let want = format!("{OCI_MANIFEST}, {OCI_INDEX}, {DOCKER_LIST}");
    let accepts = world.fx.accepts();
    assert_eq!(accepts.len(), 2, "{accepts:?}");
    for (path, accept) in &accepts {
        assert_eq!(accept.as_deref(), Some(want.as_str()), "{path}");
    }
    assert!(accepts[1].0.ends_with(mac.digest.as_str()), "{accepts:?}");
}

/// Asserts a pin that names a manifest, not an index, is read as it always was by a
/// selecting kind: the same manifest, no index recorded, and one request. Catches a
/// selecting kind that insists on an index or reports the manifest as its own index.
#[tokio::test]
async fn a_manifest_pin_selects_nothing() {
    let mac = darwin("mac");
    let world = World::new(index_of(&[]), &[&mac], &[]);
    each_source!(world, |name, source| {
        let before = world.fx.requests().len();
        let selected = source
            .fetch_selected(&mac.digest, &Kind::bundle_from_index())
            .await;
        if name == "layout" {
            // `index.json` lists only the index, so a manifest pin is not listed.
            assert!(
                matches!(selected, Err(Error::Layout(LayoutFault::NotListed(_)))),
                "{name}"
            );
            continue;
        }
        let selected = selected.expect(name);
        assert_eq!(selected.index, None, "{name}");
        assert_eq!(selected.manifest.digest(), &mac.digest, "{name}");
        assert_eq!(asked_since(&world, before).len(), 1, "{name}");
    });
}

/// Asserts an index that lists only Linux manifests is `NoPlatform` from either source,
/// that the error names the platforms listed, and that nothing but the index was read
/// (no child manifest, no blob). Catches a fall-back to another platform and a fetch
/// before the refusal.
#[tokio::test]
async fn an_index_without_darwin_arm64_is_refused() {
    let (amd, arm) = (linux("amd"), linux("arm"));
    let index = index_of(&[
        (OCI_MANIFEST, &amd.digest, LINUX_AMD64),
        (OCI_MANIFEST, &arm.digest, LINUX_ARM64),
    ]);
    let world = World::new(index, &[], &[&amd, &arm]);
    each_source!(world, |name, source| {
        let before = world.fx.requests().len();
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        let Error::NoPlatform {
            index,
            want,
            listed,
        } = &err
        else {
            panic!("{name}: {err}");
        };
        assert_eq!(index, world.digest.as_str(), "{name}");
        assert_eq!(want, "darwin/arm64", "{name}");
        assert_eq!(listed, r#""linux/amd64", "linux/arm64""#, "{name}");
        let asked = asked_since(&world, before);
        assert!(asked.len() <= 1, "{name}: only the index: {asked:?}");
    });
}

/// Asserts two darwin/arm64 entries with different digests are `AmbiguousPlatform`
/// naming both digests and fetching neither, while the same digest listed twice is one
/// candidate and is selected. Catches picking the first of two, and a repeated entry
/// taken as a conflict.
#[tokio::test]
async fn an_index_with_two_darwin_arm64_manifests_is_refused() {
    let (one, two) = (darwin("one"), darwin("two"));
    let both = index_of(&[
        (OCI_MANIFEST, &one.digest, DARWIN),
        (OCI_MANIFEST, &two.digest, DARWIN),
    ]);
    let world = World::new(both, &[&one, &two], &[]);
    each_source!(world, |name, source| {
        let before = world.fx.requests().len();
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        let Error::AmbiguousPlatform {
            index,
            want,
            digests,
        } = &err
        else {
            panic!("{name}: {err}");
        };
        assert_eq!(index, world.digest.as_str(), "{name}");
        assert_eq!(want, "darwin/arm64", "{name}");
        assert_eq!(
            digests,
            &format!(r#""{}", "{}""#, one.digest, two.digest),
            "{name}"
        );
        assert!(asked_since(&world, before).len() <= 1, "{name}");
    });
    let twice = index_of(&[
        (OCI_MANIFEST, &one.digest, DARWIN),
        (OCI_MANIFEST, &one.digest, DARWIN),
    ]);
    let world = World::new(twice, &[&one], &[]);
    each_source!(world, |name, source| {
        let selected = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect(name);
        assert_eq!(selected.manifest.digest(), &one.digest, "{name}");
    });
}

/// Asserts a registry or layout that serves other bytes for the child's digest is
/// refused with a manifest-digest error, and that a child it does not hold at all is an
/// error from each source (not found at the registry, unreadable in the layout). Catches
/// a child trusted because the index named it.
#[tokio::test]
async fn a_child_manifest_that_does_not_hash_to_its_index_entry_is_refused() {
    let (mac, other) = (darwin("mac"), darwin("other"));
    let index = index_of(&[(OCI_MANIFEST, &mac.digest, DARWIN)]);
    let world = World::new(index.clone(), &[&mac], &[]);
    world.tamper_manifest(&mac.digest, &other.manifest);
    each_source!(world, |name, source| {
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        assert!(
            matches!(&err, Error::ManifestDigest { pinned, actual }
                if *pinned == mac.digest.to_string() && *actual == other.digest.to_string()),
            "{name}: {err}"
        );
    });
    let world = World::new(index, &[], &[]);
    each_source!(world, |name, source| {
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        match name {
            "registry" => assert!(matches!(err, Error::Status { status: 404, .. }), "{err}"),
            _ => assert!(
                matches!(&err, Error::Layout(LayoutFault::Unreadable { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound),
                "{err}"
            ),
        }
    });
}

/// Asserts index bytes that do not hash to the pin are `ManifestDigest` from either
/// source, before the index is read (the bytes served are a valid index naming the
/// darwin manifest) and with no child fetched. Catches an index believed under a digest
/// it does not have.
#[tokio::test]
async fn index_bytes_that_do_not_hash_to_the_pin_are_refused() {
    let (mac, other) = (darwin("mac"), darwin("other"));
    let index = index_of(&[(OCI_MANIFEST, &mac.digest, DARWIN)]);
    let world = World::new(index, &[&mac], &[]);
    let forged = index_of(&[
        (OCI_MANIFEST, &mac.digest, DARWIN),
        (OCI_MANIFEST, &other.digest, DARWIN),
    ]);
    world.tamper_manifest(&world.digest, &forged);
    each_source!(world, |name, source| {
        let before = world.fx.requests().len();
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        assert!(
            matches!(&err, Error::ManifestDigest { pinned, actual }
                if *pinned == world.digest.to_string()
                    && *actual == Digest::of(&forged).to_string()),
            "{name}: {err}"
        );
        assert!(asked_since(&world, before).len() <= 1, "{name}");
    });
    assert_ne!(world.index, forged);
}

/// Asserts an index inside an index is `Index` from either source: by the entry's media
/// type (nothing is fetched), and by a child whose bytes are an index though the entry
/// calls it a manifest. Catches a second level followed, which would let a registry
/// choose the platform manifest behind the first index's back.
#[tokio::test]
async fn a_nested_index_is_refused() {
    let mac = darwin("mac");
    let nested = index_of(&[(OCI_MANIFEST, &mac.digest, DARWIN)]);
    let nested_digest = Digest::of(&nested);
    for media in [OCI_INDEX, DOCKER_LIST] {
        let index = index_of(&[(media, &nested_digest, DARWIN)]);
        let world = World::new(index, &[&mac], &[]);
        each_source!(world, |name, source| {
            let before = world.fx.requests().len();
            let err = source
                .fetch_selected(&world.digest, &Kind::bundle_from_index())
                .await
                .expect_err(name);
            assert!(
                matches!(&err, Error::Index(d) if *d == nested_digest.to_string()),
                "{name}: {err}"
            );
            assert!(asked_since(&world, before).len() <= 1, "{name}");
        });
    }
    let index = index_of(&[(OCI_MANIFEST, &nested_digest, DARWIN)]);
    let world = World::new(index, &[&mac], &[]);
    world.tamper_manifest(&nested_digest, &nested);
    each_source!(world, |name, source| {
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        assert!(matches!(err, Error::Index(_)), "{name}: {err}");
    });
}

/// Asserts the entry's child is read exactly as a manifest pinned by its digest: a child
/// that is not a bundle (an image has no artifact type) is `ArtifactType`, from either
/// source. Catches a child that skips the kind's checks because it came through an index.
#[tokio::test]
async fn a_child_that_is_not_a_bundle_is_refused() {
    let image = Artifact::image(vec![b"layer".to_vec()], BUNDLE_CONFIG);
    let index = index_of(&[(OCI_MANIFEST, &image.digest, DARWIN)]);
    let world = World::new(index, &[&image], &[]);
    each_source!(world, |name, source| {
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle_from_index())
            .await
            .expect_err(name);
        assert!(
            matches!(err, Error::ArtifactType { have: None, .. }),
            "{name}: {err}"
        );
    });
}

/// Asserts a darwin/arm64 entry whose manifest's config states linux/arm64 is selected
/// but its config is refused as `Platform`, from either source. Catches trusting the
/// index's platform over the config's own statement.
#[tokio::test]
async fn an_index_entry_with_the_wrong_platform_in_its_config_is_refused() {
    let liar = Artifact::bundle_with_config(
        vec![b"layer".to_vec()],
        r#"{"os":"linux","architecture":"arm64"}"#,
    );
    let index = index_of(&[(OCI_MANIFEST, &liar.digest, DARWIN)]);
    let world = World::new(index, &[&liar], &[]);
    let kind = Kind::bundle_from_index();
    each_source!(world, |name, source| {
        let selected = source
            .fetch_selected(&world.digest, &kind)
            .await
            .expect(name);
        let err = source
            .fetch_config(&selected.manifest, &kind)
            .await
            .expect_err(name);
        assert_eq!(
            err.to_string(),
            "the artifact is for linux/arm64, not darwin/arm64",
            "{name}"
        );
    });
}

/// Asserts the kinds that do not select a platform still refuse an index that would
/// have a darwin manifest, from either source, and that the index policy is part of the
/// kind. Catches the default kinds changing behaviour.
#[tokio::test]
async fn kinds_without_selection_still_refuse_an_index() {
    let mac = darwin("mac");
    let index = index_of(&[(OCI_MANIFEST, &mac.digest, DARWIN)]);
    let world = World::new(index, &[&mac], &[]);
    assert_ne!(Kind::bundle(), Kind::bundle_from_index());
    assert_eq!(Kind::bundle().index, js_oci::IndexPolicy::default());
    each_source!(world, |name, source| {
        let err = source
            .fetch_selected(&world.digest, &Kind::bundle())
            .await
            .expect_err(name);
        assert!(matches!(err, Error::Index(_)), "{name}: {err}");
    });
}
