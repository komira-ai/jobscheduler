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
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// The entry of `folders` that the `DARWIN_USER_DIR` answer `dir` names: `dir` must be
/// `/var/folders/<xx>/<hash>/0` (or `/private/var/folders/...`, with or without the last
/// slash), `<xx>` two characters and `<hash>` one plain name; the entry is
/// `<folders>/<xx>/<hash>`.
///
/// # Errors
/// `dir` has any other shape: relative, another root, `.` or `..` in it, or another
/// depth or last name.
pub(super) fn entry(dir: &Path, folders: &Path) -> Result<PathBuf, String> {
    let refused = || {
        format!(
            "the DARWIN_USER_DIR {} is not an entry of /var/folders; nothing is removed for it",
            dir.display()
        )
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
