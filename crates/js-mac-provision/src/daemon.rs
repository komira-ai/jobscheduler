//! The LaunchDaemons of the js-agent package.
//!
//! STUB: the body arrives with the code commit; the tests of `tests/daemon.rs` come first.

/// The label of the agent's daemon, and the name of its plist without `.plist`.
pub const AGENT_LABEL: &str = "com.komira.js-agent";
/// The label of the root helper's daemon.
pub const SESSION_LABEL: &str = "com.komira.js-mac-session";
/// Everything the package installs lies under this directory.
pub const PREFIX: &str = "/usr/local/js-agent";
/// The release link the update hook moves; the daemons run their programs through it.
pub const RELEASE_LINK: &str = "/usr/local/js-agent/link/current";
/// The directory of the releases.
pub const RELEASES_DIR: &str = "/usr/local/js-agent/releases";
/// The directory the package keeps the plists it installs from.
pub const SHARE_DIR: &str = "/usr/local/js-agent/share";
/// Where launchd reads the daemons from.
pub const LAUNCH_DAEMONS_DIR: &str = "/Library/LaunchDaemons";
/// Where the daemons write their output.
pub const LOG_DIR: &str = "/Library/Logs/js-agent";
/// The path whose existence lets the daemons run.
pub const GATE: &str = "/etc/js-agent/token";
/// Seconds launchd waits between two starts of a daemon.
pub const THROTTLE_SECS: u32 = 30;
/// The `--server` of a plist nobody has configured; the agent refuses it.
pub const UNSET_SERVER: &str = "SET-THE-SERVER";

/// A LaunchDaemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Daemon {
    /// The label.
    pub label: String,
    /// The program, by absolute path.
    pub program: String,
    /// The arguments after the program.
    pub args: Vec<String>,
    /// Seconds launchd gives the program to exit after SIGTERM.
    pub exit_timeout_secs: u32,
    /// The stem of the log files.
    pub log_stem: String,
}

impl Daemon {
    /// The agent's daemon for `server` and `pool`.
    #[must_use]
    pub fn agent(_server: &str, _pool: &str) -> Daemon {
        Daemon {
            label: String::new(),
            program: String::new(),
            args: Vec::new(),
            exit_timeout_secs: 0,
            log_stem: String::new(),
        }
    }

    /// The root helper's daemon.
    #[must_use]
    pub fn session(_daemon_requirement: &str, _socket_group: &str) -> Daemon {
        Daemon::agent("", "")
    }

    /// The property list.
    #[must_use]
    pub fn render(&self) -> String {
        String::new()
    }
}
