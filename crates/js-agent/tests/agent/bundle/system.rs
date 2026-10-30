//! The machine's own bundle host, over real directories and a scripted `sw_vers` and `df`.
//! Each test says the defect it catches.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use js_agent::CommandRunner;
use js_agent::bundle::{BundleHost, Fetch, Needs, SystemHost};
use js_bundle::BundleConfig;

use super::fake::digest;
use crate::support::Scratch;

const CONFIG: &str = include_str!("../../../../js-bundle/tests/fixtures/minimal.canonical.json");

/// A runner that answers the lines it is given and fails every other.
struct Runner(BTreeMap<String, Result<String, String>>);

impl Runner {
    fn answering(line: &str, answer: Result<&str, &str>) -> Self {
        let answer = answer.map(str::to_owned).map_err(str::to_owned);
        Self(BTreeMap::from([(line.to_owned(), answer)]))
    }
}

impl CommandRunner for Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let line = format!("{program} {}", args.join(" "));
        self.0
            .get(&line)
            .cloned()
            .unwrap_or_else(|| Err(format!("{program}: not found")))
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        _env: &[(&str, &str)],
    ) -> Result<String, String> {
        self.run(program, args)
    }
}

fn host() -> SystemHost<Runner> {
    SystemHost::new(Runner(BTreeMap::new()))
}

fn hex(c: char) -> String {
    c.to_string().repeat(64)
}

fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// The manifests the store holds are the files named `<64 lower case hex>.json`, listed as
/// digests in ascending order; other names are ignored, a store with no manifests directory
/// holds none, and a store that cannot be listed is an error. It catches a listing that
/// reports a junk file as a bundle, an unordered capability, and a fresh store taken for a
/// broken one.
#[test]
fn the_store_lists_the_manifests_it_holds() {
    let dir = Scratch::new("sys-held");
    let store = dir.path("store");
    assert_eq!(host().held(&store), Ok(Vec::new()));
    let manifests = store.join("v1/objects/manifests");
    for name in [
        format!("{}.json", hex('b')),
        format!("{}.json", hex('a')),
        format!("{}.json", hex('A')),
        format!("{}.tmp", hex('c')),
        "junk.json".to_owned(),
        "README".to_owned(),
    ] {
        put(&manifests.join(name), "{}");
    }
    assert_eq!(host().held(&store), Ok(vec![digest('a'), digest('b')]));
    let broken = dir.path("broken");
    put(
        &broken.join("v1/objects/manifests"),
        "a file where a directory should be",
    );
    let why = host().held(&broken).unwrap_err();
    assert!(why.contains("manifests"), "{why}");
}

/// A held bundle's needs are read through its manifest to its config: the macOS floor and the
/// Xcode need of the config. It catches a floor read from the wrong file.
#[test]
fn a_bundles_needs_come_from_its_config() {
    let dir = Scratch::new("sys-needs");
    let store = dir.path("store");
    let objects = store.join("v1/objects");
    put(
        &objects.join(format!("manifests/{}.json", hex('a'))),
        &format!(r#"{{"config": {{"digest": "sha256:{}"}}}}"#, hex('c')),
    );
    put(&objects.join(format!("configs/{}.json", hex('c'))), CONFIG);
    let parsed = BundleConfig::parse(CONFIG.as_bytes()).unwrap();
    let want = Needs {
        os_version: parsed.os_version,
        xcode: parsed.xcode,
    };
    assert_eq!(host().needs(&store, &digest('a')), Ok(want));
}

/// Each way a bundle's record can be wrong is an error that names what: a digest that is not
/// one, a manifest that is missing, not JSON or without a config, a config digest that is not
/// a digest (a path), a config that is missing or does not parse. It catches a path built
/// from text the store did not write, and a bundle run on a config nobody checked.
#[test]
fn a_damaged_record_is_an_error() {
    let dir = Scratch::new("sys-damaged");
    let store = dir.path("store");
    let objects = store.join("v1/objects");
    let manifest =
        |c: char, text: &str| put(&objects.join(format!("manifests/{}.json", hex(c))), text);
    manifest('b', "not json");
    manifest('c', "{}");
    manifest('d', r#"{"config": {"digest": "../../etc/passwd"}}"#);
    manifest(
        'e',
        &format!(r#"{{"config": {{"digest": "sha256:{}"}}}}"#, hex('f')),
    );
    manifest(
        '1',
        &format!(r#"{{"config": {{"digest": "sha256:{}"}}}}"#, hex('2')),
    );
    put(&objects.join(format!("configs/{}.json", hex('2'))), "{}");
    let cases = [
        ("../x".to_owned(), "not a sha256 digest"),
        (digest('9'), "No such file"),
        (digest('b'), "not JSON"),
        (digest('c'), "names no config"),
        (digest('d'), "not a sha256 digest"),
        (digest('e'), "No such file"),
        (digest('1'), ""),
    ];
    for (pin, said) in cases {
        let why = host().needs(&store, &pin).unwrap_err();
        assert!(why.contains(said), "{pin}: {why}");
    }
}

/// `sw_vers` is read as the macOS version, with a release that prints no minor number read
/// as `.0`; text that is no version and a failed run are errors. It catches a version
/// compared as text and a Mac that prints `15`.
#[test]
fn the_macos_version_is_read_from_sw_vers() {
    const LINE: &str = "/usr/bin/sw_vers -productVersion";
    let version =
        |answer: Result<&str, &str>| SystemHost::new(Runner::answering(LINE, answer)).os_version();
    assert_eq!(String::from(version(Ok("14.8.9\n")).unwrap()), "14.8.9");
    assert_eq!(String::from(version(Ok("15\n")).unwrap()), "15.0");
    assert_eq!(
        version(Ok("x\n")),
        Err("sw_vers printed no version".to_owned())
    );
    assert_eq!(version(Err("boom")), Err("boom".to_owned()));
    assert_eq!(
        host().os_version(),
        Err("/usr/bin/sw_vers: not found".to_owned())
    );
}

/// The bytes the store uses are the allocated blocks of its trees and its trash, and nothing
/// else: a store that does not exist uses none, a file in the VM directory is not counted, and
/// a trees directory that cannot be read is an error. It catches a budget that counts the wrong
/// directories or reads a fresh store as broken.
#[test]
fn the_store_use_is_the_blocks_of_trees_and_trash() {
    let dir = Scratch::new("sys-used");
    let store = dir.path("store");
    assert_eq!(host().used_bytes(&store), Ok(0));
    let megabyte = "ab".repeat(1 << 19);
    put(&store.join("v1/objects/trees/t1/file"), &megabyte);
    let trees_only = host().used_bytes(&store).unwrap();
    assert!(trees_only >= 1 << 20, "{trees_only}");
    put(&store.join("v1/trash/t2-x/file"), &megabyte);
    let with_trash = host().used_bytes(&store).unwrap();
    assert!(
        with_trash >= trees_only + (1 << 20),
        "{with_trash} {trees_only}"
    );
    put(&store.join("v1/objects/vm/base/disk.img"), &megabyte);
    assert_eq!(host().used_bytes(&store), Ok(with_trash));
    let broken = dir.path("broken");
    put(
        &broken.join("v1/objects/trees"),
        "a file where a directory should be",
    );
    let why = host().used_bytes(&broken).unwrap_err();
    assert!(why.contains("broken"), "{why}");
}

/// A list file is read as it is; one that is not there is an error naming it. It catches a
/// list read from a cache.
#[test]
fn a_list_file_is_read_when_asked() {
    let dir = Scratch::new("sys-read");
    let path = dir.path("allow");
    assert!(host().read_file(&path).unwrap_err().contains("allow"));
    put(&path, "one\n");
    assert_eq!(host().read_file(&path), Ok("one\n".to_owned()));
    put(&path, "two\n");
    assert_eq!(host().read_file(&path), Ok("two\n".to_owned()));
}

/// The free space of a volume is what `df` says; this build has no Xcode catalog and no
/// fetcher, and says so. It catches a fetch that reports a bundle as on its way when nothing
/// is fetching.
#[test]
fn free_space_comes_from_df_and_there_is_no_fetcher_or_catalog() {
    let line = "/bin/df -P -k /Volumes/work";
    let row =
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3 9 9 2048 1% /\n";
    let host = SystemHost::new(Runner::answering(line, Ok(row)));
    assert_eq!(host.free_bytes(Path::new("/Volumes/work")), Ok(2048 * 1024));
    assert!(host.xcodes().is_empty());
    let fetched = host.fetch(Path::new("/s"), "n", &digest('a'));
    assert_eq!(
        fetched,
        Fetch::Failed("this agent has no bundle fetcher yet".to_owned())
    );
}

/// Off a Mac the store checks cannot run and say so. It catches a Linux host that passes
/// checks it did not make.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_a_mac_the_start_checks_refuse() {
    let why = host()
        .check_start(Path::new("/a"), Path::new("/b"))
        .unwrap_err();
    assert_eq!(why, "the store checks run on macOS only");
}

/// On a Mac the real checks run: a store and a scratch root in one directory pass (a hosted
/// runner's temporary directory is APFS, ownership honoured, securelevel -1), and a store that
/// does not exist is refused. It catches checks that are wired to nothing.
#[cfg(target_os = "macos")]
#[test]
fn on_a_mac_the_start_checks_read_the_real_volume() {
    let dir = Scratch::new("sys-mac");
    let here = dir.path("store");
    fs::create_dir(&here).unwrap();
    assert_eq!(host().check_start(&here, &here), Ok(()));
    assert!(host().check_start(&dir.path("missing"), &here).is_err());
}
