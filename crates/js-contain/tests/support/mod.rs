//! Shared by the integration tests that build layer tars and unpack them.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh, empty directory for one test, under Cargo's temporary directory, removed
/// when it is dropped (a test may leave directories nobody can write: [`force_remove`]
/// makes them writable first).
pub struct Scratch(PathBuf);

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        force_remove(&self.0);
    }
}

/// A scratch directory `<tmp>/js-contain-<name>-<pid>-<n>`.
pub fn scratch(name: &str) -> Scratch {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("js-contain-{name}-{}-{n}", std::process::id()));
    force_remove(&path);
    std::fs::create_dir_all(&path).expect("make scratch");
    Scratch(path)
}

/// Removes `dir`, first giving the owner every permission on every directory in it.
pub fn force_remove(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::symlink_metadata(dir).is_err() {
        return;
    }
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    std::fs::remove_dir_all(dir).expect("remove scratch");
}

/// An entry of a layer tar.
pub enum Item<'a> {
    /// A regular file.
    File {
        path: &'a str,
        data: &'a [u8],
        mode: u32,
    },
    /// A directory.
    Dir { path: &'a str, mode: u32 },
    /// A symlink.
    Symlink { path: &'a str, target: &'a str },
    /// A hard link.
    Hardlink { path: &'a str, target: &'a str },
    /// An entry of a type that is not a file, directory or link.
    Special { path: &'a str, kind: tar::EntryType },
}

/// The owner every entry of a test tar names: the running user's, which an unprivileged
/// unpack may give files to.
pub fn my_ids() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

/// A tar of `items`, owned by `ids`.
pub fn tar_of(items: &[Item<'_>], ids: (u64, u64)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for item in items {
        let mut header = tar::Header::new_gnu();
        header.set_uid(ids.0);
        header.set_gid(ids.1);
        header.set_mtime(1_000_000);
        match item {
            Item::File { path, data, mode } => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(data.len() as u64);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, *data).expect("append");
            }
            Item::Dir { path, mode } => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                header.set_size(0);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
            Item::Symlink { path, target } | Item::Hardlink { path, target } => {
                let kind = if matches!(item, Item::Symlink { .. }) {
                    tar::EntryType::Symlink
                } else {
                    tar::EntryType::Link
                };
                header.set_entry_type(kind);
                header.set_mode(0o777);
                header.set_size(0);
                put_name(&mut header, path);
                if target.len() > 100 {
                    // Too long for the header (an absolute temp path on some hosts): a GNU
                    // long-link entry carries it.
                    builder
                        .append_link(&mut header, path, target)
                        .expect("append long link");
                } else {
                    // An empty name is left as zeros: the builder refuses to write one.
                    if !target.is_empty() {
                        header.set_link_name(target).expect("link name");
                    }
                    header.set_cksum();
                    builder.append(&header, std::io::empty()).expect("append");
                }
            }
            Item::Special { path, kind } => {
                header.set_entry_type(*kind);
                header.set_mode(0o600);
                header.set_size(0);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
        }
    }
    builder.into_inner().expect("finish")
}

/// Writes `path` into the header's name field as given, `..` included (which the
/// builder's own setter refuses): a hostile tar is one this test must be able to make.
fn put_name(header: &mut tar::Header, path: &str) {
    let name = &mut header.as_old_mut().name;
    name.fill(0);
    name[..path.len()].copy_from_slice(path.as_bytes());
}

/// `bytes` gzipped.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).expect("gzip");
    encoder.finish().expect("gzip end")
}
