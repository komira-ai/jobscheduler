//! The flags a declaration gives the hook before the step: the words of its `hook_args` after
//! `update-hook`, each `--name=value`.
//!
//! ```text
//! js-agent update-hook --release-link=/opt/js-agent/link/current --state-root=/var/lib/js-agent/state cutover --params <file>
//! ```
//!
//! They carry the six text values the hook may also read from the params table: the release
//! link and how to reach the lease server and the agent's state ([`ConnectParams`]). The host
//! updater refuses a string in `[hook.params]` to a hook that runs as root, which this hook
//! does on Linux, so the declaration of a Linux host gives them here; one value may come from
//! one place only ([`Params::with_flags`]). Each flag is the agent's own flag of that name where
//! the agent has one.
//!
//! [`ConnectParams`]: super::ConnectParams
//! [`Params::with_flags`]: super::Params::with_flags

use std::path::PathBuf;

use super::args::ArgsError;

/// `--release-link`: the release link, as the params key `release_link`.
pub const RELEASE_LINK_FLAG: &str = "--release-link";
/// `--server`: the lease server, as the params key `server`.
pub const SERVER_FLAG: &str = "--server";
/// `--state-root`: the agent's state root, as the params key `state_root`.
pub const STATE_ROOT_FLAG: &str = "--state-root";
/// `--token-file`: the join token file, as the params key `token_file`.
pub const TOKEN_FILE_FLAG: &str = "--token-file";
/// `--ca-cert`: the certificate to trust, as the params key `ca_cert`.
pub const CA_CERT_FLAG: &str = "--ca-cert";
/// `--host-id`: the host's id, as the params key `host_id`.
pub const HOST_ID_FLAG: &str = "--host-id";

/// The values given as flags before the step. Each is `None` when its flag is absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `--release-link`, an absolute path.
    pub release_link: Option<PathBuf>,
    /// `--server`.
    pub server: Option<String>,
    /// `--state-root`, an absolute path.
    pub state_root: Option<PathBuf>,
    /// `--token-file`, an absolute path.
    pub token_file: Option<PathBuf>,
    /// `--ca-cert`, an absolute path.
    pub ca_cert: Option<PathBuf>,
    /// `--host-id`.
    pub host_id: Option<String>,
}

/// Puts `value` in `slot`, which must be empty.
fn fill<T>(slot: &mut Option<T>, name: &str, value: T) -> Result<(), ArgsError> {
    match slot.replace(value) {
        Some(_) => Err(ArgsError::DuplicateFlag(name.to_owned())),
        None => Ok(()),
    }
}

/// `value` as an absolute path.
fn absolute(name: &str, value: &str) -> Result<PathBuf, ArgsError> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ArgsError::RelativeFlag(name.to_owned()))
    }
}

impl Flags {
    /// Takes one word that comes before the step: `--name=value`, with a name above and a
    /// value that is not empty.
    ///
    /// # Errors
    /// [`ArgsError::UnknownFlag`] for another name or a word that is not text,
    /// [`ArgsError::FlagValue`] for a flag with no `=value` or an empty one,
    /// [`ArgsError::DuplicateFlag`] for a flag given twice and [`ArgsError::RelativeFlag`] for
    /// a path that is not absolute.
    pub fn take(&mut self, word: &str) -> Result<(), ArgsError> {
        let (name, value) = word.split_once('=').unwrap_or((word, ""));
        match name {
            RELEASE_LINK_FLAG | SERVER_FLAG | STATE_ROOT_FLAG | TOKEN_FILE_FLAG | CA_CERT_FLAG
            | HOST_ID_FLAG => {}
            _ => return Err(ArgsError::UnknownFlag(word.to_owned())),
        }
        if value.is_empty() {
            return Err(ArgsError::FlagValue(name.to_owned()));
        }
        match name {
            RELEASE_LINK_FLAG => fill(&mut self.release_link, name, absolute(name, value)?),
            SERVER_FLAG => fill(&mut self.server, name, value.to_owned()),
            STATE_ROOT_FLAG => fill(&mut self.state_root, name, absolute(name, value)?),
            TOKEN_FILE_FLAG => fill(&mut self.token_file, name, absolute(name, value)?),
            CA_CERT_FLAG => fill(&mut self.ca_cert, name, absolute(name, value)?),
            _ => fill(&mut self.host_id, name, value.to_owned()),
        }
    }
}
