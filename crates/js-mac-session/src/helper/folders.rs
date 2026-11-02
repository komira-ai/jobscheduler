//! The uid's own entry under the per-user folders, from its `DARWIN_USER_DIR`.
//!
//! macOS keeps a folder per user under `/var/folders` (`/private/var/folders`, `/var` is a
//! link): `/var/folders/<xx>/<hash>/`, keyed by the user record's UUID and uid, holding
//! `0` (`DARWIN_USER_DIR`), `C` (`DARWIN_USER_CACHE_DIR`) and `T`
//! (`DARWIN_USER_TEMP_DIR`). Daemons make folders there for the user (a
//! `com.apple.ScreenTimeAgent` folder under `C`, say) that outlive the account, so
//! `user-delete` removes the entry whole. The entry is the one the uid's own
//! `DARWIN_USER_DIR` names, never one guessed from the uid: only an answer of exactly
//! that shape is taken, and its entry is spelled under the configured folders, so no
//! answer can name a path outside them.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A `DARWIN_USER_DIR` answer that names no entry of the per-user folders.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the DARWIN_USER_DIR {} is not an entry of /var/folders; nothing is removed for it",
    .dir.display()
)]
pub(crate) struct NotAnEntry {
    /// The answer, as it was given.
    dir: PathBuf,
}

/// The program, and its argument, that answer `DARWIN_USER_DIR` on macOS.
// Only `MacHost` runs the resolver; off macOS the tests do.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const GETCONF: (&str, &str) = ("/usr/bin/getconf", "DARWIN_USER_DIR");

/// Runs `program` with `args` as `uid` and `gid`, out of any session: an empty
/// environment, `/` as its working directory, standard input empty (std drops the
/// caller's supplementary groups before the `setuid` when the caller is root), and waits
/// for it. Its answer is [`answer`] of what it printed.
///
/// # Errors
/// The program could not be started or waited for, or [`answer`] refused its output.
// Only `MacHost` runs the resolver; off macOS the tests do.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn ask(program: &str, args: &[&str], uid: u32, gid: u32) -> io::Result<PathBuf> {
    use std::os::unix::process::CommandExt as _;
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .current_dir("/")
        .uid(uid)
        .gid(gid)
        .stdin(Stdio::null())
        .output()?;
    answer(output.status.code(), &output.stdout, &output.stderr)
}

/// The path a resolver printed: its standard output, one line of text, the newline
/// that ends it dropped.
///
/// # Errors
/// It did not exit 0 (or a signal ended it), or its output is not exactly one
/// non-empty line of text.
// Only `MacHost` runs the resolver; off macOS the tests do.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn answer(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> io::Result<PathBuf> {
    let said = String::from_utf8_lossy(stderr);
    let refused = |why: String| io::Error::other(format!("{why}: {}", said.trim()));
    match code {
        Some(0) => {}
        Some(code) => return Err(refused(format!("exited {code}"))),
        None => return Err(refused("ended by a signal".to_owned())),
    }
    let text = std::str::from_utf8(stdout)
        .map_err(|_| refused("printed something that is not text".to_owned()))?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.is_empty() | line.contains('\n') {
        return Err(refused(format!("printed {text:?}, not one path")));
    }
    Ok(PathBuf::from(line))
}

/// The entry of `folders` that the `DARWIN_USER_DIR` answer `dir` names: `dir` must be
/// `/var/folders/<xx>/<hash>/0` (or `/private/var/folders/...`, with or without the last
/// slash), `<xx>` two characters and `<hash>` one plain name; the entry is
/// `<folders>/<xx>/<hash>`.
///
/// # Errors
/// `dir` has any other shape: relative, another root, `.` or `..` in it, or another
/// depth or last name.
pub(crate) fn entry(dir: &Path, folders: &Path) -> Result<PathBuf, NotAnEntry> {
    let refused = || NotAnEntry {
        dir: dir.to_owned(),
    };
    // Split by hand, not by `Path::components`, which drops a `.` and a doubled slash
    // and would let a path of another spelling through.
    let text = dir.as_os_str().as_encoded_bytes();
    let Some(rest) = text.strip_prefix(b"/") else {
        return Err(refused());
    };
    let rest = rest.strip_suffix(b"/").unwrap_or(rest);
    let names: Vec<&[u8]> = rest.split(|b| *b == b'/').collect();
    let names = match names.as_slice() {
        [b"private", rest @ ..] => rest,
        all => all,
    };
    match names {
        [b"var", b"folders", xx, hash, b"0"] if xx.len() == 2 && plain(xx) && plain(hash) => {
            Ok(folders
                .join(OsStr::from_bytes(xx))
                .join(OsStr::from_bytes(hash)))
        }
        _ => Err(refused()),
    }
}

/// Whether `name` is a plain name of an entry: not empty, and only the characters
/// macOS spells these entries with (letters, digits, `+`, `-`, `_`).
fn plain(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'_'))
}

#[cfg(test)]
mod tests;
