//! The API key: read from a file that only its owner can use, held so that it cannot be
//! printed.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::OFlags;
use rustix::process::geteuid;

/// The most a key file may hold; a key is a short line.
const MAX_KEY_FILE: usize = 4096;

/// The API key. It has no `Display`, and its `Debug` leaves the key out.
#[derive(Clone)]
pub struct ApiKey(String);

/// Why a key file was refused. No variant holds any of the file's content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The file cannot be opened or read.
    #[error("{path}: cannot be read: {why}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The operating system's reason.
        why: String,
    },
    /// The file is not a regular file owned by the user running the program, or others
    /// can use it.
    #[error(
        "{path}: must be a regular file owned by you that nobody else can read or write, not mode {mode:04o} owned by uid {owner}"
    )]
    Untrusted {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
        /// Its owner.
        owner: u32,
    },
    /// The file holds nothing but whitespace.
    #[error("{path}: is empty")]
    Empty {
        /// The file.
        path: PathBuf,
    },
    /// The file is longer than a key can be.
    #[error("{path}: is longer than {MAX_KEY_FILE} bytes")]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file holds more than one token, or a character a header cannot carry.
    #[error("{path}: must hold one key of printable ASCII with no space or line break inside it")]
    NotPlain {
        /// The file.
        path: PathBuf,
    },
}

/// What opening a key file showed.
struct Probe {
    is_file: bool,
    mode: u32,
    owner: u32,
    bytes: Vec<u8>,
}

impl ApiKey {
    /// Reads the key in `path`: a regular file owned by the user running the program with
    /// no group or other permission bits, holding one key of printable ASCII (trailing
    /// whitespace, such as the final newline, is dropped).
    ///
    /// # Errors
    ///
    /// [`KeyError`] naming the file and the rule it broke.
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        Self::load(path, geteuid().as_raw())
    }

    fn load(path: &Path, euid: u32) -> Result<Self, KeyError> {
        let probe = probe(path, euid).map_err(|err| KeyError::Read {
            path: path.to_owned(),
            why: err.to_string(),
        })?;
        if !trusted(&probe, euid) {
            return Err(KeyError::Untrusted {
                path: path.to_owned(),
                mode: probe.mode,
                owner: probe.owner,
            });
        }
        Self::parse(path, &probe.bytes)
    }

    /// The key in `bytes`, the content of the file at `path` that a caller with its own
    /// way of reading owner-only files has read (the service reads every secret file one
    /// way). The same content rules as [`ApiKey::from_file`]: one key of printable ASCII,
    /// trailing whitespace dropped, at most 4096 bytes. `path` only names the file in the
    /// refusal.
    ///
    /// # Errors
    ///
    /// [`KeyError::TooLarge`], [`KeyError::Empty`] or [`KeyError::NotPlain`].
    pub fn parse(path: &Path, bytes: &[u8]) -> Result<Self, KeyError> {
        let path = path.to_owned();
        if bytes.len() > MAX_KEY_FILE {
            return Err(KeyError::TooLarge { path });
        }
        let key = bytes.trim_ascii_end();
        match (key.is_empty(), key.iter().all(u8::is_ascii_graphic)) {
            (true, _) => Err(KeyError::Empty { path }),
            (false, false) => Err(KeyError::NotPlain { path }),
            (false, true) => Ok(Self(String::from_utf8_lossy(key).into_owned())),
        }
    }

    /// The `Authorization` header value.
    pub(crate) fn authorization(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("ApiKey(<redacted>)")
    }
}

/// Opens `path` without blocking, so a named pipe is refused instead of waited on (a
/// symbolic link is followed: secret mounts are links), and reads it only when the open
/// file passes the checks.
fn probe(path: &Path, euid: u32) -> std::io::Result<Probe> {
    let nonblocking = OFlags::NONBLOCK.bits().cast_signed();
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path)?;
    let meta = file.metadata()?;
    let mut probe = Probe {
        is_file: meta.is_file(),
        mode: meta.permissions().mode() & 0o777,
        owner: meta.uid(),
        bytes: Vec::new(),
    };
    if trusted(&probe, euid) {
        (&mut file)
            .take(MAX_KEY_FILE as u64 + 1)
            .read_to_end(&mut probe.bytes)?;
    }
    Ok(probe)
}

/// Whether the file may be trusted: regular, owned by `euid`, no group or other bits.
fn trusted(probe: &Probe, euid: u32) -> bool {
    probe.is_file & (probe.mode & 0o077 == 0) & (probe.owner == euid)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_file_of_another_owner_is_refused_without_being_read() {
        let dir = std::env::temp_dir().join(format!("lambdalabs-key-owner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        fs::write(&path, "k\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mine = geteuid().as_raw();
        let err = ApiKey::load(&path, mine.wrapping_add(1)).unwrap_err();
        let expected = KeyError::Untrusted {
            path: path.clone(),
            mode: 0o600,
            owner: mine,
        };
        assert_eq!(err, expected);
        assert!(ApiKey::load(&path, mine).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
