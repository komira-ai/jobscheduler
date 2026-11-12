//! The Debian packages of js-agent in the release: in artifacts.yml each Linux leg makes its
//! architecture's package, `sums` lists both, `attest` signs them and writes each package's
//! bundle, and `verify` checks them; release.yml's `publish` uploads them with their bundles. No
//! hostkeeper archive, index or package verb is run by either file any more.

use super::{artifacts_workflow, job, jobs, legs, step, workflow};

/// The two architectures a .deb is made for, with the platform `release deb` names each by.
const DEBS: [(&str, &str); 2] = [("amd64", "linux-x86_64"), ("arm64", "linux-arm64")];

/// The `deb` and `deb_arch` of each leg, by target.
const LEG_DEBS: [(&str, &str, &str); 3] = [
    ("x86_64-unknown-linux-musl", "linux-x86_64", "amd64"),
    ("aarch64-unknown-linux-musl", "linux-arm64", "arm64"),
    ("aarch64-apple-darwin", "", ""),
];

/// Asserts that the matrix names, for each leg, the platform and the architecture of its .deb
/// (the two Linux legs; the Mac leg has none), that every leg builds `js-agent`, and that the
/// `build` job packages it in three named steps (`release deb`, `dpkg-deb --info`, `dpkg-deb
/// --contents`), each running one command under `matrix.deb != ''`, after the binaries are
/// checked and before the artifact is uploaded, with the binary just built, the release version
/// and `dist`; and that the upload carries the `.deb` beside the `.tar.gz`. Catches a leg that
/// ships no package, a package made from another leg's binary, a package made on the Mac leg,
/// and a package left out of the artifact, which `sums` would then miss.
#[test]
fn each_linux_leg_packages_the_agent_as_a_deb_and_uploads_it() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let build = job(&jobs, "build");
    let legs = legs(build);
    assert_eq!(legs.len(), LEG_DEBS.len(), "{legs:?}");
    for (leg, (target, deb, arch)) in legs.iter().zip(LEG_DEBS) {
        assert_eq!(
            (leg.target.as_str(), leg.deb.as_str(), leg.deb_arch.as_str()),
            (target, deb, arch),
            "{leg:?}"
        );
        assert!(
            leg.builds.iter().any(|entry| entry == "js-agent:js-agent"),
            "{leg:?}"
        );
    }
    let condition = "if: ${{ matrix.deb != '' }}";
    for (name, command) in [
        (
            "Package js-agent as a .deb",
            r#"run: cargo xtask release deb --binary "target/$TARGET/release/js-agent" --version "$VERSION" --platform "$DEB" --out dist"#,
        ),
        (
            "Show the control file of the .deb",
            r#"run: dpkg-deb --info "dist/js-agent_${VERSION}_${DEB_ARCH}.deb""#,
        ),
        (
            "List the files of the .deb",
            r#"run: dpkg-deb --contents "dist/js-agent_${VERSION}_${DEB_ARCH}.deb""#,
        ),
    ] {
        let lines = step(build, name);
        let runs: Vec<&str> = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| l.starts_with("run:"))
            .collect();
        assert_eq!(runs, [command], "{name}");
        assert!(lines.iter().any(|l| l.trim() == condition), "{name}");
    }
    let packaging = step(build, "Package js-agent as a .deb");
    assert!(
        packaging
            .iter()
            .any(|l| l.trim() == "DEB: ${{ matrix.deb }}"),
        "{packaging:?}"
    );
    let joined = build.join("\n");
    assert_eq!(
        joined.matches("cargo xtask release deb").count(),
        1,
        "{joined}"
    );
    assert!(!joined.contains("hostkeeper"), "{joined}");
    let at = |needle: &str| joined.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("- name: Build and package") < at("- name: Check the binaries"));
    assert!(at("- name: Check the binaries") < at("- name: Package js-agent as a .deb"));
    assert!(at("- name: Package js-agent as a .deb") < at("name: dist-${{ matrix.target }}"));
    assert!(joined.contains("path: |\n            dist/*.tar.gz\n            dist/*.deb\n"));
    assert!(joined.contains(
        "path: |\n            dist/*.tar.gz\n            dist/*.deb\n            dist/*.oci.tar\n"
    ));
    // The platform the matrix names is one `release deb` is given for the architecture.
    for (arch, platform) in DEBS {
        assert!(
            legs.iter().any(|l| l.deb == platform && l.deb_arch == arch),
            "{arch} {platform}"
        );
    }
}

/// Asserts that `sums` runs no repository code and no hostkeeper verb (it downloads the legs'
/// artifacts and sums them), lists the archives, the two .debs and the two images, and writes
/// SHA256SUMS over exactly the archives, the .debs and the images, after the list of assets is
/// checked. Catches sums that leave a package out, a leftover index, and a checkout beside the
/// sums.
#[test]
fn sums_lists_the_archives_and_the_debs_and_nothing_else() {
    let text = artifacts_workflow();
    let jobs = jobs(&text);
    let sums = job(&jobs, "sums");
    let joined = sums.join("\n");
    for banned in [
        "actions/checkout",
        "rustup",
        "cargo ",
        "hostkeeper",
        "index",
    ] {
        assert!(!joined.contains(banned), "{banned}: {joined}");
    }
    assert!(joined.contains("pattern: dist-*\n          merge-multiple: true\n"));
    assert!(joined.contains("VERSION: ${{ inputs.version }}"));
    assert!(
        joined.contains("sha256sum -- *.tar.gz *.deb *.oci.tar > SHA256SUMS"),
        "{joined}"
    );
    assert!(!joined.contains("*.tar.zst"), "{joined}");
    let at = |needle: &str| joined.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("actions/download-artifact@") < at("- name: Write and check SHA256SUMS"));
    assert!(at("if [ \"$actual\" != \"$expected\" ]") < at("> SHA256SUMS"));
}

/// Asserts that artifacts.yml's `attest` attests the whole SHA256SUMS (so the archives and the
/// .debs, which are in it) and then each .deb alone, copying the bundle of that attestation
/// beside the package as `<package>.sigstore.json` and uploading the bundles; that `verify`
/// checks the attestation of each archive and each .deb against artifacts.yml on main at the
/// commit from a hosted runner, and each .deb from its bundle as well; and that release.yml's
/// `publish` downloads the bundles, refuses a .deb without one, and uploads the archives, the
/// .debs, their bundles and SHA256SUMS. Catches a .deb that is built and listed but never
/// signed, verified or published, a .deb published without the bundle the apt jobs verify it
/// with, and a bundle under another name.
#[test]
fn attest_verify_and_publish_carry_the_debs_and_their_bundles() {
    let artifacts = artifacts_workflow();
    let jobs = jobs(&artifacts);
    let attest = job(&jobs, "attest").join("\n");
    assert!(
        attest.contains("subject-checksums: dist/SHA256SUMS"),
        "{attest}"
    );
    for (arch, _) in DEBS {
        let subject = format!("subject-path: dist/js-agent_${{{{ inputs.version }}}}_{arch}.deb\n");
        assert_eq!(attest.matches(&subject).count(), 1, "{subject}: {attest}");
        let copy = format!(r#"cp "$BUNDLE" "dist/js-agent_${{VERSION}}_{arch}.deb.sigstore.json""#);
        assert_eq!(attest.matches(&copy).count(), 1, "{copy}: {attest}");
        // The copy takes the bundle of the attestation just made, of this package.
        assert!(
            attest.find(&subject).expect("subject") < attest.find(&copy).expect("copy"),
            "{attest}"
        );
        let id = format!("steps.provenance-deb-{arch}.outputs.bundle-path");
        assert_eq!(attest.matches(&id).count(), 1, "{id}: {attest}");
    }
    assert!(
        attest.contains("VERSION: ${{ inputs.version }}"),
        "{attest}"
    );
    // Two for the .debs and two for the images (images.rs reads the latter).
    assert_eq!(attest.matches("cp \"$BUNDLE\"").count(), 4, "{attest}");
    assert!(
        attest.contains("name: bundles\n          path: dist/*.sigstore.json\n"),
        "{attest}"
    );
    let verify = job(&jobs, "verify").join("\n");
    assert!(
        verify.contains("for file in ./*.tar.gz ./*.deb ./*.oci.tar; do"),
        "{verify}"
    );
    assert!(
        verify.contains(
            r#"gh attestation verify "$package" --bundle "$package.sigstore.json" "${signer[@]}""#
        ),
        "{verify}"
    );
    assert!(
        verify.contains("for package in ./*.deb ./*.oci.tar; do"),
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
        publish.contains("for file in ./*.tar.gz ./*.deb ./*.deb.sigstore.json ./*.oci.tar ./*.oci.tar.sigstore.json SHA256SUMS; do"),
        "{publish}"
    );
    assert!(
        publish.contains("for package in ./*.deb ./*.oci.tar; do"),
        "{publish}"
    );
    let missing = publish
        .find(r#"if [ ! -f "$package.sigstore.json" ]; then"#)
        .expect("the check for a package's bundle");
    let first_upload = publish.find("uploads.github.com").expect("upload");
    assert!(missing < first_upload, "{publish}");
    for name in [
        "tar.zst",
        "js-agent-index.json",
        "hostkeeper-package",
        "hostkeeper-index",
    ] {
        assert!(!publish.contains(name), "{name}: {publish}");
        assert!(!artifacts.contains(name), "{name} in artifacts.yml");
    }
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
    // The other `continue` is the macOS archives, which `promote-macos` adds.
    assert_eq!(publish.matches("continue").count(), 3, "{publish}");
    assert!(publish.contains("              *-aarch64-apple-darwin.tar.gz) continue ;;\n"));
}
