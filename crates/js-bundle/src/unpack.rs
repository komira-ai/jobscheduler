//! The extractor: a layer's tar stream onto a directory descriptor.
//!
//! The input is hostile. The `tar` crate's reader parses the headers and its `unpack` is
//! never called: every create is `mkdirat`, `openat` with `O_CREAT | O_EXCL | O_NOFOLLOW`
//! or `symlinkat`, relative to a descriptor, and each parent is opened one component at a
//! time with `O_NOFOLLOW | O_DIRECTORY`, so the kernel never resolves a path from the
//! root. Creation is thereby safe whatever the archive holds; resolution by later tools is
//! protected by the walker, which resolves every symlink target the way the kernel does,
//! against the model of the whole tree, once the stream has ended.
//!
//! What is admitted: directories, regular files and symlinks, whose paths and targets pass
//! [`check_path`](crate::check_path) and [`check_target`](crate::check_target), whose parent is an
//! earlier directory entry, and whose number and bytes are within the [`Limits`]. Modes are
//! rebuilt from the header's execute bit alone (directories 0755, files 0644 or 0755);
//! owners, times and every other mode bit are ignored, so a setuid bit never reaches disk.
//! Nothing else is written: no extended attribute, no device, fifo, socket or hard link.
//!
//! Two more bounds keep a hostile layer from costing more than its ceilings say: the model of
//! the tree holds at most [`Limits::model_bytes`] of paths and targets, and the symlink walker
//! has a budget of resolution steps for the whole tree (a fixed 65536 and 64 per symlink; a
//! link of the packer's takes a handful, so its output never nears it). Both refuse with an
//! error of their own.
//!
//! The stream is counted, hashed and capped as it is read, whatever follows the end
//! marker, so [`Unpacked::diff_id`] is the digest of every byte and a stream over its
//! ceiling stops there.

use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{Mode, OFlags, fchmod, mkdirat, openat, symlinkat};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::media::LayerEncoding;
use crate::names::DiffId;
use crate::stream::{Decoder, StreamError};
use crate::tree::{Node, Tree, TreeRefusal, check_path, check_target, show};

/// The size of a tar block.
const BLOCK: u64 = 512;
/// The two zero blocks that end a tar stream.
pub const END_BLOCKS: u64 = 2 * BLOCK;
/// The longest name or link name a header holds without a long-name record.
const HEADER_NAME_BYTES: usize = 100;
/// The most bytes a long-name or PAX record may hold (the tar reader would otherwise read
/// as many bytes as the header declares into memory).
const MAX_RECORD_BYTES: u64 = 8192;
/// The PAX keys an entry may carry; anything else (an extended attribute, an ACL, a
/// sparse map, a `size` that would override the header's) is refused.
const ALLOWED_KEYS: [&[u8]; 7] = [
    b"path",
    b"linkpath",
    b"mtime",
    b"uid",
    b"gid",
    b"uname",
    b"gname",
];

const DIR_MODE: Mode = Mode::RWXU
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);
const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);
const EXEC_MODE: Mode = FILE_MODE
    .union(Mode::XUSR)
    .union(Mode::XGRP)
    .union(Mode::XOTH);
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The ceilings a layer is extracted under, from its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most entries (directories, files and symlinks).
    pub entries: u64,
    /// The most bytes of the tar stream before its end blocks, which is also the most bytes
    /// of regular files. The whole raw stream, every byte after the end marker included,
    /// may not pass this plus the two end blocks.
    pub bytes: u64,
    /// The most bytes the model of the tree may hold: every path twice and every symlink
    /// target once. A hostile layer cannot grow the extractor's memory past this, whatever
    /// the entry limit. 256 MiB (`DEFAULT_MODEL_BYTES`) is a thousand times the model of a
    /// layer of the first bundles.
    pub model_bytes: u64,
}

/// What an extraction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The entries created.
    pub entries: u64,
    /// The bytes of the regular files written.
    pub file_bytes: u64,
    /// The bytes of the raw stream, trailing bytes included.
    pub stream_bytes: u64,
    /// The digest of the raw stream, which names the tree.
    pub diff_id: DiffId,
}

/// Why a layer is refused.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    /// A path, target or entry breaks a rule of the tree.
    #[error(transparent)]
    Tree(#[from] TreeRefusal),
    /// The stream is truncated, over its ceiling or not a layer's zstd frame.
    #[error(transparent)]
    Stream(#[from] StreamError),
    /// The entry is of a type a layer does not hold.
    #[error("entry {path:?} has the type {kind:?}, which a layer does not hold")]
    EntryType {
        /// The path.
        path: String,
        /// The type flag byte as a character.
        kind: char,
    },
    /// The entry carries a PAX record a layer does not allow (an extended attribute, an ACL, a
    /// sparse map), or the same key twice.
    #[error("entry {path:?} carries the extension record {key:?}")]
    Extension {
        /// The path.
        path: String,
        /// The record's key.
        key: String,
    },
    /// A long-name or PAX record is too large, comes twice for one entry or describes no
    /// entry.
    #[error("a long-name or PAX record is refused: {why}")]
    Record {
        /// What is wrong with it.
        why: &'static str,
    },
    /// The header and a long-name or PAX record give different paths.
    #[error("entry {path:?}: the header and its long-name record disagree")]
    Disagree {
        /// The path (the record's).
        path: String,
    },
    /// A directory or symlink with a size.
    #[error("entry {path:?} is not a file but has a size")]
    SizeOnNonFile {
        /// The path.
        path: String,
    },
    /// More entries than the limit.
    #[error("the layer has more than {max} entries")]
    TooManyEntries {
        /// The limit.
        max: u64,
    },
    /// More file bytes than the limit.
    #[error("the layer's files hold more than {max} bytes")]
    TooManyBytes {
        /// The limit.
        max: u64,
    },
    /// The first end block is not followed by a second.
    #[error("the first end block of the archive is not followed by a second")]
    EndMarker,
    /// The stream hashes to another diff id than the config's.
    #[error("the stream's diff id is {got}, the config's is {expected}")]
    DiffId {
        /// The config's.
        expected: String,
        /// The stream's.
        got: String,
    },
    /// The tar reader refused the stream.
    #[error("tar: {0}")]
    Tar(String),
    /// A file-system call failed.
    #[error("creating {path:?}: {source}")]
    Create {
        /// The path.
        path: String,
        /// The failure.
        source: io::Error,
    },
}

/// An error on its way up: a refusal, a read failure to be explained by the meter, or a
/// create failure that may be a read failure surfacing through the copy.
enum Failure {
    Refused(UnpackError),
    Io(io::Error),
    Create { path: String, source: io::Error },
}

impl From<UnpackError> for Failure {
    fn from(error: UnpackError) -> Failure {
        Failure::Refused(error)
    }
}

impl From<TreeRefusal> for Failure {
    fn from(error: TreeRefusal) -> Failure {
        Failure::Refused(UnpackError::Tree(error))
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Failure {
        Failure::Io(error)
    }
}

impl Failure {
    /// The error to report, given what the meter saw of the stream.
    fn explain(self, meter: &Meter<'_>) -> UnpackError {
        match self {
            Failure::Refused(error) => error,
            Failure::Io(error) => meter
                .stream_fault()
                .unwrap_or_else(|| UnpackError::Tar(error.to_string())),
            Failure::Create { path, source } => meter
                .stream_fault()
                .unwrap_or(UnpackError::Create { path, source }),
        }
    }
}

/// The raw stream as the tar reader sees it: counted, hashed and capped.
struct Meter<'a> {
    inner: &'a mut dyn Read,
    hasher: Sha256,
    total: u64,
    ceiling: u64,
    ended: bool,
    fault: Option<StreamError>,
}

impl<'a> Meter<'a> {
    fn new(inner: &'a mut dyn Read, ceiling: u64) -> Meter<'a> {
        Meter {
            inner,
            hasher: Sha256::new(),
            total: 0,
            ceiling,
            ended: false,
            fault: None,
        }
    }

    /// The stream's own failure, if it had one: the one the decoder or the ceiling
    /// raised, or the end of the input in the middle of a read.
    fn stream_fault(&self) -> Option<UnpackError> {
        let ended = self.ended.then_some(StreamError::Truncated);
        self.fault.clone().or(ended).map(UnpackError::Stream)
    }
}

impl Read for Meter<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = match self.inner.read(buf) {
            Ok(count) => count,
            Err(error) => {
                let raised = error
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<StreamError>());
                self.fault = raised.cloned().or(self.fault.take());
                return Err(error);
            }
        };
        self.ended |= count == 0;
        self.total += count as u64;
        if self.total > self.ceiling {
            let fault = StreamError::Ceiling { max: self.ceiling };
            self.fault = Some(fault.clone());
            return Err(io::Error::other(fault));
        }
        self.hasher.update(&buf[..count]);
        Ok(count)
    }
}

/// What a walk over the archive counted.
struct Progress {
    entries: u64,
    file_bytes: u64,
}

/// What to create for an admitted entry.
#[derive(Clone, Copy)]
enum Plan<'t> {
    Dir,
    File { exec: bool },
    Link(&'t str),
}

/// The state of an extraction.
struct State<'a> {
    root: BorrowedFd<'a>,
    limits: &'a Limits,
    tree: Tree,
    entries: u64,
    file_bytes: u64,
}

/// Whether the `effective` path (or link name) of an entry and the one in its header
/// agree: equal, or the header holds the first bytes of one too long for it.
fn agrees(effective: &[u8], header: &[u8]) -> bool {
    let same = effective == header;
    let cut = (effective.len() > HEADER_NAME_BYTES) & effective.starts_with(header);
    same | cut
}

/// The refusal of a PAX record.
fn extension(shown: &str, key: &[u8]) -> Failure {
    Failure::Refused(UnpackError::Extension {
        path: shown.to_owned(),
        key: show(key),
    })
}

/// The long-name, long-link and PAX records that precede an entry.
#[derive(Default)]
struct Records {
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    pax: Option<Vec<u8>>,
}

impl Records {
    /// Whether any record is waiting for its entry.
    fn waiting(&self) -> bool {
        [&self.long_name, &self.long_link, &self.pax]
            .iter()
            .any(|slot| slot.is_some())
    }
}

/// `bytes` without the NUL that ends a GNU long name.
fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

/// The effective path and link name of an entry, from its `records` and its header, once the
/// records are checked: only the allowed PAX keys, each once, and a path (or link name) a
/// record gives is the same as every other source gives, up to the cut a header makes.
fn names(
    records: Records,
    header_path: &[u8],
    header_link: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), Failure> {
    let shown = show(header_path);
    let mut pax_path = None;
    let mut pax_link = None;
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let data = records.pax.unwrap_or_default();
    for record in tar::PaxExtensions::new(&data) {
        let record = record?;
        let key = record.key_bytes();
        let fresh = !seen.iter().any(|earlier| earlier == key);
        if !(ALLOWED_KEYS.contains(&key) & fresh) {
            return Err(extension(&shown, key));
        }
        seen.push(key.to_vec());
        match key {
            b"path" => pax_path = Some(record.value_bytes().to_vec()),
            b"linkpath" => pax_link = Some(record.value_bytes().to_vec()),
            _ => {}
        }
    }
    let path = records
        .long_name
        .map(trim_nul)
        .or_else(|| pax_path.clone())
        .unwrap_or_else(|| header_path.to_vec());
    let link = records
        .long_link
        .map(trim_nul)
        .or_else(|| pax_link.clone())
        .unwrap_or_else(|| header_link.to_vec());
    let records_agree =
        pax_path.is_none_or(|given| given == path) & pax_link.is_none_or(|given| given == link);
    let in_step = records_agree & agrees(&path, header_path) & agrees(&link, header_link);
    if !in_step {
        let path = show(&path);
        return Err(Failure::Refused(UnpackError::Disagree { path }));
    }
    Ok((path, link))
}

/// Reads the body of a long-name, long-link or PAX record into `slot`.
fn read_record<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    slot: &mut Option<Vec<u8>>,
) -> Result<(), Failure> {
    if entry.size() > MAX_RECORD_BYTES {
        let why = "it is over 8192 bytes";
        return Err(Failure::Refused(UnpackError::Record { why }));
    }
    if slot.is_some() {
        let why = "two of one kind describe one entry";
        return Err(Failure::Refused(UnpackError::Record { why }));
    }
    let mut data = Vec::new();
    entry.read_to_end(&mut data)?;
    *slot = Some(data);
    Ok(())
}

impl State<'_> {
    /// Takes one physical entry of the archive: a record that describes the next entry is
    /// kept, anything else is admitted.
    ///
    /// Returns the stream offset where the entry's data (padded to a block) ends.
    fn accept<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        records: &mut Records,
    ) -> Result<u64, Failure> {
        match entry.header().entry_type() {
            EntryType::GNULongName => read_record(entry, &mut records.long_name)?,
            EntryType::GNULongLink => read_record(entry, &mut records.long_link)?,
            EntryType::XHeader => read_record(entry, &mut records.pax)?,
            _ => self.admit(entry, std::mem::take(records))?,
        }
        let padded = entry.size().div_ceil(BLOCK).saturating_mul(BLOCK);
        Ok(entry.raw_file_position().saturating_add(padded))
    }

    /// Admits one entry: checks it, adds it to the model, creates it.
    fn admit<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        records: Records,
    ) -> Result<(), Failure> {
        let kind = entry.header().entry_type();
        let is_link = kind == EntryType::Symlink;
        let header_path = entry.header().path_bytes().into_owned();
        // Only a symlink's link name is read, so only its header's is compared.
        let header_link = if is_link {
            let name = entry.header().link_name_bytes();
            name.map(std::borrow::Cow::into_owned).unwrap_or_default()
        } else {
            Vec::new()
        };
        let (path_bytes, link_bytes) = names(records, &header_path, &header_link)?;
        let shown = show(&path_bytes);
        if !matches!(
            kind,
            EntryType::Regular | EntryType::Directory | EntryType::Symlink
        ) {
            return Err(UnpackError::EntryType {
                path: shown,
                kind: char::from(kind.as_byte()),
            }
            .into());
        }
        let text = check_path(&path_bytes).map_err(|defect| TreeRefusal::Path {
            path: shown.clone(),
            defect,
        })?;
        self.entries += 1;
        if self.entries > self.limits.entries {
            let max = self.limits.entries;
            return Err(UnpackError::TooManyEntries { max }.into());
        }
        let size = entry.size();
        let exec = entry.header().mode()? & 0o111 != 0;
        let (node, plan) = match kind {
            EntryType::Regular => {
                self.file_bytes = self.file_bytes.saturating_add(size);
                if self.file_bytes > self.limits.bytes {
                    let max = self.limits.bytes;
                    return Err(UnpackError::TooManyBytes { max }.into());
                }
                (Node::File, Plan::File { exec })
            }
            EntryType::Directory => (Node::Dir, Plan::Dir),
            _ => {
                let target = check_target(&link_bytes).map_err(|defect| TreeRefusal::Target {
                    path: shown.clone(),
                    defect,
                })?;
                (Node::Link(target.to_owned()), Plan::Link(target))
            }
        };
        if size != 0 && !matches!(node, Node::File) {
            return Err(UnpackError::SizeOnNonFile { path: shown }.into());
        }
        self.tree.add(text, node)?;
        self.create(text, plan, entry, &shown)?;
        Ok(())
    }

    /// Creates the entry at `path` below the root, its data read from `entry`.
    fn create<R: Read>(
        &self,
        path: &str,
        plan: Plan<'_>,
        entry: &mut tar::Entry<'_, R>,
        shown: &str,
    ) -> Result<(), Failure> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        with_dir(self.root, parent, |dir| match plan {
            Plan::Dir => make_dir(dir, name),
            Plan::File { exec } => write_file(dir, name, exec, entry),
            Plan::Link(target) => Ok(symlinkat(target, dir, name)?),
        })
        .map_err(|source| Failure::Create {
            path: shown.to_owned(),
            source,
        })
    }
}

/// Opens `parent` (a relative path of directories made by this extraction, `""` for the
/// root) one component at a time without following a symlink, and calls `then` with it.
fn with_dir<T>(
    root: BorrowedFd<'_>,
    parent: &str,
    then: impl FnOnce(BorrowedFd<'_>) -> io::Result<T>,
) -> io::Result<T> {
    let mut held: Option<OwnedFd> = None;
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        let at = held.as_ref().map_or(root, AsFd::as_fd);
        let next = openat(at, part, DIR_FLAGS, Mode::empty())?;
        held = Some(next);
    }
    then(held.as_ref().map_or(root, AsFd::as_fd))
}

/// Makes the directory `name` in `dir` with the mode 0755, whatever the umask.
fn make_dir(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    mkdirat(dir, name, DIR_MODE)?;
    let made = openat(dir, name, DIR_FLAGS, Mode::empty())?;
    fchmod(&made, DIR_MODE)?;
    Ok(())
}

/// Writes the file `name` in `dir`, which must not exist, with the bytes of `entry`.
fn write_file<R: Read>(
    dir: BorrowedFd<'_>,
    name: &str,
    exec: bool,
    entry: &mut tar::Entry<'_, R>,
) -> io::Result<()> {
    let fd = openat(dir, name, CREATE_FLAGS, Mode::RUSR | Mode::WUSR)?;
    fchmod(&fd, [FILE_MODE, EXEC_MODE][usize::from(exec)])?;
    let mut file = std::fs::File::from(fd);
    io::copy(entry, &mut file)?;
    Ok(())
}

/// Reads the archive from `meter` to the end of the stream.
fn walk(meter: &mut Meter<'_>, root: BorrowedFd<'_>, limits: &Limits) -> Result<Progress, Failure> {
    let mut state = State {
        root,
        limits,
        tree: Tree::new(limits.model_bytes),
        entries: 0,
        file_bytes: 0,
    };
    let mut last_end = 0;
    {
        let mut archive = tar::Archive::new(&mut *meter);
        let mut records = Records::default();
        for entry in archive.entries()?.raw(true) {
            let mut entry = entry?;
            last_end = state.accept(&mut entry, &mut records)?;
        }
        if records.waiting() {
            let why = "it describes no entry";
            return Err(UnpackError::Record { why }.into());
        }
    }
    // The tar reader stops at the first zero block, or at the end of the input. Only the
    // first may be followed by the second; whatever follows both is read to the end.
    if meter.total != last_end + BLOCK {
        return Err(UnpackError::Stream(StreamError::Truncated).into());
    }
    let mut second = [0u8; BLOCK as usize];
    meter.read_exact(&mut second)?;
    if second.iter().any(|byte| *byte != 0) {
        return Err(UnpackError::EndMarker.into());
    }
    io::copy(meter, &mut io::sink())?;
    state.tree.check_links()?;
    Ok(Progress {
        entries: state.entries,
        file_bytes: state.file_bytes,
    })
}

/// `unpack_tar` over a trait object.
fn unpack_dyn(
    reader: &mut dyn Read,
    root: BorrowedFd<'_>,
    limits: &Limits,
) -> Result<Unpacked, UnpackError> {
    let mut meter = Meter::new(reader, limits.bytes.saturating_add(END_BLOCKS));
    let walked = walk(&mut meter, root, limits);
    let progress = walked.map_err(|failure| failure.explain(&meter))?;
    Ok(Unpacked {
        entries: progress.entries,
        file_bytes: progress.file_bytes,
        stream_bytes: meter.total,
        diff_id: DiffId::from_sha256(meter.hasher.finalize()),
    })
}

/// Extracts the tar stream `tar` into the directory `root`, which must be empty.
///
/// # Errors
/// The stream breaks a rule of the module documentation, is over its ceiling or ends early,
/// or a file-system call fails. The directory may hold part of the tree then; the caller
/// discards it.
pub fn unpack_tar(
    tar: impl Read,
    root: BorrowedFd<'_>,
    limits: &Limits,
) -> Result<Unpacked, UnpackError> {
    let mut tar = tar;
    unpack_dyn(&mut tar, root, limits)
}

/// Extracts a layer, stored as `encoding`, into the directory `root`, and checks that its
/// raw stream is the one `diff_id` names.
///
/// # Errors
/// As [`unpack_tar`], and the compressed stream breaks a limit of [`Decoder`], or the
/// stream's digest is another one.
pub fn unpack_layer(
    layer: impl Read,
    encoding: LayerEncoding,
    root: BorrowedFd<'_>,
    diff_id: &DiffId,
    limits: &Limits,
) -> Result<Unpacked, UnpackError> {
    let mut layer = layer;
    let unpacked = match encoding {
        LayerEncoding::Tar => unpack_dyn(&mut layer, root, limits)?,
        LayerEncoding::TarZstd => {
            let mut decoder = Decoder::new(&mut layer)?;
            unpack_dyn(&mut decoder, root, limits)?
        }
    };
    if unpacked.diff_id != *diff_id {
        return Err(UnpackError::DiffId {
            expected: diff_id.as_str().to_owned(),
            got: unpacked.diff_id.as_str().to_owned(),
        });
    }
    Ok(unpacked)
}
