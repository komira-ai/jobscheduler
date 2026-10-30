//! The packer: a directory into a layer, byte for byte the same whenever the tree is the same.
//!
//! The tar stream is deterministic: entries sorted by path, headers with a fixed mode
//! (directories 0755, files 0644 or 0755 by their execute bit, symlinks 0777), owner 0:0, no
//! user or group name, time 0. Long paths and targets use GNU long-name records whose
//! header holds the first 100 bytes, so the header and the record agree by the extractor's
//! rule. The compressed layer is one zstd frame of a fixed level (see [`compress`]). Nothing
//! read from the directory but names, kinds, execute bits, file bytes and link targets
//! reaches the output.
//!
//! The compressor writes no content size in the frame header (the encoder of the `ruzstd`
//! crate leaves it out and uses a window descriptor), which differs from a frame that states
//! its size; the decoder does not need it. The config's `uncompressed_bytes` is the length of
//! the tar stream before its end blocks, headers and long-name records included, not the
//! bytes of the files and not the size on disk. A long path or target is written as a GNU
//! record whose header holds its first 100 bytes; the extractor accepts a header that is any
//! prefix of an effective path over 100 bytes (the effective path is what it uses). The
//! whole layer is built in memory (every file, the tar, the compressed copy), so the host
//! that packs needs about three times the layer's size in RAM. `.DS_Store` and `__MACOSX`
//! are refused, since they appear when someone opens the folder in Finder.
//!
//! The directory itself is not followed if it is a symlink ([`PackError::RootSymlink`]), and
//! each file is read through one descriptor opened without following a link, so what is
//! checked (kind, set-id bits, link count) is what is read. Directories are listed by path:
//! a writer that swaps a directory for a link while the packer runs is outside what it
//! defends against, as it needs write access to the tree being packed.
//!
//! The packer applies the rules the extractor applies (same path and target checks, same
//! walker), so it refuses what the extractor would: a name outside the rules, a symlink
//! that leaves the tree, and also, because the extractor ignores them, a file with a set-id
//! or sticky bit or with more than one hard link, and anything that is not a directory, a
//! regular file or a symlink.

use std::collections::BTreeMap;
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use sha2::{Digest, Sha256};

use crate::names::DiffId;
use crate::stream::compress;
use crate::tree::{
    DEFAULT_MODEL_BYTES, NameDefect, Node, Tree, TreeRefusal, check_path, check_target, show,
};
use crate::unpack::END_BLOCKS;

/// The set-user-id, set-group-id and sticky bits.
const SET_ID_BITS: u32 = 0o7000;
/// The bytes of a header's name field.
const NAME_FIELD: usize = 100;

/// Why a directory cannot be packed.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// A name, target or entry breaks a rule of the tree.
    #[error(transparent)]
    Tree(#[from] TreeRefusal),
    /// The entry is neither a directory, a regular file nor a symlink.
    #[error("{path:?} is neither a directory, a regular file nor a symlink")]
    Special {
        /// The path.
        path: String,
    },
    /// The file has a set-user-id, set-group-id or sticky bit.
    #[error("{path:?} has the mode {mode:o}, which sets a set-id or sticky bit")]
    SetId {
        /// The path.
        path: String,
        /// Its mode.
        mode: u32,
    },
    /// The file has other names (hard links).
    #[error("{path:?} has {links} hard links; a layer holds none")]
    HardLinked {
        /// The path.
        path: String,
        /// The link count.
        links: u64,
    },
    /// The entry is a Finder leftover (`.DS_Store`, `__MACOSX`), which would make a layer depend
    /// on who last opened the directory.
    #[error("{path:?} is a Finder leftover; remove it before packing")]
    Excluded {
        /// The path.
        path: String,
    },
    /// The directory to pack is itself a symlink; a link is not followed to find a tree.
    #[error("{path:?} is a symlink; name the directory it points to")]
    RootSymlink {
        /// The path.
        path: String,
    },
    /// Reading the directory failed.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// The failure.
        source: io::Error,
    },
    /// Writing the archive failed.
    #[error("writing the archive: {0}")]
    Write(#[from] io::Error),
}

/// The tar stream of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedTar {
    /// The stream, end blocks included.
    pub bytes: Vec<u8>,
    /// The entries in it.
    pub entries: u64,
    /// The digest of the stream: the diff id of the layer.
    pub diff_id: DiffId,
}

impl PackedTar {
    /// The length of the stream before its two end blocks, which is the layer's
    /// `uncompressed_bytes` in the config.
    #[must_use]
    pub fn body_bytes(&self) -> u64 {
        (self.bytes.len() as u64).saturating_sub(END_BLOCKS)
    }
}

/// A layer ready to be pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// The digest of the tar stream: the diff id of the layer.
    pub diff_id: DiffId,
    /// The compressed layer, the blob a registry stores.
    pub blob: Vec<u8>,
    /// The digest of the blob, `sha256:` and 64 hex digits.
    pub blob_digest: String,
    /// The config's `uncompressed_bytes` for the layer.
    pub uncompressed_bytes: u64,
    /// The config's `entries` for the layer.
    pub entries: u64,
}

/// What one entry of the directory is.
enum Item {
    Dir,
    File { data: Vec<u8>, exec: bool },
    Link(String),
}

/// `result` with the path it concerns added to its error.
fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, PackError> {
    result.map_err(|source| PackError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Reads the regular file `source` (shown as `shown` in errors) through one descriptor: it
/// is opened without following a link and without blocking, and what it is, its mode and its
/// link count are those of the descriptor, so a file swapped for a link, a fifo or another
/// file after the directory was listed is not read as the file that was listed.
fn read_file(source: &Path, shown: &str) -> Result<(Vec<u8>, bool), PackError> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = at(
        source,
        rustix::fs::open(source, flags, Mode::empty()).map_err(io::Error::from),
    )?;
    let mut file = std::fs::File::from(fd);
    let meta = at(source, file.metadata())?;
    if !meta.is_file() {
        return Err(PackError::Special {
            path: shown.to_owned(),
        });
    }
    if meta.mode() & SET_ID_BITS != 0 {
        let mode = meta.mode() & 0o7777;
        return Err(PackError::SetId {
            path: shown.to_owned(),
            mode,
        });
    }
    if meta.nlink() > 1 {
        let links = meta.nlink();
        return Err(PackError::HardLinked {
            path: shown.to_owned(),
            links,
        });
    }
    let mut data = Vec::new();
    at(source, file.read_to_end(&mut data))?;
    Ok((data, meta.mode() & 0o111 != 0))
}

/// Lists everything below `dir` (whose path in the layer is `prefix`) into `out`, checking
/// each path and target. The map is the sort order of the archive: by path, bytewise.
fn collect(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Item>) -> Result<(), PackError> {
    for listed in at(dir, std::fs::read_dir(dir))? {
        let listed = at(dir, listed)?;
        let name = listed.file_name();
        let joined = if prefix.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [prefix.as_bytes(), b"/", name.as_bytes()].concat()
        };
        let shown = show(&joined);
        let path = check_path(&joined).map_err(|defect| TreeRefusal::Path {
            path: shown.clone(),
            defect,
        })?;
        if matches!(name.to_str(), Some(".DS_Store" | "__MACOSX")) {
            return Err(PackError::Excluded { path: shown });
        }
        let source = listed.path();
        let meta = at(&source, std::fs::symlink_metadata(&source))?;
        let kind = meta.file_type();
        let item = if kind.is_dir() {
            collect(&source, path, out)?;
            Item::Dir
        } else if kind.is_file() {
            let (data, exec) = read_file(&source, &shown)?;
            Item::File { data, exec }
        } else if kind.is_symlink() {
            let target = at(&source, std::fs::read_link(&source))?;
            let checked = check_target(target.as_os_str().as_bytes());
            let target = checked.map_err(|defect: NameDefect| TreeRefusal::Target {
                path: shown.clone(),
                defect,
            })?;
            Item::Link(target.to_owned())
        } else {
            return Err(PackError::Special { path: shown });
        };
        out.insert(path.to_owned(), item);
    }
    Ok(())
}

/// Copies the first bytes of `text` into a header field of fixed width.
fn fill(field: &mut [u8], text: &str) {
    let count = text.len().min(field.len());
    field[..count].copy_from_slice(&text.as_bytes()[..count]);
}

/// A header with the fixed metadata of every entry.
fn plain_header() -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header
}

/// Writes the GNU long-name record (`b'L'` for a path, `b'K'` for a link target) for
/// `text` if it does not fit a header field.
fn long_name(builder: &mut tar::Builder<Vec<u8>>, kind: u8, text: &str) -> io::Result<()> {
    if text.len() <= NAME_FIELD {
        return Ok(());
    }
    let mut header = plain_header();
    fill(&mut header.as_old_mut().name, "././@LongLink");
    header.set_mode(0o644);
    header.set_size(text.len() as u64 + 1);
    header.set_entry_type(tar::EntryType::new(kind));
    header.set_cksum();
    let mut data = text.as_bytes().to_vec();
    data.push(0);
    builder.append(&header, &data[..])
}

/// Writes one entry.
fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, item: &Item) -> Result<(), PackError> {
    let (kind, mode, data, target) = match item {
        Item::Dir => (tar::EntryType::Directory, 0o755, &[][..], ""),
        Item::File { data, exec } => {
            let mode = [0o644, 0o755][usize::from(*exec)];
            (tar::EntryType::Regular, mode, data.as_slice(), "")
        }
        Item::Link(target) => (tar::EntryType::Symlink, 0o777, &[][..], target.as_str()),
    };
    let mut header = plain_header();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_size(data.len() as u64);
    long_name(builder, b'L', path)?;
    fill(&mut header.as_old_mut().name, path);
    long_name(builder, b'K', target)?;
    fill(&mut header.as_old_mut().linkname, target);
    header.set_cksum();
    builder.append(&header, data)?;
    Ok(())
}

/// Packs the directory `root` into a tar stream.
///
/// # Errors
/// The directory cannot be read, holds something a layer does not (see the module
/// documentation), or breaks a rule of the tree.
pub fn pack_tar(root: &Path) -> Result<PackedTar, PackError> {
    if at(root, std::fs::symlink_metadata(root))?.is_symlink() {
        return Err(PackError::RootSymlink {
            path: show(root.as_os_str().as_bytes()),
        });
    }
    let mut items = BTreeMap::new();
    collect(root, "", &mut items)?;
    let mut tree = Tree::new(DEFAULT_MODEL_BYTES);
    for (path, item) in &items {
        let node = match item {
            Item::Dir => Node::Dir,
            Item::File { .. } => Node::File,
            Item::Link(target) => Node::Link(target.clone()),
        };
        tree.add(path, node)?;
    }
    tree.check_links()?;
    let mut builder = tar::Builder::new(Vec::new());
    for (path, item) in &items {
        append(&mut builder, path, item)?;
    }
    let bytes = builder.into_inner()?;
    Ok(PackedTar {
        diff_id: DiffId::from_sha256(Sha256::digest(&bytes)),
        entries: items.len() as u64,
        bytes,
    })
}

/// Packs the directory `root` into a compressed layer.
///
/// # Errors
/// As [`pack_tar`].
pub fn pack_layer(root: &Path) -> Result<Packed, PackError> {
    let tar = pack_tar(root)?;
    let blob = compress(&tar.bytes);
    Ok(Packed {
        blob_digest: format!("sha256:{}", hex::encode(Sha256::digest(&blob))),
        uncompressed_bytes: tar.body_bytes(),
        entries: tar.entries,
        diff_id: tar.diff_id,
        blob,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, removed by the test.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("js-bundle-pack-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The case the listing cannot show: a file that was a regular file when the directory
    /// was listed and is a symlink when it is opened is not followed.
    #[test]
    fn a_file_swapped_for_a_symlink_is_not_followed() {
        let dir = scratch("swapped-link");
        std::fs::write(dir.join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink("secret", dir.join("f")).unwrap();
        let error = read_file(&dir.join("f"), "f").unwrap_err();
        assert!(matches!(error, PackError::Io { .. }), "{error:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file swapped for a fifo is refused by what the descriptor is, not read.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_swapped_for_a_fifo_is_refused() {
        let dir = scratch("swapped-fifo");
        let fifo = dir.join("p");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let error = read_file(&fifo, "p").unwrap_err();
        assert!(matches!(error, PackError::Special { path } if path == "p"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
