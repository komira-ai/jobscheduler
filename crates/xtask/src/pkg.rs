//! `pkg check-deb --deb <file> --version <X.Y.Z> --binary <file> --packaging <dir>`: refuses a
//! js-agent deb unless it is exactly what `packaging/js-agent` says it is.
//!
//! The deb is read back from the bytes nfpm wrote (an `ar` archive of `control.tar.gz` and
//! `data.tar.gz`), not from the configuration that made it. It must hold:
//!
//! - exactly the files of [`FILES`], each with its mode and owner root:root, and no other
//!   file or link; its directories are root's and mode 0755;
//! - at `/usr/bin/js-agent`, the bytes of `--binary`; as the unit and the tmpfiles file, the
//!   bytes of `js-agent.service` and `tmpfiles.conf` in `--packaging`;
//! - a control file naming the package `js-agent`, `--version` and architecture `amd64`, with
//!   no `Depends`, `Pre-Depends`, `Breaks` or `Conflicts` (a host installs it with a plain
//!   `dpkg --install`, which resolves nothing), and no maintainer script.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The files of the package: path, mode, and the file in `--packaging` (or the binary) whose
/// bytes it must hold.
pub const FILES: [(&str, u32, Source); 4] = [
    ("usr/bin/js-agent", 0o755, Source::Binary),
    (
        "usr/lib/systemd/system/js-agent.service",
        0o644,
        Source::Packaging("js-agent.service"),
    ),
    (
        "usr/lib/tmpfiles.d/js-agent.conf",
        0o644,
        Source::Packaging("tmpfiles.conf"),
    ),
    ("usr/share/doc/js-agent/copyright", 0o644, Source::Any),
];

/// The control fields that make a package need something of the host.
const FORBIDDEN_FIELDS: [&str; 4] = ["Depends:", "Pre-Depends:", "Breaks:", "Conflicts:"];

/// Where the bytes of a package file must come from.
#[derive(Clone, Copy, Debug)]
pub enum Source {
    /// The `--binary` file.
    Binary,
    /// A file of `--packaging`.
    Packaging(&'static str),
    /// Any bytes.
    Any,
}

/// The arguments of `pkg check-deb`.
#[derive(clap::Args)]
pub struct CheckArgs {
    /// The deb to check.
    #[arg(long)]
    pub deb: PathBuf,
    /// The version the control file must name.
    #[arg(long)]
    pub version: String,
    /// The js-agent binary the package must hold.
    #[arg(long)]
    pub binary: PathBuf,
    /// The directory (`packaging/js-agent`) with the unit and the tmpfiles file.
    #[arg(long)]
    pub packaging: PathBuf,
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

/// Reads the deb `bytes`.
///
/// # Errors
/// [`Error::Malformed`] when it is not an `ar` archive with a gzip `control.tar.gz` holding
/// `control` and a gzip `data.tar.gz`.
pub fn parse_deb(bytes: &[u8]) -> Result<Deb, Error> {
    let members = ar_members(bytes)?;
    let find = |name: &str| {
        members
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, body)| *body)
            .ok_or_else(|| Error::Malformed(format!("no {name} member")))
    };
    let control_entries = tar_entries(find("control.tar.gz")?)?;
    let control = control_entries
        .iter()
        .find(|(path, _, _)| path == "control")
        .map(|(_, _, body)| String::from_utf8_lossy(body).into_owned())
        .ok_or_else(|| malformed("no control file"))?;
    let mut data = BTreeMap::new();
    for (path, header, body) in tar_entries(find("data.tar.gz")?)? {
        let kind = header.entry_type();
        let member = Member {
            mode: header.mode().map_err(|e| Error::Malformed(e.to_string()))?,
            uid: header.uid().map_err(|e| Error::Malformed(e.to_string()))?,
            gid: header.gid().map_err(|e| Error::Malformed(e.to_string()))?,
            dir: kind.is_dir(),
            file: kind.is_file(),
            sha256: hex::encode(Sha256::digest(&body)),
        };
        data.insert(path, member);
    }
    Ok(Deb {
        control,
        control_names: control_entries.into_iter().map(|(p, _, _)| p).collect(),
        members: data,
    })
}

/// What is wrong with `deb` as the package at `version`, whose sources have the SHA-256s
/// `sources` (hex, by [`Source`]): one line per problem.
pub fn problems(deb: &Deb, version: &str, sources: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    for want in [
        "Package: js-agent".to_owned(),
        format!("Version: {version}"),
        "Architecture: amd64".to_owned(),
    ] {
        if !deb.control.lines().any(|line| line == want) {
            problems.push(format!("the control file lacks the line {want:?}"));
        }
    }
    for line in deb.control.lines() {
        if FORBIDDEN_FIELDS.iter().any(|field| line.starts_with(field)) {
            problems.push(format!("the control file has the line {line:?}"));
        }
    }
    for name in &deb.control_names {
        if matches!(name.as_str(), "preinst" | "postinst" | "prerm" | "postrm") {
            problems.push(format!("the package has the maintainer script {name}"));
        }
    }
    for (path, member) in &deb.members {
        let allowed = FILES.iter().any(|(p, _, _)| p == path);
        if member.uid != 0 || member.gid != 0 {
            problems.push(format!("{path}: owned by {}:{}", member.uid, member.gid));
        }
        if member.dir && member.mode != 0o755 {
            problems.push(format!("{path}: a directory of mode {:o}", member.mode));
        }
        if !member.dir && !allowed {
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
        let expected = match source {
            Source::Binary => sources.get("binary"),
            Source::Packaging(name) => sources.get(name),
            Source::Any => None,
        };
        if expected.is_some_and(|sha| *sha != member.sha256) {
            problems.push(format!("{path}: not the bytes of {source:?}"));
        }
    }
    problems
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::Read(format!("{}: {e}", path.display())))
}

/// Checks the deb the arguments name: the line to print when it is the package.
pub fn check(args: &CheckArgs) -> Result<String, Error> {
    let bytes = read(&args.deb)?;
    let deb = parse_deb(&bytes)?;
    let mut sources = BTreeMap::new();
    sources.insert(
        "binary".to_owned(),
        hex::encode(Sha256::digest(read(&args.binary)?)),
    );
    for (_, _, source) in FILES {
        if let Source::Packaging(name) = source {
            let sha = hex::encode(Sha256::digest(read(&args.packaging.join(name))?));
            sources.insert(name.to_owned(), sha);
        }
    }
    let found = problems(&deb, &args.version, &sources);
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
