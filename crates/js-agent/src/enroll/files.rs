//! The two files `enroll` makes: what an existing one must be, and how a new one is written.

use std::fs::{self, File, OpenOptions};
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
/// that `kind` accepts holds what `wanted` asks for, and the words of a refusal for anything
/// else: a link (never followed), a file that is not a regular file, is not `owner`'s, has a
/// mode the kind does not allow, is long, cannot be read, or holds something else. Nothing
/// is changed.
pub(super) fn inspect(path: &Path, kind: Kind, wanted: &[u8], owner: u32) -> Result<bool, String> {
    let found =
        read_plain(path, kind, owner).map_err(|why| format!("{}: {why}", path.display()))?;
    match found {
        None => Ok(false),
        Some(bytes) => match kind.same(&bytes, wanted) {
            true => Ok(true),
            false => Err(kind.differs(path, &bytes, wanted)),
        },
    }
}

/// The content of the file at `path`; `None` when nothing is there. The file is opened
/// without following a link and without blocking (a named pipe is not waited on), and
/// checked on the open file.
fn read_plain(path: &Path, kind: Kind, owner: u32) -> io::Result<Option<Vec<u8>>> {
    let flags = (OFlags::NONBLOCK | OFlags::NOFOLLOW).bits().cast_signed();
    let opened = OpenOptions::new().read(true).custom_flags(flags).open(path);
    match opened {
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(why),
        Ok(file) => plain(file, kind, owner).map(Some),
    }
}

/// The content of `file`, if it is a regular file of `owner`'s that `kind` allows.
fn plain(file: File, kind: Kind, owner: u32) -> io::Result<Vec<u8>> {
    let meta = file.metadata()?;
    let mode = meta.mode() & 0o7777;
    let problems = [
        (!meta.is_file(), "is not a regular file".to_owned()),
        (meta.uid() != owner, format!("is not owned by uid {owner}")),
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

/// Checks the directory the file at `path` will be made in, where the secret is staged: if it
/// is there it must be a real directory (not a link) of `owner`'s that group and others cannot
/// write, or its owner could swap the staged file before it is linked. A directory that is
/// not there yet is fine: the setup makes it, and a write into a missing one fails by itself.
pub(super) fn check_dir(path: &Path, owner: u32) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(why) => return Err(format!("{}: {why}", dir.display())),
    };
    let problems = [
        (!meta.is_dir(), "is not a directory".to_owned()),
        (meta.uid() != owner, format!("is not owned by uid {owner}")),
        (
            meta.mode() & 0o022 != 0,
            "is writable by group or others".to_owned(),
        ),
    ];
    match problems.into_iter().find(|(bad, _)| *bad) {
        Some((_, why)) => Err(format!("{}: {why}", dir.display())),
        None => Ok(()),
    }
}

/// Makes the file at `path` with `wanted` in it, and looks at it again. Nothing is replaced:
/// a file that appeared since [`inspect`] is an error. The setup has run by now, so every
/// failure here is a failure of a step, not a refusal.
pub(super) fn write_new(path: &Path, kind: Kind, wanted: &[u8], owner: u32) -> Result<(), Failure> {
    check_dir(path, owner).map_err(Failure::Failed)?;
    js_client::create_new(path, wanted, kind.mode())
        .map_err(|why| Failure::Failed(format!("cannot write {}: {why}", path.display())))?;
    read_back(path, inspect(path, kind, wanted, owner))
}

/// What the look at a file just written says: it must be there and be what was written.
/// A file that is gone again is a failure, not a success.
fn read_back(path: &Path, found: Result<bool, String>) -> Result<(), Failure> {
    match found {
        Ok(true) => Ok(()),
        Ok(false) => Err(Failure::Failed(format!(
            "{}: was written and is gone again",
            path.display()
        ))),
        Err(why) => Err(Failure::Failed(why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that an instance file or token file that holds the right bytes with an
    /// allowed mode, but is not the owner's (root's on a host), is refused naming the uid,
    /// and is left as it was. The integration tests cannot make a file another user owns,
    /// so the owner the file is compared with is changed instead. It catches an existing
    /// file of another user's being trusted as the host's identity or secret.
    #[test]
    fn a_file_of_another_owner_is_refused_and_kept() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "enroll_file_owner");
        let mine = rustix::process::geteuid().as_raw();
        for (kind, mode, text) in [
            (Kind::Instance, 0o644, "burst:i-0abc\n"),
            (Kind::Token, 0o600, "0123\n"),
        ] {
            let path = dir.path().join("file");
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
            assert_eq!(inspect(&path, kind, text.as_bytes(), mine), Ok(true));
            let other = mine.wrapping_add(1);
            assert_eq!(
                inspect(&path, kind, text.as_bytes(), other),
                Err(format!("{}: is not owned by uid {other}", path.display()))
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
            fs::remove_file(&path).unwrap();
        }
    }

    /// Asserts that the read-back after a write accepts only a file that is there and right:
    /// one that is gone again is a failure naming the path, and a refusal of what is there
    /// is a failure with its words. It catches a verb that reports `written` and `enrolled`
    /// for a file that is not on the host.
    #[test]
    fn a_file_gone_after_its_write_is_a_failure() {
        let path = Path::new("/etc/js-agent/token");
        let words = |got: Result<(), Failure>| match got {
            Ok(()) => "ok".to_owned(),
            Err(Failure::Failed(text)) => format!("failed: {text}"),
            Err(Failure::Usage(text)) => format!("usage: {text}"),
        };
        assert_eq!(words(read_back(path, Ok(true))), "ok");
        assert_eq!(
            words(read_back(path, Ok(false))),
            "failed: /etc/js-agent/token: was written and is gone again"
        );
        assert_eq!(
            words(read_back(path, Err("it differs".to_owned()))),
            "failed: it differs"
        );
    }

    /// Asserts that the directory check names the directory and the uid when the directory
    /// is not the owner's, with the file check out of the way. It catches a staged secret in
    /// a directory another user could swap it in.
    #[test]
    fn a_directory_of_another_owner_is_refused_by_its_path() {
        let dir = js_testkit::dir::TestDir::under(std::env::temp_dir(), "enroll_dir_owner");
        // The check refuses a directory others can write; the umask of the machine running
        // the test must not decide that.
        fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let mine = rustix::process::geteuid().as_raw();
        let other = mine.wrapping_add(1);
        let file = dir.path().join("token");
        assert_eq!(check_dir(&file, mine), Ok(()));
        assert_eq!(
            check_dir(&file, other),
            Err(format!(
                "{}: is not owned by uid {other}",
                dir.path().display()
            ))
        );
    }
}
