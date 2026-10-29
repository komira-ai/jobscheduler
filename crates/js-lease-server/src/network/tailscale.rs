//! The `tailscale` mode: the host joins a tailnet first, with an auth key minted for
//! that launch.

use std::time::Duration;

use js_elastic::{Artifact, BootStep, HostName, SlotName};

use super::{Contribution, FORGET_ATTEMPTS, LaunchCtx, NetworkError, NetworkMode, TailscaleApi};

/// The slot the auth key fills.
pub const AUTH_KEY_SLOT: &str = "network.tailscale.authkey";

/// Where the auth key is written on the host, and removed from after `tailscale up`.
pub const AUTH_KEY_PATH: &str = "/etc/jobscheduler/ts-authkey";

/// The longest an auth key may be asked to stay valid. A key is for one boot: minutes.
pub const MAX_KEY_VALID_FOR: Duration = Duration::from_secs(60 * 60);

/// The most characters Tailscale accepts in an auth key's description.
pub const KEY_DESCRIPTION_MAX: usize = 50;

/// Where the Tailscale archive is downloaded to.
const ARCHIVE_PATH: &str = "/usr/local/lib/tailscale.tgz";

/// Where the binaries are unpacked to.
const BIN_DIR: &str = "/usr/local/bin";

/// Where `tailscaled` keeps its state.
const STATE_PATH: &str = "/var/lib/tailscale/tailscaled.state";

/// What the mode needs besides an API client.
#[derive(Debug, Clone)]
pub struct TailscaleSettings {
    /// The tag every launched host carries, such as `tag:js-agent`. The OAuth client
    /// must be limited to it (the operator's part).
    pub tag: String,
    /// The Tailscale static build to fetch: its URL and SHA-256. The pinned table of
    /// versions is the release's to keep; the mode takes the entry it uses.
    pub binary: Artifact,
}

/// Whether `tag` is `tag:` and a name of letters, digits and '-'.
pub(crate) fn valid_tag(tag: &str) -> bool {
    tag.strip_prefix("tag:").is_some_and(|name| {
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// The mode that joins hosts to a tailnet.
pub struct TailscaleMode {
    api: TailscaleApi,
    settings: TailscaleSettings,
}

impl std::fmt::Debug for TailscaleMode {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("TailscaleMode")
            .field("api", &self.api)
            .field("tag", &self.settings.tag)
            .finish_non_exhaustive()
    }
}

impl TailscaleMode {
    /// The mode over `api` with `settings`.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Misconfigured`] when the tag is not `tag:` and a name.
    pub fn new(api: TailscaleApi, settings: TailscaleSettings) -> Result<Self, NetworkError> {
        match valid_tag(&settings.tag) {
            true => Ok(Self::unchecked(api, settings)),
            false => Err(NetworkError::Misconfigured(format!(
                "tailscale tag {:?} must be \"tag:\" and letters, digits or '-'",
                settings.tag
            ))),
        }
    }

    /// The mode over `api` for a `settings` whose tag is known to pass [`valid_tag`], as
    /// the configuration file's reader has already checked it.
    pub(crate) fn unchecked(api: TailscaleApi, settings: TailscaleSettings) -> Self {
        Self { api, settings }
    }

    /// The steps of one launch, with the auth key named by its slot.
    fn steps(&self, host: &HostName) -> Vec<BootStep> {
        let run = |argv: &[&str]| BootStep::Run(argv.iter().map(|&a| a.to_owned()).collect());
        let tailscaled = format!("{BIN_DIR}/tailscaled");
        let tailscale = format!("{BIN_DIR}/tailscale");
        vec![
            BootStep::Fetch {
                url: self.settings.binary.url.clone(),
                sha256: self.settings.binary.sha256,
                dest: ARCHIVE_PATH.to_owned(),
            },
            run(&[
                "tar",
                "-xzf",
                ARCHIVE_PATH,
                "--strip-components=1",
                "-C",
                BIN_DIR,
            ]),
            BootStep::SecretFile {
                path: AUTH_KEY_PATH.to_owned(),
                slot: SlotName(AUTH_KEY_SLOT.to_owned()),
            },
            run(&[
                "systemd-run",
                "--unit=tailscaled",
                tailscaled.as_str(),
                format!("--state={STATE_PATH}").as_str(),
            ]),
            run(&[
                tailscale.as_str(),
                "up",
                format!("--auth-key=file:{AUTH_KEY_PATH}").as_str(),
                format!("--hostname={host}").as_str(),
                "--accept-dns=false",
            ]),
            run(&["rm", "-f", "--", AUTH_KEY_PATH]),
        ]
    }
}

/// The description of a launch's key: the host's name, cut to [`KEY_DESCRIPTION_MAX`]
/// characters.
fn key_description(host: &str) -> String {
    host.chars().take(KEY_DESCRIPTION_MAX).collect()
}

impl NetworkMode for TailscaleMode {
    /// Mints one auth key for the launch and returns the steps that use it.
    ///
    /// # Errors
    ///
    /// [`NetworkError::KeyLifetime`] when `valid_for` is under a second or over
    /// [`MAX_KEY_VALID_FOR`]; otherwise the errors of [`TailscaleApi::mint_key`].
    fn prepare(&self, launch: &LaunchCtx<'_>) -> Result<Contribution, NetworkError> {
        let secs = launch.valid_for.as_secs();
        if secs == 0 || launch.valid_for > MAX_KEY_VALID_FOR {
            return Err(NetworkError::KeyLifetime {
                asked: launch.valid_for,
                max: MAX_KEY_VALID_FOR.as_secs(),
            });
        }
        let description = key_description(launch.host.as_str());
        let key = self.api.mint_key(&self.settings.tag, &description, secs)?;
        Ok(Contribution {
            steps: self.steps(launch.host),
            secrets: vec![(SlotName(AUTH_KEY_SLOT.to_owned()), key)],
        })
    }

    fn describe(&self, launch: &LaunchCtx<'_>) -> Vec<BootStep> {
        self.steps(launch.host)
    }

    /// Exchanges the OAuth client for a token. Mints no key.
    fn check(&self) -> Result<(), NetworkError> {
        self.api.check()
    }

    /// Deletes the host's device, trying again up to [`FORGET_ATTEMPTS`] times while the
    /// failure may pass.
    fn forget(&self, host: &HostName) -> Result<(), NetworkError> {
        let mut result = self.api.delete_devices(host.as_str(), &self.settings.tag);
        for _ in 1..FORGET_ATTEMPTS {
            if !result.as_ref().is_err_and(NetworkError::transient) {
                break;
            }
            result = self.api.delete_devices(host.as_str(), &self.settings.tag);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that a description of exactly the limit is kept whole and one character
    /// more is cut to the limit.
    #[test]
    fn a_key_description_is_cut_at_the_limit() {
        let at = "h".repeat(KEY_DESCRIPTION_MAX);
        assert_eq!(key_description(&at), at);
        let over = "h".repeat(KEY_DESCRIPTION_MAX + 1);
        assert_eq!(key_description(&over), at);
        assert_eq!(KEY_DESCRIPTION_MAX, 50);
    }
}
