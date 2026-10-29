//! The host credential's home: one file in the state root, `host-credential`.
//!
//! A server that admits a host with a one-time join token for the first time and was told
//! the agent keeps a credential ([`js_agent_wire::Hello::accepts_host_credential`]) answers
//! with a [`HostCredential`] and retires the token. From then on the credential is the only
//! thing that authenticates the host, so it has to survive a restart of the agent.
//!
//! - **It is written before it is used.** The text goes to a temporary file in the state
//!   root (mode 0600 whatever the umask), is synced, is renamed over the credential file,
//!   and the directory is synced. A crash leaves the old file or the new one, never half of
//!   one.
//! - **It is read when the agent starts.** A file that is there and cannot be read or is not
//!   a credential stops the agent with the path named; it is never taken for an absent one,
//!   because the token it would fall back to is dead.
//! - **Nothing prints it.** The file's path may appear in an error, the text never.
//! - **It belongs to the state root.** The state root is private to the agent's user and
//!   no job may reach it (the agent refuses a state root inside a mount root).

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use js_agent_wire::HostCredential;

/// The name of the credential file in the state root.
pub const CREDENTIAL_FILE: &str = "host-credential";

/// The temporary file the credential is written through, renamed over [`CREDENTIAL_FILE`].
pub const CREDENTIAL_TEMP_FILE: &str = "host-credential.tmp";

/// What the error says of a file that is there and holds no credential.
const NOT_A_CREDENTIAL: &str = "does not hold a host credential";

/// The most bytes of the file that are read. A credential is 64 digits.
const CREDENTIAL_MAX: u64 = 4096;

/// Where a session keeps the credential its server gave it.
pub trait CredentialStore {
    /// Makes `credential` durable, so a restarted agent finds it.
    ///
    /// # Errors
    ///
    /// Any step failed. The file is then what it was before, or the new one: never a part
    /// of it.
    fn save(&self, credential: &HostCredential) -> io::Result<()>;
}

/// The credential file of a state root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialFile {
    dir: PathBuf,
}

impl CredentialFile {
    /// The credential file in the state root at `dir`, which the caller has checked to be
    /// private ([`crate::lifecycle::StateRoot`]).
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The path of the credential file, which may not exist.
    pub fn path(&self) -> PathBuf {
        self.dir.join(CREDENTIAL_FILE)
    }

    /// The credential on record: `None` when there is no file.
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read, or does not hold a credential (more than one
    /// run of the credential alphabet, or too long). The text names the file and never
    /// holds a byte of its content.
    pub fn load(&self) -> Result<Option<HostCredential>, String> {
        let path = self.path();
        let fail = |why: &str| format!("{}: {why}", path.display());
        let read = File::open(&path).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(CREDENTIAL_MAX)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let bytes = match read {
            Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(why) => return Err(fail(&format!("cannot be read: {}", why.kind()))),
            Ok(bytes) => bytes,
        };
        let text = String::from_utf8(bytes).map_err(|_| fail(NOT_A_CREDENTIAL))?;
        HostCredential::parse(text.trim())
            .map(Some)
            .map_err(|_| fail(NOT_A_CREDENTIAL))
    }
}

impl CredentialStore for CredentialFile {
    fn save(&self, credential: &HostCredential) -> io::Result<()> {
        let temp = self.dir.join(CREDENTIAL_TEMP_FILE);
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .and_then(|mut file| {
                // A temporary file left by a crash keeps the mode it had; the credential
                // gets 0600 whatever that was.
                file.set_permissions(Permissions::from_mode(0o600))
                    .and_then(|()| file.write_all(credential.expose().as_bytes()))
                    .and_then(|()| file.sync_all())
            })
            .and_then(|()| fs::rename(&temp, self.path()))
            .and_then(|()| File::open(&self.dir).and_then(|dir| dir.sync_all()))
    }
}
