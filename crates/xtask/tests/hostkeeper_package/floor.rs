//! The oldest hostkeeper a package names (`min_hostkeeper`): the default the verb writes, and
//! what a daemon older than it does with the package.

use hostkeeper_config::manifest::{HostkeeperVersion, Manifest, ManifestRefusal};
use xtask::release::Error;
use xtask::release::hostkeeper_package::Platform;
use xtask::release::hostkeeper_package::{MIN_HOSTKEEPER, PackageArgs, file_name, run};

use crate::support::{Scratch, VERSION, binary, expected, manifest_toml, version, write_tree};

/// The manifest names the shipped default hostkeeper, `0.1.92`, unless told otherwise: a daemon
/// of that release reads it and an older one refuses it as `daemon_too_old`, an environment
/// refusal and not a bad package; a `--min-hostkeeper` that is not a version is refused before
/// anything is written. Catches a default that drifted from the release the packages are tested
/// against, and a floor that is written without being judged.
#[test]
fn the_manifest_names_the_default_min_hostkeeper_and_an_older_daemon_refuses_it() {
    let scratch = Scratch::new("hostkeeper-floor");
    let bin = scratch.path().join("js-agent");
    std::fs::write(&bin, binary()).expect("binary");
    let args = |min: &str, out: &str| PackageArgs {
        binary: bin.clone(),
        target: "x86_64-unknown-linux-musl".to_owned(),
        version: VERSION.to_owned(),
        min_hostkeeper: min.to_owned(),
        out: scratch.path().join(out),
    };
    run(&args(MIN_HOSTKEEPER, "dist")).expect("packaged");
    let bytes = std::fs::read(
        scratch
            .path()
            .join("dist")
            .join(file_name(version(), Platform::LinuxX86_64)),
    )
    .expect("the package");
    assert_eq!(
        manifest_toml(&bytes)["min_hostkeeper"].as_str(),
        Some("0.1.92")
    );
    let dir = scratch.path().join("version");
    write_tree(&dir, &bytes, |_, bytes| bytes);
    let daemon = |text: &str| HostkeeperVersion::parse(text).expect("a version");
    let order = expected(Platform::LinuxX86_64);
    assert!(Manifest::load(&dir, &order, &daemon("0.1.92")).is_ok());
    let refused = Manifest::load(&dir, &order, &daemon("0.1.91")).unwrap_err();
    assert_eq!(refused.reason(), "daemon_too_old");
    assert!(matches!(refused, ManifestRefusal::DaemonTooOld { .. }));
    for bad in ["0.1", "v0.1.92", "0.1.92-rc1", ""] {
        let refused = run(&args(bad, "never")).unwrap_err();
        assert_eq!(refused, Error::NotAVersion(bad.to_owned()));
    }
    assert!(!scratch.path().join("never").exists());
}
