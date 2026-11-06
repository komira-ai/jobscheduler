//! What the tests of the hostkeeper packages share: a stand-in binary, scratch directories, the
//! package of each platform, hostkeeper's unpacker and manifest loader on it, and readers of
//! its bytes (the zstd frame, the tar entries) and of the texts inside it (the manifest as
//! TOML, the unit's assignments, the plist's values).

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hostkeeper_config::daemon::Sha256Hex;
use hostkeeper_config::manifest::{Expected, HostkeeperVersion, Manifest, ManifestRefusal};
use hostkeeper_config::names::{ServiceName, Version as HostVersion};
use hostkeeper_config::platform::Platform as Hk;
use hostkeeper_unpack::{HostFs, Limits, Owner, UnpackError, UnpackOrder, Unpacked};
use js_testkit::dir::TestDir;
use sha2::{Digest, Sha256};
use xtask::release::Version;
use xtask::release::hostkeeper_package::Platform;
use xtask::release::hostkeeper_package::archive;

/// The version every package of the tests is built as: what `js-agent --version` prints after
/// the program's name.
pub const VERSION: &str = "1.2.3";

/// The `min_hostkeeper` of the packages the tests unpack. hostkeeper's unpacker compares it with
/// the version of the hostkeeper crates it was built from, which is the workspace's `0.1.0`
/// here (a release of hostkeeper stamps its tag into the daemon), so a package that names the
/// shipped default `0.1.92` is read with `Manifest::load` and a daemon version of the test's
/// choosing instead (`verbs.rs`).
pub const TEST_MIN: &str = "0.1.0";

/// The three platforms.
pub const ALL: [Platform; 3] = Platform::ALL;

/// The two Linux platforms.
pub const LINUX: [Platform; 2] = [Platform::LinuxX86_64, Platform::LinuxArm64];

/// Where hostkeeper keeps every package on a host.
pub const PACKAGES: &str = "/var/lib/hostkeeper/packages";

/// The program every unit runs: the package's binary through hostkeeper's `current` link.
pub const CURRENT_BINARY: &str = "/var/lib/hostkeeper/packages/js-agent/current/bin/js-agent";

/// The join token's path, which the operator places and no package does.
pub const TOKEN: &str = "/etc/js-agent/token";

/// The host's flags file, which the operator places.
pub const FLAGS: &str = "/etc/js-agent/flags";

/// The flags file of a sample host: the flags a package cannot carry.
pub const SAMPLE_FLAGS: &str = "# this host's own flags\n--server=https://js.example.com:8443\n--pool=farm\n--instance-file=/etc/js-agent/instance\n--host-id=node-5\n";

/// The params the update hook reads from `[params]` of the manifest, sorted as the manifest's
/// table is: the agent's own list (`js_agent::update_hook::params::KEYS`).
pub fn hook_params() -> Vec<&'static str> {
    let mut keys = js_agent::update_hook::params::KEYS.to_vec();
    keys.sort_unstable();
    keys
}

/// The release version as the verb's type.
pub fn version() -> Version {
    VERSION.parse().expect("a version")
}

/// A stand-in binary: bytes that hold the agent's version text as a build holds it
/// (`<VERSION>\nruntimes: docker crun`), and the magic of a macOS code signature, so a test
/// sees the bytes go through untouched.
pub fn binary() -> Vec<u8> {
    let mut bytes = b"\xcf\xfa\xed\xfe stand-in js-agent\0".to_vec();
    bytes.extend_from_slice(VERSION.as_bytes());
    bytes.extend_from_slice(
        b"\nruntimes: docker crun\0read-only data\0\xfa\xde\x0c\xc0 signature blob\0",
    );
    bytes
}

/// The package of [`binary`] for `platform`.
pub fn package(platform: Platform) -> Vec<u8> {
    archive(platform, version(), TEST_MIN, &binary()).expect("the package")
}

/// A scratch directory of one test. hostkeeper leaves an unpacked version read-only (0555), so
/// the drop opens every directory below it up first, or it could not be removed.
pub struct Scratch(TestDir);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        Scratch(TestDir::under(env!("CARGO_TARGET_TMPDIR"), name))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// Gives `path` and every directory below it mode 0700.
fn open_up(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            open_up(&entry.path());
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        open_up(self.path());
    }
}

/// The order of js-agent [`VERSION`] on `platform`.
pub fn expected(platform: Platform) -> Expected {
    Expected {
        name: ServiceName::new("js-agent").expect("a service name"),
        version: HostVersion::new(VERSION).expect("a version"),
        platform: hk(platform),
    }
}

/// hostkeeper's `Platform` for `platform`, by the spelling.
pub fn hk(platform: Platform) -> Hk {
    platform.as_str().parse().expect("a hostkeeper platform")
}

/// The hostkeeper that reads the packages: the minimum they name.
pub fn running() -> HostkeeperVersion {
    HostkeeperVersion::parse(TEST_MIN).expect("a version")
}

/// Unpacks `archive` with hostkeeper's unpacker, as the daemon does for an order of js-agent
/// [`VERSION`] on `platform`, into `<dir>/packages`, owned by the test's own user (the daemon's
/// is root). The unpack is the one of a Linux host, which makes no macOS check of the tree.
pub fn unpack(dir: &Path, archive: &[u8], platform: Platform) -> Result<Unpacked, UnpackError> {
    let file = dir.join("package.tar.zst");
    std::fs::create_dir_all(dir).expect("the scratch directory");
    std::fs::write(&file, archive).expect("write the package");
    let packages = dir.join("packages");
    std::fs::create_dir_all(&packages).expect("the packages directory");
    let meta = std::fs::metadata(dir).expect("the scratch directory");
    let owner = Owner {
        uid: meta.uid(),
        gid: meta.gid(),
    };
    let fs = HostFs::open(&packages, owner).expect("open the packages directory");
    let order = UnpackOrder {
        service: ServiceName::new("js-agent").expect("a service name"),
        version: HostVersion::new(VERSION).expect("a version"),
        platform: hk(platform),
        package_digest: Sha256Hex::parse(&hex::encode(Sha256::digest(archive))).expect("a digest"),
    };
    let limits = Limits {
        max_unpacked_bytes: 1 << 30,
        max_entries: 1000,
    };
    hostkeeper_unpack::unpack(
        &file,
        &order,
        &limits,
        &fs,
        hostkeeper_unpack::Platform::Linux,
    )
}

/// The fields of a zstd frame header (RFC 8878, section 3.1.1.1).
#[derive(Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub single_segment: bool,
    pub reserved: bool,
    pub checksum: bool,
    pub dictionary: bool,
    /// The window the frame declares, in bytes.
    pub window: u64,
}

/// The header of the zstd frame `archive` starts with.
pub fn frame_header(archive: &[u8]) -> FrameHeader {
    assert_eq!(archive[..4], [0x28, 0xb5, 0x2f, 0xfd], "the zstd magic");
    let descriptor = archive[4];
    let single_segment = descriptor & 0x20 != 0;
    assert!(
        !single_segment,
        "a single-segment frame has no window descriptor"
    );
    let window_descriptor = archive[5];
    let base = 1u64 << (10 + u64::from(window_descriptor >> 3));
    FrameHeader {
        single_segment,
        reserved: descriptor & 0x08 != 0,
        checksum: descriptor & 0x04 != 0,
        dictionary: descriptor & 0x03 != 0,
        window: base + base / 8 * u64::from(window_descriptor & 0x07),
    }
}

/// The tar `archive` holds, decoded from its first zstd frame, and the number of bytes after
/// that frame.
pub fn decode(archive: &[u8]) -> (Vec<u8>, usize) {
    let mut rest = archive;
    let mut tar = Vec::new();
    {
        let mut decoder =
            ruzstd::decoding::StreamingDecoder::new(&mut rest).expect("a zstd frame header");
        decoder.read_to_end(&mut tar).expect("the frame decodes");
    }
    (tar, rest.len())
}

/// One entry of the tar in a package, as its header says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub kind: tar::EntryType,
    pub ustar: bool,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub owner_names: (Vec<u8>, Vec<u8>),
    pub content: Vec<u8>,
}

/// The entries of the package `archive`, in order.
pub fn listing(archive: &[u8]) -> Vec<Listed> {
    let (tar, _) = decode(archive);
    let mut reader = tar::Archive::new(&tar[..]);
    let mut listed = Vec::new();
    for entry in reader.entries().expect("a tar") {
        let mut entry = entry.expect("an entry");
        let header = entry.header().clone();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("the content");
        listed.push(Listed {
            name: String::from_utf8(entry.path_bytes().into_owned()).expect("UTF-8"),
            kind: header.entry_type(),
            ustar: header.as_ustar().is_some(),
            mode: header.mode().expect("a mode"),
            uid: header.uid().expect("a uid"),
            gid: header.gid().expect("a gid"),
            mtime: header.mtime().expect("a time"),
            owner_names: (
                header.username_bytes().unwrap_or_default().to_vec(),
                header.groupname_bytes().unwrap_or_default().to_vec(),
            ),
            content,
        });
    }
    listed
}

/// The content of the file `name` of the package `archive`.
pub fn file(archive: &[u8], name: &str) -> Vec<u8> {
    listing(archive)
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("no {name} in the package"))
        .content
}

/// The content of the file `name` of the package `archive`, as text.
pub fn text(archive: &[u8], name: &str) -> String {
    String::from_utf8(file(archive, name)).expect("UTF-8")
}

/// The manifest of the package `archive`, as TOML.
pub fn manifest_toml(archive: &[u8]) -> toml::Table {
    text(archive, "manifest.toml")
        .parse()
        .expect("the manifest is TOML")
}

/// Writes the files of the package `archive` below `dir` (the directories they are in made),
/// each through `edit` (name, bytes) first.
pub fn write_tree(dir: &Path, archive: &[u8], edit: impl Fn(&str, Vec<u8>) -> Vec<u8>) {
    for entry in listing(archive) {
        let path = dir.join(&entry.name);
        if entry.kind == tar::EntryType::Directory {
            std::fs::create_dir_all(&path).expect("a directory");
            continue;
        }
        std::fs::write(&path, edit(&entry.name, entry.content)).expect("a file");
    }
}

/// The manifest of `platform`'s package, through `edit` first, loaded by hostkeeper's loader
/// from a version directory below `dir` that holds the package's files.
pub fn load_edited(
    dir: &Path,
    platform: Platform,
    edit: impl Fn(String) -> String,
) -> Result<Manifest, ManifestRefusal> {
    let version = dir.join("version");
    write_tree(&version, &package(platform), |name, bytes| match name {
        "manifest.toml" => edit(String::from_utf8(bytes).expect("UTF-8")).into_bytes(),
        _ => bytes,
    });
    Manifest::load(&version, &expected(platform), &running())
}

/// `<root>/var/lib/hostkeeper/packages/js-agent/<VERSION>`: where a host keeps the version,
/// below the scratch directory `root` that stands for `/`.
pub fn version_dir(root: &Path) -> PathBuf {
    root.join(PACKAGES.trim_start_matches('/'))
        .join("js-agent")
        .join(VERSION)
}

/// The `(section, key, value)` of each assignment of a systemd unit, its continuation lines
/// joined and its comments left out.
pub fn assignments(unit: &str) -> Vec<(String, String, String)> {
    let mut joined = Vec::new();
    let mut open = String::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => {
                open.push_str(head);
                open.push(' ');
            }
            None => {
                open.push_str(line);
                joined.push(std::mem::take(&mut open));
            }
        }
    }
    let mut section = String::new();
    let mut out = Vec::new();
    for line in joined {
        if line.starts_with('[') {
            section = line;
        } else if let Some((key, value)) = line.split_once('=') {
            out.push((section.clone(), key.to_owned(), value.to_owned()));
        }
    }
    out
}

/// The value of the one assignment of `key` in `section` of `unit`.
pub fn unit_value(unit: &str, section: &str, key: &str) -> String {
    let found: Vec<String> = assignments(unit)
        .into_iter()
        .filter(|(s, k, _)| (s == section) & (k == key))
        .map(|(_, _, value)| value)
        .collect();
    assert_eq!(found.len(), 1, "{section} {key}= in {unit}");
    found[0].clone()
}

/// A value of a launchd plist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    String(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

/// The tags and texts of the plist `text` after its prolog: `(tag, text after it)`.
fn tokens(text: &str) -> Vec<(String, String)> {
    let body = &text[text.find("<plist").expect("a plist")..];
    body.split('<')
        .skip(1)
        .map(|part| {
            let (tag, rest) = part.split_once('>').expect("a closed tag");
            (tag.to_owned(), rest.trim_matches(['\n', '\t']).to_owned())
        })
        .collect()
}

/// The value that starts at `at` in `tokens`, and the index after it.
fn value(tokens: &[(String, String)], at: usize) -> (Value, usize) {
    let (tag, text) = &tokens[at];
    match tag.as_str() {
        "string" => (Value::String(text.clone()), at + 2),
        "integer" => (Value::Integer(text.parse().expect("an integer")), at + 2),
        "true/" => (Value::Bool(true), at + 1),
        "false/" => (Value::Bool(false), at + 1),
        "array" => {
            let (mut items, mut next) = (Vec::new(), at + 1);
            while tokens[next].0 != "/array" {
                let (item, after) = value(tokens, next);
                items.push(item);
                next = after;
            }
            (Value::Array(items), next + 1)
        }
        "dict" => {
            let (mut items, mut next) = (BTreeMap::new(), at + 1);
            while tokens[next].0 != "/dict" {
                assert_eq!(tokens[next].0, "key", "a key, then its value");
                let key = tokens[next].1.clone();
                let (item, after) = value(tokens, next + 2);
                items.insert(key, item);
                next = after;
            }
            (Value::Dict(items), next + 1)
        }
        other => panic!("a plist element this test does not read: {other}"),
    }
}

/// The top dictionary of the launchd plist `text`.
pub fn plist(text: &str) -> BTreeMap<String, Value> {
    let tokens = tokens(text);
    match value(&tokens, 1).0 {
        Value::Dict(map) => map,
        other => panic!("the plist's top value is {other:?}"),
    }
}

/// The text of the string `value`.
pub fn string(value: &Value) -> &str {
    match value {
        Value::String(text) => text,
        other => panic!("not a string: {other:?}"),
    }
}

/// The strings of the array `value`.
pub fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::Array(items) => items.iter().map(string).collect(),
        other => panic!("not an array: {other:?}"),
    }
}

/// `words` with the pair `--flags-file PATH` replaced by the words of `flags_text`, as the
/// agent replaces it before it reads its flags.
pub fn expand(words: &[&str], flags_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = words.iter();
    while let Some(word) = rest.next() {
        if *word == js_agent::flags_file::FLAG {
            let _path = rest.next().expect("a path after --flags-file");
            out.extend(js_agent::flags_file::words(flags_text).expect("the flags file's words"));
        } else {
            out.push((*word).to_owned());
        }
    }
    out
}
