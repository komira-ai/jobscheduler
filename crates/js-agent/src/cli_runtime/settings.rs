//! The flags that configure containers, checked when the agent starts.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The user and group containers run as when `--container-user` is not given: `nobody`
/// and `nogroup` on the common Linux distributions.
pub const DEFAULT_USER: &str = "65534:65534";

/// The network containers get when `--container-network` is not given: none at all.
pub const DEFAULT_NETWORK: &str = "none";

/// The longest network name accepted.
const NETWORK_NAME_MAX: usize = 64;

/// Why a container flag is wrong. The text names the flag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// `--container-bin` is relative or not UTF-8: it is started by exactly this path,
    /// never looked up on the `PATH`.
    #[error("--container-bin: not an absolute UTF-8 path")]
    BinPath,
    /// `--container-bin` is not an executable regular file.
    #[error("--container-bin: not an executable file")]
    BinMissing,
    /// `--container-network` names no network the agent will use: `host` (it would put
    /// the job on the machine's own network), or text that is not a network name (which
    /// would let `container:<id>` or an option pass as one).
    #[error("--container-network: not a network name the agent will use")]
    Network,
    /// `--container-user` is not `<uid>:<gid>` with both numbers above zero.
    #[error("--container-user: not <uid>:<gid> of a user that is not root")]
    User,
    /// `--container-network` or `--container-user` without `--container-bin`: they
    /// configure containers, which the agent runs only with a runtime.
    #[error("--container-network and --container-user need --container-bin")]
    NoBin,
}

/// The checked container flags: where the runtime is, which network jobs get and which
/// user they run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    bin: String,
    network: String,
    user: String,
}

impl Settings {
    /// Checks the three flags. `None` for all three means containers are not enabled.
    ///
    /// # Errors
    ///
    /// The flag at fault; see [`SettingsError`].
    pub fn from_flags(
        bin: Option<&Path>,
        network: Option<&str>,
        user: Option<&str>,
    ) -> Result<Option<Self>, SettingsError> {
        match (bin, network.or(user)) {
            (Some(bin), _) => Self::new(bin, network, user).map(Some),
            (None, None) => Ok(None),
            (None, Some(_)) => Err(SettingsError::NoBin),
        }
    }

    /// Checks `bin` (an absolute path to an executable file) and the optional `network`
    /// and `user`.
    ///
    /// # Errors
    ///
    /// The flag at fault; see [`SettingsError`].
    pub fn new(
        bin: &Path,
        network: Option<&str>,
        user: Option<&str>,
    ) -> Result<Self, SettingsError> {
        let bin = bin
            .to_str()
            .filter(|text| text.starts_with('/'))
            .ok_or(SettingsError::BinPath)?;
        fs::metadata(bin)
            .ok()
            .filter(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .ok_or(SettingsError::BinMissing)?;
        let network = network.map_or(Ok(DEFAULT_NETWORK), network_name)?;
        let user = user.map_or(Ok(DEFAULT_USER.to_owned()), non_root)?;
        Ok(Self {
            bin: bin.to_owned(),
            network: network.to_owned(),
            user,
        })
    }

    /// The runtime's absolute path.
    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// The network the containers join.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// The `<uid>:<gid>` the containers run as.
    pub fn user(&self) -> &str {
        &self.user
    }
}

/// `name` when it is a plain network name other than `host`.
fn network_name(name: &str) -> Result<&str, SettingsError> {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let shaped = (1..=NETWORK_NAME_MAX).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(word);
    Some(name)
        .filter(|name| shaped && !name.eq_ignore_ascii_case("host"))
        .ok_or(SettingsError::Network)
}

/// `<uid>:<gid>` of numbers above zero, written canonically.
fn non_root(text: &str) -> Result<String, SettingsError> {
    let number = |part: &str| part.parse::<u32>().ok().filter(|n| *n > 0);
    text.split_once(':')
        .and_then(|(uid, gid)| number(uid).zip(number(gid)))
        .map(|(uid, gid)| format!("{uid}:{gid}"))
        .ok_or(SettingsError::User)
}
