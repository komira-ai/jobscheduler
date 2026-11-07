//! The two verbs as the release workflow runs them (`cargo xtask release hostkeeper-package`,
//! `cargo xtask release hostkeeper-index`) and each way they refuse: the line the package verb
//! prints, the index it writes read back by hostkeeper's parser, and the refusals that write
//! nothing.

use std::path::Path;

use hostkeeper_config::index::ReleaseIndex;
use hostkeeper_config::names::{ServiceName, Version as HostVersion};
use sha2::{Digest, Sha256};
use xtask::release::Error;
use xtask::release::hostkeeper_index::{INDEX_FILE, IndexArgs, run as index};
use xtask::release::hostkeeper_package::Platform;
use xtask::release::hostkeeper_package::{PackageArgs, file_name, run as package_run};

use crate::support::{ALL, Scratch, TEST_MIN, VERSION, binary, hk, package, version};

const TARGETS: [(&str, Platform); 3] = [
    ("aarch64-unknown-linux-musl", Platform::LinuxArm64),
    ("x86_64-unknown-linux-musl", Platform::LinuxX86_64),
    ("aarch64-apple-darwin", Platform::MacosArm64),
];

fn args(binary: &Path, target: &str, version: &str, out: &Path) -> PackageArgs {
    PackageArgs {
        binary: binary.to_owned(),
        target: target.to_owned(),
        version: version.to_owned(),
        min_hostkeeper: TEST_MIN.to_owned(),
        out: out.to_owned(),
    }
}

/// A binary on disk in `dir`.
fn binary_in(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("js-agent");
    std::fs::write(&path, binary()).expect("binary");
    path
}

/// Writes the three packages of the stand-in binary to `dir`.
fn three(dir: &Path) {
    let bin = binary_in(dir);
    for (target, _) in TARGETS {
        package_run(&args(&bin, target, VERSION, dir)).expect("packaged");
    }
}

fn index_args(dir: &Path, version: &str) -> IndexArgs {
    IndexArgs {
        dir: dir.to_owned(),
        version: version.to_owned(),
    }
}

/// The verbs' platforms are hostkeeper's, spelled and ordered as it spells and orders them: the
/// index lists packages in that order, and a host looks its platform up by that spelling.
/// Catches a spelling or an order that drifted from hostkeeper's.
#[test]
fn the_platforms_are_spelled_and_ordered_as_hostkeeper_does() {
    let ours: Vec<&str> = ALL.iter().map(|p| p.as_str()).collect();
    let theirs: Vec<&str> = hostkeeper_config::platform::Platform::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    assert_eq!(ours, theirs);
    for platform in ALL {
        assert_eq!(platform.to_string(), platform.as_str());
        assert_eq!(hk(platform).as_str(), platform.as_str());
    }
    let mut sorted = ALL;
    sorted.sort();
    assert_eq!(sorted, ALL);
}

/// The verb prints the name, digest and size of what it wrote, and what it wrote is the package
/// the library builds. Catches a printed digest or size that is not the file's, which the
/// workflow's summary and the index would repeat.
#[test]
fn the_package_verb_writes_the_package_and_prints_its_name_digest_and_size() {
    let scratch = Scratch::new("hostkeeper-verb-package");
    let bin = binary_in(scratch.path());
    for (target, platform) in TARGETS {
        let out = scratch.path().join("dist");
        let line = package_run(&args(&bin, target, VERSION, &out)).expect("packaged");
        let name = file_name(version(), platform);
        let bytes = std::fs::read(out.join(&name)).expect("the package");
        assert_eq!(bytes, package(platform));
        assert_eq!(
            line,
            format!(
                "{name}: sha256 {}, {} bytes",
                hex::encode(Sha256::digest(&bytes)),
                bytes.len()
            )
        );
    }
}

/// Catches a verb that writes before it has judged its inputs: a target no package is made for,
/// a version that is not three plain numbers, a binary that cannot be read and a binary that
/// does not report the version leave no directory behind.
#[test]
fn the_package_verb_refuses_a_bad_target_version_or_binary_and_writes_nothing() {
    let scratch = Scratch::new("hostkeeper-verb-refusals");
    let bin = binary_in(scratch.path());
    let out = scratch.path().join("dist");
    let linux = "x86_64-unknown-linux-musl";
    assert_eq!(
        package_run(&args(&bin, "x86_64-unknown-linux-gnu", VERSION, &out)),
        Err(Error::UnknownTarget("x86_64-unknown-linux-gnu".to_owned()))
    );
    for bad in [
        "1.2",
        "v1.2.3",
        "01.2.3",
        "1.2.3-rc1",
        "1.2.3+0123456789ab",
        "",
    ] {
        assert_eq!(
            package_run(&args(&bin, linux, bad, &out)),
            Err(Error::NotAVersion(bad.to_owned())),
            "{bad:?}"
        );
    }
    let missing = scratch.path().join("nothing-here");
    let refused = package_run(&args(&missing, linux, VERSION, &out)).unwrap_err();
    assert!(
        matches!(&refused, Error::Io { path, .. } if Path::new(path) == missing),
        "{refused:?}"
    );
    // A binary of another version (a longer one that starts with the same digits among them),
    // one that names no version, and one that has the version but not as its `--version` text.
    for held in [
        "1.2.4\nruntimes: docker crun",
        "1.2.30\nruntimes: docker crun",
        "no version here",
        "1.2.3 and nothing after it",
    ] {
        let other = scratch.path().join("other");
        std::fs::write(&other, held).expect("binary");
        assert_eq!(
            package_run(&args(&other, linux, VERSION, &out)),
            Err(Error::VersionNotInBinary {
                path: other.display().to_string(),
                version: VERSION.to_owned()
            }),
            "{held:?}"
        );
    }
    assert!(!out.exists(), "nothing was made");
}

/// Catches a verb that overwrites a package that exists (a second run of a release would replace
/// what the first signed) and one that hides a directory it cannot make or a file it cannot
/// write: each is an error naming the path, and the package already there is untouched.
#[test]
fn the_package_verb_refuses_an_existing_package_and_an_out_it_cannot_make() {
    let scratch = Scratch::new("hostkeeper-verb-existing");
    let bin = binary_in(scratch.path());
    let out = scratch.path().join("dist");
    let target = "aarch64-apple-darwin";
    package_run(&args(&bin, target, VERSION, &out)).expect("packaged");
    let path = out.join(file_name(version(), Platform::MacosArm64));
    std::fs::write(&path, b"already here").expect("replace");
    let again = package_run(&args(&bin, target, VERSION, &out)).unwrap_err();
    assert!(
        matches!(&again, Error::Io { path: p, why } if Path::new(p) == path && why.contains("exists")),
        "{again:?}"
    );
    assert_eq!(std::fs::read(&path).expect("the file"), b"already here");
    // An out below a file cannot be made.
    let below = bin.join("dist");
    let refused = package_run(&args(&bin, target, VERSION, &below)).unwrap_err();
    assert!(
        matches!(&refused, Error::Io { path: p, .. } if Path::new(p) == below),
        "{refused:?}"
    );
}

/// The index of the three packages is hostkeeper's own canonical index: its parser reads it back
/// for this service and version, each package's file name, SHA-256 and size are the file's, and
/// files of other kinds in the directory are left alone. Catches an index a host would refuse, a
/// digest or size of another file, and one that is not in canonical form.
#[test]
fn the_index_lists_the_three_packages_with_their_digests_and_hostkeeper_reads_it_back() {
    let scratch = Scratch::new("hostkeeper-verb-index");
    let dir = scratch.path();
    three(dir);
    std::fs::write(
        dir.join("js-agent-1.2.3-aarch64-apple-darwin.tar.gz"),
        b"another archive",
    )
    .expect("a file");
    std::fs::write(
        dir.join(format!(
            "{}.sigstore.json",
            file_name(version(), Platform::LinuxArm64)
        )),
        b"{}",
    )
    .expect("a bundle");
    let line = index(&index_args(dir, VERSION)).expect("indexed");
    assert_eq!(line, "js-agent-index.json: 3 packages");
    let text = std::fs::read_to_string(dir.join(INDEX_FILE)).expect("the index");
    let parsed = ReleaseIndex::parse(
        &text,
        &ServiceName::new("js-agent").expect("a name"),
        &HostVersion::new(VERSION).expect("a version"),
    )
    .expect("hostkeeper reads the index");
    assert_eq!(parsed.to_json(), text, "canonical form");
    for platform in ALL {
        let entry = parsed
            .entry_for(hk(platform))
            .expect("an entry per platform");
        let bytes = std::fs::read(dir.join(file_name(version(), platform))).expect("the package");
        assert_eq!(
            entry.sha256.as_str(),
            hex::encode(Sha256::digest(&bytes)),
            "{platform}"
        );
        assert_eq!(entry.bytes, bytes.len() as u64, "{platform}");
    }
    // A second run does not replace the index.
    let again = index(&index_args(dir, VERSION)).unwrap_err();
    assert!(
        matches!(&again, Error::Io { why, .. } if why.contains("exists")),
        "{again:?}"
    );
}

/// Catches an index of a set of packages that is not the three of the version: a missing one,
/// an extra one (another platform, another version), a misnamed one; each is refused naming the
/// files, and no index is written. Also a directory that cannot be read and a bad version.
#[test]
fn the_index_refuses_a_missing_extra_or_misnamed_package_and_writes_nothing() {
    let scratch = Scratch::new("hostkeeper-verb-index-refusals");
    let dir = scratch.path();
    three(dir);
    let name = |platform| file_name(version(), platform);
    let arm = dir.join(name(Platform::LinuxArm64));
    let moved = dir.join("moved");
    std::fs::rename(&arm, &moved).expect("move");
    assert_eq!(
        index(&index_args(dir, VERSION)),
        Err(Error::Packages(format!(
            "missing {}",
            name(Platform::LinuxArm64)
        )))
    );
    // Misnamed: the right bytes under the name of another version.
    let other = "js-agent-1.2.4-linux-arm64.tar.zst";
    std::fs::rename(&moved, dir.join(other)).expect("rename");
    assert_eq!(
        index(&index_args(dir, VERSION)),
        Err(Error::Packages(format!(
            "missing {}; unexpected {other}",
            name(Platform::LinuxArm64)
        )))
    );
    // Extra: the right set and one more.
    std::fs::rename(dir.join(other), &arm).expect("restore");
    std::fs::write(dir.join("js-agent-1.2.3-linux-riscv.tar.zst"), b"x").expect("extra");
    assert_eq!(
        index(&index_args(dir, VERSION)),
        Err(Error::Packages(
            "unexpected js-agent-1.2.3-linux-riscv.tar.zst".to_owned()
        ))
    );
    assert!(!dir.join(INDEX_FILE).exists());
    assert_eq!(
        index(&index_args(dir, "1.2")),
        Err(Error::NotAVersion("1.2".to_owned()))
    );
    let nowhere = dir.join("nowhere");
    let refused = index(&index_args(&nowhere, VERSION)).unwrap_err();
    assert!(
        matches!(&refused, Error::Io { path, .. } if Path::new(path) == nowhere),
        "{refused:?}"
    );
}

/// A package that is a directory where a file is expected is an error naming it, not a panic.
#[test]
fn the_index_refuses_a_package_it_cannot_read() {
    let scratch = Scratch::new("hostkeeper-verb-index-unreadable");
    let dir = scratch.path();
    three(dir);
    let arm = dir.join(file_name(version(), Platform::LinuxArm64));
    std::fs::remove_file(&arm).expect("remove");
    std::fs::create_dir(&arm).expect("a directory of that name");
    let refused = index(&index_args(dir, VERSION)).unwrap_err();
    assert!(
        matches!(&refused, Error::Io { path, .. } if Path::new(path) == arm),
        "{refused:?}"
    );
}
