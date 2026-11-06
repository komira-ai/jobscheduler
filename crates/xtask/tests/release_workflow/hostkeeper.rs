//! The hostkeeper packages of js-agent in the release workflow: each leg makes its platform's
//! package, `sums` writes the index and lists both, and `attest`, `verify` and `publish` carry
//! them.

use xtask::release::hostkeeper_package::TARGETS;

use super::{job, jobs, legs, step, workflow};

/// Asserts that every leg of the matrix builds `js-agent`, and that the `build` job packages it for
/// hostkeeper in one named step with no condition (so each leg makes its platform's package)
/// after the archives are made and before the binaries are checked, with the binary just built,
/// the leg's target, the release version and `dist` as the directory; that the xtask verb maps
/// each of the matrix targets to a platform; and that the artifact of a leg carries the
/// `.tar.zst` beside the `.tar.gz`. Catches a leg that ships no package, a package made from
/// another leg's binary, and a package left out of the artifact, which `sums` would then miss.
#[test]
fn each_leg_packages_the_agent_for_hostkeeper_and_uploads_the_package() {
    let text = workflow();
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
    let text = workflow();
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
    assert!(joined.contains("VERSION: ${{ needs.plan.outputs.version }}"));
    assert!(
        joined.contains("sha256sum -- *.tar.gz *.tar.zst js-agent-index.json > SHA256SUMS"),
        "{joined}"
    );
}

/// Asserts that `attest` attests the whole SHA256SUMS (so the packages and the index, which are
/// in it, as the archives are), that `verify` checks the attestation of each archive, each
/// package and the index against this workflow on main at the commit, and that `publish` uploads
/// the archives, the packages, the index and SHA256SUMS. Catches a package or the index that is
/// built and listed but never verified or published.
#[test]
fn attest_verify_and_publish_carry_the_packages_and_the_index() {
    let text = workflow();
    let jobs = jobs(&text);
    let attest = job(&jobs, "attest").join("\n");
    assert!(
        attest.contains("subject-checksums: dist/SHA256SUMS"),
        "{attest}"
    );
    let verify = job(&jobs, "verify").join("\n");
    assert!(
        verify.contains("for file in ./*.tar.gz ./*.tar.zst ./js-agent-index.json; do"),
        "{verify}"
    );
    for flag in [
        r#"--signer-workflow "$REPO/.github/workflows/release.yml""#,
        "--source-ref refs/heads/main",
        r#"--source-digest "$COMMIT""#,
    ] {
        assert!(verify.contains(flag), "{flag}: {verify}");
    }
    let publish = job(&jobs, "publish").join("\n");
    assert!(
        publish.contains("for file in ./*.tar.gz ./*.tar.zst ./js-agent-index.json SHA256SUMS; do"),
        "{publish}"
    );
}
