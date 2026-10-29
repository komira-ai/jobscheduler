//! Helpers for the layer tests: a raw tar writer that can write what no sane writer
//! writes, scratch directories, and a snapshot of a tree.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use js_bundle::{DiffId, Limits, UnpackError, Unpacked, unpack_tar};
use sha2::{Digest, Sha256};

/// A directory of its own with an empty `root` (to extract into) and an empty `outside`
/// beside it (where an escape would land). Removed when dropped.
pub struct Scratch {
    dir: PathBuf,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Scratch {
    pub fn new() -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("js-bundle-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("root")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        Scratch { dir }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    pub fn outside(&self) -> PathBuf {
        self.dir.join("outside")
    }

    pub fn root_fd(&self) -> OwnedFd {
        OwnedFd::from(File::open(self.root()).unwrap())
    }

    /// Nothing was created beside `root`, and nothing in `outside`.
    pub fn assert_contained(&self) {
        let mut beside: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        beside.sort();
        assert_eq!(beside, ["outside", "root"]);
        assert_eq!(std::fs::read_dir(self.outside()).unwrap().count(), 0);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Generous limits for tests that are not about limits.
pub fn limits() -> Limits {
    Limits {
        entries: 1000,
        bytes: 1 << 20,
    }
}

/// An extraction and the scratch directory it ran in.
pub struct Run {
    pub result: Result<Unpacked, UnpackError>,
    pub scratch: Scratch,
}

pub fn extract(tar: &[u8]) -> Run {
    extract_with(tar, &limits())
}

pub fn extract_with(tar: &[u8], limits: &Limits) -> Run {
    let scratch = Scratch::new();
    let fd = scratch.root_fd();
    let result = unpack_tar(tar, fd.as_fd(), limits);
    Run { result, scratch }
}

impl Run {
    pub fn err(&self) -> &UnpackError {
        match &self.result {
            Ok(done) => panic!("expected a refusal, got {done:?}"),
            Err(e) => e,
        }
    }

    pub fn ok(&self) -> &Unpacked {
        match &self.result {
            Ok(done) => done,
            Err(e) => panic!("expected success, got {e}"),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.scratch.root()
    }
}

/// The diff id of `tar`, computed here.
pub fn diff_id_of(tar: &[u8]) -> DiffId {
    DiffId::parse(&format!("sha256:{}", hex::encode(Sha256::digest(tar)))).unwrap()
}

/// One entry to write, with every header field a hostile writer may choose.
pub struct Entry<'a> {
    pub name: &'a [u8],
    pub typeflag: u8,
    pub mode: u32,
    /// What the header says; the data may be shorter or longer.
    pub size: u64,
    pub link: &'a [u8],
    pub data: &'a [u8],
}

impl<'a> Entry<'a> {
    pub fn new(name: &'a [u8], typeflag: u8) -> Entry<'a> {
        Entry {
            name,
            typeflag,
            mode: 0o644,
            size: 0,
            link: b"",
            data: b"",
        }
    }

    pub fn link(mut self, link: &'a [u8]) -> Entry<'a> {
        self.link = link;
        self
    }

    pub fn mode(mut self, mode: u32) -> Entry<'a> {
        self.mode = mode;
        self
    }

    pub fn data(mut self, data: &'a [u8]) -> Entry<'a> {
        self.size = data.len() as u64;
        self.data = data;
        self
    }

    pub fn size(mut self, size: u64) -> Entry<'a> {
        self.size = size;
        self
    }
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(text.as_bytes());
    field[width] = 0;
}

/// A ustar header; names longer than 100 bytes are cut (use a long-name record for them).
pub fn header(e: &Entry<'_>) -> [u8; 512] {
    let mut h = [0u8; 512];
    let name = &e.name[..e.name.len().min(100)];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], u64::from(e.mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], e.size);
    octal(&mut h[136..148], 0);
    h[148..156].fill(b' ');
    h[156] = e.typeflag;
    let link = &e.link[..e.link.len().min(100)];
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// A tar stream under construction.
#[derive(Default)]
pub struct Tar {
    bytes: Vec<u8>,
}

impl Tar {
    pub fn new() -> Tar {
        Tar::default()
    }

    pub fn entry(mut self, e: Entry<'_>) -> Tar {
        self.bytes.extend_from_slice(&header(&e));
        self.bytes.extend_from_slice(e.data);
        let pad = (512 - e.data.len() % 512) % 512;
        self.bytes.resize(self.bytes.len() + pad, 0);
        self
    }

    pub fn dir(self, name: &str) -> Tar {
        self.entry(Entry::new(name.as_bytes(), b'5').mode(0o755))
    }

    pub fn file(self, name: &str, data: &[u8]) -> Tar {
        self.entry(Entry::new(name.as_bytes(), b'0').data(data))
    }

    pub fn exec(self, name: &str, data: &[u8]) -> Tar {
        self.entry(Entry::new(name.as_bytes(), b'0').mode(0o755).data(data))
    }

    pub fn symlink(self, name: &str, target: &str) -> Tar {
        self.entry(Entry::new(name.as_bytes(), b'2').link(target.as_bytes()))
    }

    /// A record that describes the next entry: GNU long name (`b'L'`), GNU long link
    /// (`b'K'`) or PAX (`b'x'`) with `data` as its body.
    pub fn record(self, typeflag: u8, data: &[u8]) -> Tar {
        self.entry(Entry::new(b"././@LongLink", typeflag).data(data))
    }

    /// A GNU long-name record holding `name`.
    pub fn long_name(self, name: &[u8]) -> Tar {
        let mut data = name.to_vec();
        data.push(0);
        self.record(b'L', &data)
    }

    /// A GNU long-link record holding `target`.
    pub fn long_link(self, target: &[u8]) -> Tar {
        let mut data = target.to_vec();
        data.push(0);
        self.record(b'K', &data)
    }

    /// A PAX record for the next entry.
    pub fn pax(self, records: &[(&str, &[u8])]) -> Tar {
        let mut data = Vec::new();
        for (key, value) in records {
            data.extend_from_slice(&pax_record(key, value));
        }
        self.record(b'x', &data)
    }

    /// A file whose name is longer than a header holds: a long-name record, then the
    /// entry with the first 100 bytes.
    pub fn long_file(self, name: &str, data: &[u8]) -> Tar {
        self.long_name(name.as_bytes()).file(name, data)
    }

    /// The stream so far, without end blocks.
    pub fn body(self) -> Vec<u8> {
        self.bytes
    }

    /// The stream with its two end blocks.
    pub fn end(mut self) -> Vec<u8> {
        self.bytes.resize(self.bytes.len() + 1024, 0);
        self.bytes
    }
}

/// `"<len> <key>=<value>\n"`, where `len` counts the whole record.
pub fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let rest = key.len() + value.len() + 3;
    let mut digits = 1;
    while rest + digits != (rest + digits).to_string().len() + rest {
        digits += 1;
    }
    let mut out = format!("{} {key}=", rest + digits).into_bytes();
    out.extend_from_slice(value);
    out.push(b'\n');
    out
}

/// One thing in a tree, for comparing trees.
#[derive(Debug, PartialEq, Eq)]
pub enum Seen {
    Dir,
    File { exec: bool, bytes: Vec<u8> },
    Link(String),
}

/// Every path below `root` and what it is.
pub fn snapshot(root: &Path) -> BTreeMap<String, Seen> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Seen>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            let seen = if meta.is_dir() {
                walk(&entry.path(), &path, out);
                Seen::Dir
            } else if meta.is_symlink() {
                let target = std::fs::read_link(entry.path()).unwrap();
                Seen::Link(target.to_string_lossy().into_owned())
            } else {
                Seen::File {
                    exec: meta.mode() & 0o111 != 0,
                    bytes: std::fs::read(entry.path()).unwrap(),
                }
            };
            out.insert(path, seen);
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

/// The permission bits of `path`, set-id bits included.
pub fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

/// Writes `data` to `root/rel` (making parent directories) with `mode`.
pub fn write(root: &Path, rel: &str, data: &[u8], mode: u32) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A symlink `root/rel` to `target`.
pub fn link(root: &Path, rel: &str, target: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, path).unwrap();
}

/// The path of 136 bytes the fixture tree holds (longer than a header field).
pub fn deep_dir() -> String {
    format!(
        "deep/{}/{}/{}",
        "a".repeat(40),
        "b".repeat(40),
        "c".repeat(40)
    )
}

/// A tree with every kind of entry: executable and plain files, an empty file, an empty
/// directory, symlinks (to a file, to a directory, with a target and a path over 100 bytes)
/// and nesting. `reverse` creates it in the opposite order.
pub fn fixture(root: &Path, reverse: bool) {
    let deep = deep_dir();
    let long_target = format!("../{deep}/file.txt");
    let mut steps: Vec<Box<dyn Fn()>> = Vec::new();
    let r = root.to_path_buf();
    steps.push(Box::new({
        let r = r.clone();
        move || write(&r, "bin/tool", b"#!/bin/sh\necho tool\n", 0o755)
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || link(&r, "bin/alias", "tool")
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || write(&r, "lib/data.txt", b"hello\n", 0o644)
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || write(&r, "lib/empty.bin", b"", 0o600)
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || link(&r, "lib/current", "../bin")
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || std::fs::create_dir_all(r.join("share/empty")).unwrap()
    }));
    steps.push(Box::new({
        let r = r.clone();
        let deep = deep.clone();
        move || write(&r, &format!("{deep}/file.txt"), b"deep\n", 0o644)
    }));
    steps.push(Box::new({
        let r = r.clone();
        move || link(&r, "lib/long-link", &long_target)
    }));
    if reverse {
        steps.reverse();
    }
    for step in steps {
        step();
    }
}
