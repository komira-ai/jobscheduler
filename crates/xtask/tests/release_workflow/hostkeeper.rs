//! The hostkeeper packages of js-agent in the release: in artifacts.yml each leg makes its
//! platform's package, `sums` writes the index and lists both, `attest` signs them and writes
//! each package's bundle, and `verify` checks them; release.yml's `publish` uploads them with
//! their bundles.

use xtask::release::Version;
use xtask::release::hostkeeper_package::{Platform, TARGETS, file_name};

use super::{artifacts_workflow, job, jobs, legs, step, workflow};

/// The file name of each platform's package, with `version` written where the version goes.
fn packages(version: &str) -> Vec<String> {
    let v: Version = "1.2.3".parse().expect("a version");
    Platform::ALL
        .into_iter()
        .map(|platform| file_name(v, platform).replace("1.2.3", version))
        .collect()
}

/// Asserts that every leg of the matrix builds `js-agent`, and that the `build` job packages it for
/// hostkeeper in one named step with no condition (so each leg makes its platform's package)
/// after the archives are made and before the binaries are checked, with the binary just built,
/// the leg's target, the release version and `dist` as the directory; that the xtask verb maps
/// each of the matrix targets to a platform; and that the artifact of a leg carries the
/// `.tar.zst` beside the `.tar.gz`. Catches a leg that ships no package, a package made from
/// another leg's binary, and a package left out of the artifact, which `sums` would then miss.
#[test]
fn each_leg_packages_the_agent_for_hostkeeper_and_uploads_the_package() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build");
    for leg in legs(build) {
        assert!(
            leg.builds.iter().any(|entry| entry == "js-agent:js-agent"),
            "{leg:?}"
        );
        assert!(
            TARGETS.iter().any(|(target, _)| *target == leg.target),
            "{leg:?}"
        );
    }
    let packaging = step(build, "Package js-agent for hostkeeper");
    assert_eq!(
        packaging
            .iter()
            .filter(|l| l.trim_start().starts_with("run:"))
            .count(),
        1,
        "{packaging:?}"
    );
    assert!(
        packaging.iter().any(|l| l.trim()
            == r#"run: cargo xtask release hostkeeper-package --binary "target/$TARGET/release/js-agent" --target "$TARGET" --version "$VERSION" --out dist"#),
        "{packaging:?}"
    );
    assert!(
        !packaging.iter().any(|l| l.trim_start().starts_with("if:")),
        "{packaging:?}"
    );
    let joined = build.join("\n");
    assert_eq!(joined.matches("release hostkeeper-package").count(), 1);
    let at = |needle: &str| joined.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("- name: Build and package") < at("- name: Package js-agent for hostkeeper"));
    assert!(at("- name: Package js-agent for hostkeeper") < at("- name: Check the binaries"));
    assert!(joined.contains("path: |\n            dist/*.tar.gz\n            dist/*.tar.zst\n"));
}

/// Asserts that `sums` writes the hostkeeper index with one named command, from a checkout of the
/// commit and a toolchain, after the artifacts are downloaded and before the list of assets is
/// checked and SHA256SUMS is written; and that SHA256SUMS lists the archives, the packages and the
/// index. Catches an index written after the sums (not in them, so not attested), and sums that
/// leave a package out.
#[test]
fn sums_writes_the_index_and_lists_the_packages_and_the_index() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let sums = job(&jobs, "sums");
    let index = step(sums, "Write the hostkeeper release index");
    assert!(
        index.iter().any(|l| l.trim()
            == r#"run: cargo xtask release hostkeeper-index --dir dist --version "$VERSION""#),
        "{index:?}"
    );
    let joined = sums.join("\n");
    let at = |needle: &str| joined.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("actions/checkout@") < at("actions/download-artifact@"));
    assert!(at("actions/download-artifact@") < at("run: rustup toolchain install"));
    assert!(at("run: rustup toolchain install") < at("- name: Write the hostkeeper release index"));
    assert!(
        at("- name: Write the hostkeeper release index") < at("- name: Write and check SHA256SUMS")
    );
    assert!(joined.contains("VERSION: ${{ inputs.version }}"));
    assert!(
        joined.contains("sha256sum -- *.tar.gz *.tar.zst js-agent-index.json > SHA256SUMS"),
        "{joined}"
    );
}

/// Asserts that artifacts.yml's `attest` attests the whole SHA256SUMS (so the packages and the
/// index, which are in it, as the archives are) and then each package alone, copying the bundle
/// of that attestation beside the package as `<package>.sigstore.json` (named as
/// `hostkeeper_package::file_name` names the package) and uploading the bundles; that `verify`
/// checks the attestation of each archive, each package and the index against artifacts.yml on
/// main at the commit from a hosted runner, and each package from its bundle as well; and that
/// release.yml's `publish` downloads the bundles, refuses a package without one, and uploads the
/// archives, the packages, their bundles, the index and SHA256SUMS. Catches a package or the index
/// that is built and listed but never signed, verified or published, a package published without
/// the bundle hostkeeper needs, and a bundle under a name hostkeeper does not look for.
#[test]
fn attest_verify_and_publish_carry_the_packages_their_bundles_and_the_index() {
    let artifacts = artifacts_workflow();
    let jobs = jobs(&artifacts);
    let attest = job(&jobs, "attest").join("\n");
    assert!(
        attest.contains("subject-checksums: dist/SHA256SUMS"),
        "{attest}"
    );
    for (input, shell) in packages("${{ inputs.version }}")
        .into_iter()
        .zip(packages("$VERSION"))
    {
        let subject = format!("subject-path: dist/{input}\n");
        assert_eq!(attest.matches(&subject).count(), 1, "{subject}: {attest}");
        let copy = format!(r#"cp "$BUNDLE" "dist/{shell}.sigstore.json""#);
        assert_eq!(attest.matches(&copy).count(), 1, "{copy}: {attest}");
        // The copy takes the bundle of the attestation just made, of this package.
        assert!(
            attest.find(&subject).expect("subject") < attest.find(&copy).expect("copy"),
            "{attest}"
        );
    }
    assert!(
        attest.contains("VERSION: ${{ inputs.version }}"),
        "{attest}"
    );
    assert_eq!(attest.matches("cp \"$BUNDLE\"").count(), 3, "{attest}");
    assert!(
        attest.contains("name: bundles\n          path: dist/*.sigstore.json\n"),
        "{attest}"
    );
    let verify = job(&jobs, "verify").join("\n");
    assert!(
        verify.contains("for file in ./*.tar.gz ./*.tar.zst ./js-agent-index.json; do"),
        "{verify}"
    );
    assert!(
        verify.contains(
            r#"gh attestation verify "$package" --bundle "$package.sigstore.json" "${signer[@]}""#
        ),
        "{verify}"
    );
    for flag in [
        r#"--signer-workflow "$REPO/.github/workflows/artifacts.yml""#,
        "--source-ref refs/heads/main",
        r#"--source-digest "$COMMIT""#,
        "--deny-self-hosted-runners",
    ] {
        assert!(verify.contains(flag), "{flag}: {verify}");
    }
    assert!(verify.contains("name: bundles"), "{verify}");
    let release = workflow();
    let jobs = super::jobs(&release);
    let publish = job(&jobs, "publish").join("\n");
    assert!(publish.contains("name: bundles"), "{publish}");
    assert!(
        publish.contains("for file in ./*.tar.gz ./*.tar.zst ./*.tar.zst.sigstore.json ./js-agent-index.json SHA256SUMS; do"),
        "{publish}"
    );
    let missing = publish
        .find(r#"if [ ! -f "$package.sigstore.json" ]; then"#)
        .expect("the check for a package's bundle");
    let first_upload = publish.find("uploads.github.com").expect("upload");
    assert!(missing < first_upload, "{publish}");
}

/// Asserts that `publish` compares every file a release already has with this run's, byte for
/// byte, except the bundles, which it skips by name (each signing writes new bytes for the same
/// package, and the package itself is compared), and that the skip is the only one besides an
/// unfinished upload. Catches a re-run of a published release that fails on its bundles alone,
/// and a skip widened to the packages.
#[test]
fn publish_compares_every_file_but_the_bundles() {
    let release = workflow();
    let jobs = jobs(&release);
    let publish = job(&jobs, "publish").join("\n");
    let skip = "            [ \"$state\" = uploaded ] || continue\n            case \"$name\" in\n              *.sigstore.json) continue ;;\n            esac\n            gh api -H \"Accept: application/octet-stream\"";
    assert!(publish.contains(skip), "{publish}");
    assert_eq!(publish.matches("continue").count(), 2, "{publish}");
}
