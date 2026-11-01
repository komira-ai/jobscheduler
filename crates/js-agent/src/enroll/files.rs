//! The two files `enroll` makes: what an existing one must be, and how a new one is written.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::OFlags;
use sha2::{Digest, Sha256};

use super::Failure;

/// The most an existing file may hold. An instance line and a token are far shorter.
const LIMIT: u64 = 4096;

/// Which file.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// `/etc/js-agent/instance`: not secret.
    Instance,
    /// `/etc/js-agent/token`: a secret.
    Token,
}

impl Kind {
    /// The mode a new file of this kind gets.
    fn mode(self) -> u32 {
        match self {
            Self::Instance => 0o644,
            Self::Token => 0o600,
        }
    }

    /// Whether an existing file may have `mode`: the instance file must not be writable by
    /// group or others, the token file must be what the agent itself accepts of its token
    /// file (0600 or 0400).
    fn mode_ok(self, mode: u32) -> bool {
        match self {
            Self::Instance => mode & 0o022 == 0,
            Self::Token => [0o600, 0o400].contains(&mode),
        }
    }

    /// The rule of [`Kind::mode_ok`] in words.
    fn mode_rule(self) -> &'static str {
        match self {
            Self::Instance => "it must not be writable by group or others",
            Self::Token => "it must be 0600 or 0400",
        }
    }

    /// Whether `existing` is what `wanted` asks for. The instance line must match exactly;
    /// a token matches by sha256, so it does not matter whether a line break follows it.
    fn same(self, existing: &[u8], wanted: &[u8]) -> bool {
        match self {
            Self::Instance => existing == wanted,
            Self::Token => digest(existing) == digest(wanted),
        }
    }

    /// The refusal for an existing file that is not what `wanted` asks for. It names the
    /// file and what to do, and holds a digest of a token and never the token.
    fn differs(self, path: &Path, existing: &[u8], wanted: &[u8]) -> String {
        match self {
            Self::Instance => format!(
                "{}: holds {:?}, not {:?}; remove it by hand and run again",
                path.display(),
                String::from_utf8_lossy(existing)
                    .chars()
                    .take(80)
                    .collect::<String>(),
                String::from_utf8_lossy(wanted),
            ),
            Self::Token => format!(
                "{}: present but differs from the token on standard input (the file's sha256 is {}, \
                 the input's is {}); remove it by hand and run again",
                path.display(),
                digest(existing),
                digest(wanted),
            ),
        }
    }
}

/// The sha256 of `bytes` without trailing white space, as 64 lower-case hex digits.
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes.trim_ascii_end()))
}

/// Looks at the file at `path`. `Ok(false)` when nothing is there, `Ok(true)` when a file
/// that `kind` accepts holds what `wanted` asks for, and a refusal for anything else: a link
/// (never followed), a file that is not a regular file, is not root's, has a mode the kind
/// does not allow, is long, cannot be read, or holds something else. Nothing is changed.
pub(super) fn inspect(path: &Path, kind: Kind, wanted: &[u8]) -> Result<bool, Failure> {
    let found = read_plain(path, kind)
        .map_err(|why| Failure::Usage(format!("{}: {why}", path.display())))?;
    match found {
        None => Ok(false),
        Some(bytes) => match kind.same(&bytes, wanted) {
            true => Ok(true),
            false => Err(Failure::Usage(kind.differs(path, &bytes, wanted))),
        },
    }
}

/// The content of the file at `path`; `None` when nothing is there. The file is opened
/// without following a link and without blocking (a named pipe is not waited on), and
/// checked on the open file.
fn read_plain(path: &Path, kind: Kind) -> io::Result<Option<Vec<u8>>> {
    let flags = (OFlags::NONBLOCK | OFlags::NOFOLLOW).bits().cast_signed();
    let opened = OpenOptions::new().read(true).custom_flags(flags).open(path);
    match opened {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(why),
        Ok(file) => plain(file, kind).map(Some),
    }
}

/// The content of `file`, if it is a regular file of root's that `kind` allows.
fn plain(file: File, kind: Kind) -> io::Result<Vec<u8>> {
    let meta = file.metadata()?;
    let mode = meta.mode() & 0o7777;
    let problems = [
        (!meta.is_file(), "is not a regular file".to_owned()),
        (
            meta.uid() != rustix::process::geteuid().as_raw(),
            "is not owned by root".to_owned(),
        ),
        (
            !kind.mode_ok(mode),
            format!("has mode {mode:04o}; {}", kind.mode_rule()),
        ),
        (meta.len() > LIMIT, format!("is longer than {LIMIT} bytes")),
    ];
    if let Some((_, why)) = problems.into_iter().find(|(bad, _)| *bad) {
        return Err(io::Error::other(why));
    }
    let mut bytes = Vec::new();
    Read::take(file, LIMIT).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Makes the file at `path` with `wanted` in it, and looks at it again. Nothing is replaced:
/// a file that appeared since [`inspect`] is an error.
pub(super) fn write_new(path: &Path, kind: Kind, wanted: &[u8]) -> Result<(), Failure> {
    js_client::create_new(path, wanted, kind.mode())
        .map_err(|why| Failure::Failed(format!("cannot write {}: {why}", path.display())))?;
    inspect(path, kind, wanted).map(drop)
}
