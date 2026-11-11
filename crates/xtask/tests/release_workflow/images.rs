//! The container image of js-lease-server in the release: in artifacts.yml each Linux leg makes
//! its target's image, `sums` lists both, `attest` signs them and writes each image's bundle,
//! and `verify` checks them; release.yml's `publish` uploads them with their bundles.

use super::{artifacts_workflow, job, jobs, legs, step, target_condition, workflow};

/// The two Linux targets an image is made for, with the spelling the workflow gives each in the
/// id of its attestation step.
const IMAGES: [(&str, &str); 2] = [
    ("x86_64-unknown-linux-musl", "x86-64"),
    ("aarch64-unknown-linux-musl", "aarch64"),
];

/// Asserts that the `build` job makes the image in one step, `release image` with the
/// js-lease-server binary just built, the release version, the leg's target and `dist`, whose
/// condition is true on both Linux legs and false on the Mac leg (evaluated for each), after the
/// binaries are checked and before the artifact is uploaded, and that the upload carries
/// `dist/*.oci.tar`. Catches an image made on the Mac leg (no such binary), an image made from a
/// binary that was not checked, and an image left out of the artifact, which `sums` would then
/// miss.
#[test]
fn each_linux_leg_builds_the_server_image_and_uploads_it() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build");
    let lines = step(build, "Build the js-lease-server image");
    let runs: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("run:"))
        .collect();
    assert_eq!(
        runs,
        [
            r#"run: cargo xtask release image --binary "target/$TARGET/release/js-lease-server" --version "$VERSION" --target "$TARGET" --out dist"#
        ]
    );
    let conditions: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("if:"))
        .collect();
    assert_eq!(conditions.len(), 1, "{lines:?}");
    let condition = conditions[0].strip_prefix("if: ").expect("if");
    for leg in legs(build) {
        assert_eq!(
            target_condition(condition, &leg.target),
            leg.target.ends_with("-linux-musl"),
            "{}",
            leg.target
        );
        // The legs that make the image build the server it holds.
        assert_eq!(
            leg.builds
                .iter()
                .any(|b| b == "js-lease-server:js-lease-server"),
            leg.target.ends_with("-linux-musl"),
            "{}",
            leg.target
        );
    }
    let joined = build.join("\n");
    assert_eq!(
        joined.matches("cargo xtask release image").count(),
        1,
        "{joined}"
    );
    let at = |needle: &str| joined.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("- name: Build and package") < at("- name: Check the binaries"));
    assert!(at("- name: Check the binaries") < at("- name: Build the js-lease-server image"));
    assert!(at("- name: Build the js-lease-server image") < at("name: dist-${{ matrix.target }}"));
    assert!(joined.contains(
        "            dist/*.deb\n            dist/*.oci.tar\n          if-no-files-found: error\n"
    ));
}

/// Asserts that every image is in the chain from the build to the release: `sums` expects the
/// name of each target's image and sums `*.oci.tar`; `attest` makes a provenance attestation of
/// each image alone and copies the bundle of that attestation (the step's own output) beside it
/// as `<image>.sigstore.json`, after the attestation; `verify` checks each image's attestation
/// and then from its bundle; `publish` refuses an image without a bundle before it uploads
/// anything, uploads the images and their bundles, and the release's text names the images.
/// Catches an image that is built and listed but never signed, verified or published, an image
/// published without the bundle a deployer verifies it with, and a bundle under another name.
#[test]
fn every_server_image_is_summed_attested_with_its_bundle_verified_and_published() {
    let artifacts = artifacts_workflow();
    let jobs_of = jobs(&artifacts);
    let sums = job(&jobs_of, "sums").join("\n");
    assert!(sums.contains("*.oci.tar > SHA256SUMS"), "{sums}");
    let attest = job(&jobs_of, "attest").join("\n");
    for (target, id) in IMAGES {
        let name = format!("js-lease-server-$VERSION-{target}.oci.tar");
        assert_eq!(sums.matches(&name).count(), 1, "{name}: {sums}");
        let subject = format!(
            "subject-path: dist/js-lease-server-${{{{ inputs.version }}}}-{target}.oci.tar\n"
        );
        assert_eq!(attest.matches(&subject).count(), 1, "{subject}: {attest}");
        let copy = format!(
            r#"cp "$BUNDLE" "dist/js-lease-server-${{VERSION}}-{target}.oci.tar.sigstore.json""#
        );
        assert_eq!(attest.matches(&copy).count(), 1, "{copy}: {attest}");
        // The copy takes the bundle of the attestation just made, of this image.
        assert!(
            attest.find(&subject).expect("subject") < attest.find(&copy).expect("copy"),
            "{attest}"
        );
        let bundle = format!("steps.provenance-image-{id}.outputs.bundle-path");
        assert_eq!(attest.matches(&bundle).count(), 1, "{bundle}: {attest}");
        assert_eq!(
            attest
                .matches(&format!("- id: provenance-image-{id}\n"))
                .count(),
            1,
            "{attest}"
        );
    }
    let verify = job(&jobs_of, "verify").join("\n");
    assert!(
        verify.contains("for file in ./*.tar.gz ./*.deb ./*.oci.tar; do"),
        "{verify}"
    );
    assert!(
        verify.contains("for package in ./*.deb ./*.oci.tar; do"),
        "{verify}"
    );
    assert!(
        verify.contains(
            r#"gh attestation verify "$package" --bundle "$package.sigstore.json" "${signer[@]}""#
        ),
        "{verify}"
    );

    let release = workflow();
    let jobs_of = super::jobs(&release);
    let publish = job(&jobs_of, "publish").join("\n");
    let missing = publish
        .find("for package in ./*.deb ./*.oci.tar; do\n")
        .expect("the check for a bundle");
    assert!(
        publish[missing..].contains(r#"if [ ! -f "$package.sigstore.json" ]; then"#),
        "{publish}"
    );
    assert!(missing < publish.find("uploads.github.com").expect("upload"));
    assert!(
        publish.contains("./*.oci.tar ./*.oci.tar.sigstore.json SHA256SUMS; do"),
        "{publish}"
    );
    assert!(
        publish.contains("js-lease-server container images (.oci.tar) for amd64 and arm64"),
        "{publish}"
    );
}

/// Asserts that the bundles of the images do not make `publish` compare or skip anything new: its
/// skip by name is `*.sigstore.json`, which an image's bundle matches, and the images themselves
/// are compared byte for byte. Catches a skip that was widened to the images.
#[test]
fn publish_still_compares_the_images_and_skips_bundles_by_name() {
    let release = workflow();
    let jobs_of = jobs(&release);
    let publish = job(&jobs_of, "publish").join("\n");
    assert_eq!(
        publish.matches("*.sigstore.json) continue ;;").count(),
        1,
        "{publish}"
    );
    assert!(!publish.contains("*.oci.tar) continue"), "{publish}");
}
