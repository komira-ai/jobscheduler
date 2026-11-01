//! `pkg check-deb --deb <file> --version <X.Y.Z> --binary <file> --packaging <dir> --license <file>`:
//! refuses a js-agent deb unless it is exactly what `packaging/js-agent` says it is.
//!
//! The deb is read back from the bytes nfpm wrote: an `ar` archive of exactly `debian-binary`,
//! `control.tar.gz` and `data.tar.gz`, in that order. Every other layout is refused, among them
//! a deb compressed with xz or zstd (`nfpm.yaml` sets `compression: gzip`) and one with a second
//! data archive that dpkg and this checker could disagree about. The deb must hold:
//!
//! - exactly the files of [`FILES`], each with its mode and owner root:root (by id, and by name
//!   when the entry carries one: dpkg looks the name up first), and no other file or link;
//!   its directories are the parents of those files, root's and mode 0755; no path twice;
//! - at `/usr/bin/js-agent`, the bytes of `--binary`, which must be a statically linked
//!   x86-64 Linux ELF executable (no interpreter, no needed library); as the unit and the
//!   tmpfiles file, the bytes of `js-agent.service` and `tmpfiles.conf` in `--packaging`; as the
//!   host setup script, the bytes of `crun_host_setup.sh` in `--packaging`; as the copyright
//!   file, the bytes of `--license`;
//! - a control file with exactly one `Package: js-agent`, `Version:` of `--version` and
//!   `Architecture: amd64`, and no `Depends`, `Pre-Depends`, `Breaks`, `Conflicts`, `Replaces`,
//!   `Provides` or `Essential` field in any letter case (a host installs it with a plain
//!   `dpkg --install`, which resolves nothing); `Recommends` and `Suggests` do not fail an
//!   install and are allowed;
//! - a control archive of `control` and `md5sums` only: no maintainer script, trigger,
//!   conffile or other member.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::release::Version;

/// The files of the package: path, mode, and where the bytes it must hold come from.
pub const FILES: [(&str, u32, Source); 5] = [
    ("usr/bin/js-agent", 0o755, Source::Binary),
    (
        "usr/lib/systemd/system/js-agent.service",
        0o644,
        Source::Service,
    ),
    ("usr/lib/tmpfiles.d/js-agent.conf", 0o644, Source::Tmpfiles),
    (
        "usr/share/js-agent/crun_host_setup.sh",
        0o755,
        Source::HostSetup,
    ),
    ("usr/share/doc/js-agent/copyright", 0o644, Source::License),
];

/// The control fields that make a package need, replace or provide something of the host.
const FORBIDDEN_FIELDS: [&str; 7] = [
    "Depends",
    "Pre-Depends",
    "Breaks",
    "Conflicts",
    "Replaces",
    "Provides",
    "Essential",
];

/// The members of the control archive.
const CONTROL_ARCHIVE: [&str; 3] = ["", "control", "md5sums"];

/// The members of the deb, in order.
const MEMBERS: [&str; 3] = ["debian-binary", "control.tar.gz", "data.tar.gz"];

/// Where the bytes of a package file must come from.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// The `--binary` file.
    Binary,
    /// `js-agent.service` in `--packaging`.
    Service,
    /// `tmpfiles.conf` in `--packaging`.
    Tmpfiles,
    /// `crun_host_setup.sh` in `--packaging`.
    HostSetup,
    /// The `--license` file.
    License,
}

/// The SHA-256 of each source, in hex.
pub struct Digests {
    /// Of the `--binary` file.
    pub binary: String,
    /// Of `js-agent.service`.
    pub service: String,
    /// Of `tmpfiles.conf`.
    pub tmpfiles: String,
    /// Of `crun_host_setup.sh`.
    pub host_setup: String,
    /// Of the `--license` file.
    pub license: String,
}

impl Digests {
    fn of(&self, source: Source) -> &str {
        match source {
            Source::Binary => &self.binary,
            Source::Service => &self.service,
            Source::Tmpfiles => &self.tmpfiles,
            Source::HostSetup => &self.host_setup,
            Source::License => &self.license,
        }
    }
}

/// The arguments of `pkg check-deb`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// The deb to check.
    #[arg(long)]
    pub deb: PathBuf,
    /// The version the control file must name, `MAJOR.MINOR.PATCH`.
    #[arg(long)]
    pub version: Version,
    /// The js-agent binary the package must hold.
    #[arg(long)]
    pub binary: PathBuf,
    /// The directory (`packaging/js-agent`) with the unit, the tmpfiles file and the host setup script.
    #[arg(long)]
    pub packaging: PathBuf,
    /// The license file the package must hold as its copyright file.
    #[arg(long)]
    pub license: PathBuf,
}

/// Why a deb was not checked, or was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A file could not be read.
    #[error("{0}")]
    Read(String),
    /// The deb is not a deb.
    #[error("not a deb: {0}")]
    Malformed(String),
    /// The deb is a deb that is not the package.
    #[error("the deb is not the js-agent package: {}", .0.join("; "))]
    Refused(Vec<String>),
}

/// One entry of the data archive.
pub struct Member {
    /// The mode.
    pub mode: u32,
    /// The owner's id.
    pub uid: u64,
    /// The group's id.
    pub gid: u64,
    /// The owner's name, empty when the entry has none.
    pub owner: String,
    /// The group's name, empty when the entry has none.
    pub group: String,
    /// Whether it is a directory.
    pub dir: bool,
    /// Whether it is a regular file (neither this nor a directory is a link or a device).
    pub file: bool,
    /// The SHA-256 of its bytes, in hex.
    pub sha256: String,
}

/// A deb as read back.
pub struct Deb {
    /// The `control` file.
    pub control: String,
    /// The names in the control archive.
    pub control_names: Vec<String>,
    /// The data archive by path (no leading `./`, no trailing `/`).
    pub members: BTreeMap<String, Member>,
}

fn malformed(why: &str) -> Error {
    Error::Malformed(why.to_owned())
}

/// The members of an `ar` archive: name and bytes.
fn ar_members(bytes: &[u8]) -> Result<Vec<(String, &[u8])>, Error> {
    let mut rest = bytes
        .strip_prefix(b"!<arch>\n")
        .ok_or_else(|| malformed("no ar magic"))?;
    let mut members = Vec::new();
    while !rest.is_empty() {
        let header = rest
            .get(..60)
            .ok_or_else(|| malformed("a short ar header"))?;
        let text = |from: usize, to: usize| String::from_utf8_lossy(&header[from..to]).into_owned();
        let name = text(0, 16).trim().trim_end_matches('/').to_owned();
        let size: usize = text(48, 58)
            .trim()
            .parse()
            .map_err(|_| malformed("an ar size that is not a number"))?;
        let body = rest
            .get(60..60 + size)
            .ok_or_else(|| malformed("an ar member cut short"))?;
        members.push((name, body));
        // Members start on even offsets.
        rest = rest.get(60 + size + size % 2..).unwrap_or(&[]);
    }
    Ok(members)
}

/// The entries of the gzip tar `gz`: path, header and bytes.
fn tar_entries(gz: &[u8]) -> Result<Vec<(String, tar::Header, Vec<u8>)>, Error> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_end(&mut raw)
        .map_err(|e| Error::Malformed(format!("a member that is not gzip: {e}")))?;
    let bad = |e: std::io::Error| Error::Malformed(format!("a member that is not a tar: {e}"));
    let mut entries = Vec::new();
    // Only an archive already read from has no entries to list.
    let mut archive = tar::Archive::new(&raw[..]);
    for entry in archive.entries().expect("a fresh archive") {
        let mut entry = entry.map_err(bad)?;
        let path = String::from_utf8_lossy(&entry.path_bytes())
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        let mut body = Vec::new();
        entry.read_to_end(&mut body).map_err(bad)?;
        entries.push((path, entry.header().clone(), body));
    }
    Ok(entries)
}

/// The refusal of a deb whose `ar` members are not [`MEMBERS`].
fn wrong_members(names: &[&str]) -> Error {
    Error::Malformed(format!(
        "the ar members are {names:?}, not {MEMBERS:?} in that order (only gzip archives are read)"
    ))
}

/// Reads the deb `bytes`.
///
/// # Errors
/// [`Error::Malformed`] when it is not an `ar` archive of exactly `debian-binary` (`2.0`), a
/// gzip `control.tar.gz` holding `control`, and a gzip `data.tar.gz`, neither archive listing a
/// path twice.
pub fn parse_deb(bytes: &[u8]) -> Result<Deb, Error> {
    let members = ar_members(bytes)?;
    let names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
    let [(_, version), (_, control_gz), (_, data_gz)] = members.as_slice() else {
        return Err(wrong_members(&names));
    };
    if names != MEMBERS {
        return Err(wrong_members(&names));
    }
    if *version != b"2.0\n".as_slice() {
        return Err(malformed("debian-binary is not 2.0"));
    }
    let control_entries = tar_entries(control_gz)?;
    let mut seen = std::collections::BTreeSet::new();
    for (path, _, _) in &control_entries {
        if !seen.insert(path.as_str()) {
            return Err(Error::Malformed(format!(
                "{path} is in the control archive twice"
            )));
        }
    }
    let control = control_entries
        .iter()
        .find(|(path, _, _)| path == "control")
        .map(|(_, _, body)| String::from_utf8_lossy(body).into_owned())
        .ok_or_else(|| malformed("no control file"))?;
    let name =
        |bytes: Option<&[u8]>| String::from_utf8_lossy(bytes.unwrap_or_default()).into_owned();
    let mut data = BTreeMap::new();
    for (path, header, body) in tar_entries(data_gz)? {
        let kind = header.entry_type();
        let member = Member {
            mode: header.mode().map_err(|e| Error::Malformed(e.to_string()))?,
            uid: header.uid().map_err(|e| Error::Malformed(e.to_string()))?,
            gid: header.gid().map_err(|e| Error::Malformed(e.to_string()))?,
            owner: name(header.username_bytes()),
            group: name(header.groupname_bytes()),
            dir: kind.is_dir(),
            file: kind.is_file(),
            sha256: hex::encode(Sha256::digest(&body)),
        };
        if data.insert(path.clone(), member).is_some() {
            return Err(Error::Malformed(format!(
                "{path} is in the data archive twice"
            )));
        }
    }
    Ok(Deb {
        control,
        control_names: control_entries.into_iter().map(|(p, _, _)| p).collect(),
        members: data,
    })
}

/// The `name: value` fields of a control file, in order (a continuation line, which starts
/// with white space, belongs to the field before it).
fn control_fields(control: &str) -> Vec<(&str, &str)> {
    control
        .lines()
        .filter(|line| !line.starts_with([' ', '\t']))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect()
}

/// Whether tar's owner or group name is root's (an entry without a name is, and dpkg then
/// takes the id).
fn is_root(name: &str) -> bool {
    name.is_empty() || name == "root"
}

/// Whether `path` is a directory a file of the package lies in (or the root, `./`).
fn is_package_dir(path: &str) -> bool {
    path.is_empty()
        || FILES.iter().any(|(file, _, _)| {
            file.strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// What is wrong with `deb` as the package at `version`, whose sources have the SHA-256s
/// `digests`: one line per problem.
pub fn problems(deb: &Deb, version: &str, digests: &Digests) -> Vec<String> {
    let mut problems = Vec::new();
    let fields = control_fields(&deb.control);
    for (name, want) in [
        ("Package", "js-agent"),
        ("Version", version),
        ("Architecture", "amd64"),
    ] {
        let found: Vec<&str> = fields
            .iter()
            .filter(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
            .collect();
        if found != [want] {
            problems.push(format!(
                "the control file lacks exactly one line {name}: {want} (it has {found:?})"
            ));
        }
    }
    for (name, value) in &fields {
        if FORBIDDEN_FIELDS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            problems.push(format!("the control file has the field {name}: {value}"));
        }
    }
    for name in &deb.control_names {
        if !CONTROL_ARCHIVE.contains(&name.as_str()) {
            problems.push(format!(
                "{name}: in the control archive, which holds only control and md5sums (a \
                 maintainer script, trigger or conffile is not allowed)"
            ));
        }
    }
    for (path, member) in &deb.members {
        if member.uid != 0 || member.gid != 0 {
            problems.push(format!("{path}: owned by {}:{}", member.uid, member.gid));
        }
        if !is_root(&member.owner) || !is_root(&member.group) {
            problems.push(format!(
                "{path}: owned by the names {:?}:{:?}, not root:root",
                member.owner, member.group
            ));
        }
        if member.dir {
            if !is_package_dir(path) {
                problems.push(format!(
                    "{path}: a directory no file of the package lies in"
                ));
            }
            if member.mode != 0o755 {
                problems.push(format!("{path}: a directory of mode {:o}", member.mode));
            }
        } else if !FILES.iter().any(|(file, _, _)| file == path) {
            problems.push(format!("{path}: not a file of the package"));
        }
    }
    for (path, mode, source) in FILES {
        let Some(member) = deb.members.get(path) else {
            problems.push(format!("{path}: missing"));
            continue;
        };
        if !member.file {
            problems.push(format!("{path}: not a regular file"));
        }
        if member.mode != mode {
            problems.push(format!("{path}: mode {:o}, not {mode:o}", member.mode));
        }
        if member.sha256 != digests.of(source) {
            problems.push(format!("{path}: not the bytes of {source:?}"));
        }
    }
    problems
}

/// `PT_DYNAMIC`: the program header of the dynamic section.
const PT_DYNAMIC: usize = 2;
/// `PT_INTERP`: the program header that names a dynamic loader.
const PT_INTERP: usize = 3;
/// `DT_NULL`: the end of the dynamic section.
const DT_NULL: usize = 0;
/// `DT_NEEDED`: a shared library the executable needs.
const DT_NEEDED: usize = 1;
/// `EM_X86_64`: the machine of an `amd64` package.
const EM_X86_64: usize = 62;

/// The little-endian number of `len` bytes at `at`, `None` when the bytes end first.
fn word(bytes: &[u8], at: usize, len: usize) -> Option<usize> {
    let raw = bytes.get(at..at.checked_add(len)?)?;
    let sum = raw
        .iter()
        .rev()
        .fold(0u64, |sum, byte| (sum << 8) | u64::from(*byte));
    usize::try_from(sum).ok()
}

/// Why `bytes` is not a statically linked x86-64 Linux executable, `None` when it is one:
/// a 64-bit little-endian ELF file for x86-64 with no program interpreter (`PT_INTERP`) and
/// no `DT_NEEDED` entry in its dynamic section. A static-pie executable has a dynamic section
/// (for its relocations) and passes, a glibc one needs `libc.so.6` and a loader and does not.
pub fn elf_problem(bytes: &[u8]) -> Option<String> {
    elf_checked(bytes).err()
}

fn elf_checked(bytes: &[u8]) -> Result<(), String> {
    const CUT: &str = "the ELF headers are cut short";
    if bytes.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err("not an ELF file".to_owned());
    }
    // Class 2 is 64-bit, and data 1 little-endian.
    if bytes.get(4..6) != Some([2u8, 1].as_slice()) {
        return Err("not a 64-bit little-endian ELF file".to_owned());
    }
    if word(bytes, 18, 2) != Some(EM_X86_64) {
        return Err("not an x86-64 ELF file".to_owned());
    }
    let table = word(bytes, 32, 8).ok_or(CUT)?;
    let size = word(bytes, 54, 2).ok_or(CUT)?;
    let count = word(bytes, 56, 2).ok_or(CUT)?;
    for index in 0..count {
        let at = index
            .checked_mul(size)
            .and_then(|from| table.checked_add(from))
            .ok_or(CUT)?;
        let entry = bytes.get(at..).ok_or(CUT)?;
        match word(entry, 0, 4).ok_or(CUT)? {
            PT_INTERP => {
                return Err("it has a program interpreter: it is dynamically linked".to_owned());
            }
            PT_DYNAMIC => {
                let from = word(entry, 8, 8).ok_or(CUT)?;
                let len = word(entry, 32, 8).ok_or(CUT)?;
                let end = from.checked_add(len).ok_or(CUT)?;
                for tag in bytes.get(from..end).ok_or(CUT)?.chunks_exact(16) {
                    match word(tag, 0, 8).ok_or(CUT)? {
                        DT_NULL => break,
                        DT_NEEDED => {
                            return Err(
                                "it needs a shared library: it is dynamically linked".to_owned()
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Read(format!("{}: {e}", path.display())))
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Checks the deb the arguments name: the line to print when it is the package.
pub fn check(args: &CheckArgs) -> Result<String, Error> {
    let bytes = read(&args.deb)?;
    let deb = parse_deb(&bytes)?;
    let binary = read(&args.binary)?;
    let digests = Digests {
        binary: sha(&binary),
        service: sha(&read(&args.packaging.join("js-agent.service"))?),
        tmpfiles: sha(&read(&args.packaging.join("tmpfiles.conf"))?),
        host_setup: sha(&read(&args.packaging.join("crun_host_setup.sh"))?),
        license: sha(&read(&args.license)?),
    };
    let mut found = problems(&deb, &args.version.to_string(), &digests);
    found.extend(
        elf_problem(&binary).map(|why| format!("--binary {}: {why}", args.binary.display())),
    );
    if !found.is_empty() {
        return Err(Error::Refused(found));
    }
    Ok(format!(
        "{}: js-agent {} with {} files",
        args.deb.display(),
        args.version,
        FILES.len()
    ))
}
