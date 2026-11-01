//! The LaunchDaemons of the js-agent package: the agent and the root helper it dials.
//!
//! Both are described by flags and nothing else: no environment variable, no file the program
//! reads. Neither runs at load. `KeepAlive` holds each of them while [`GATE`], the file with
//! the pool's join token, exists, which is launchd's form of the unit's
//! `ConditionPathExists`: the package can load the daemons at install time, and they start
//! when a host is configured. launchd then starts a daemon again after every exit, a clean
//! one included (the agent exits 0 after it has drained), at most once in
//! [`THROTTLE_SECS`]. Both programs are reached through [`RELEASE_LINK`], which the update
//! hook moves, so a daemon runs the release that is linked when launchd starts it.

use std::fmt::Write as _;

use crate::launchd::escape;

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
/// Seconds launchd gives the agent to exit after SIGTERM before it kills it (`ExitTimeOut`): the
/// unit's `TimeoutStopSec`, the drain the agent's default flags ask for, 10 + 10 + 30 s, with room
/// for the engine calls. `launchctl bootout` returns when the agent has exited, so whatever stops
/// it must wait at least this long.
pub const AGENT_EXIT_TIMEOUT_SECS: u32 = 120;
/// Seconds launchd waits between two starts of a daemon.
pub const THROTTLE_SECS: u32 = 30;
/// The `--server` of a plist nobody has configured; the agent refuses it.
pub const UNSET_SERVER: &str = "SET-THE-SERVER";
/// The helper's socket, which the agent dials.
pub const SESSION_SOCKET: &str = "/var/run/js-mac-session/socket";

/// A LaunchDaemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Daemon {
    /// The label.
    pub label: String,
    /// The program, by absolute path.
    pub program: String,
    /// The arguments after the program.
    pub args: Vec<String>,
    /// Seconds launchd gives the program to exit after SIGTERM before it kills it.
    pub exit_timeout_secs: u32,
    /// The stem of the log files in [`LOG_DIR`].
    pub log_stem: String,
}

impl Daemon {
    /// The agent's daemon for `server` and `pool`, with the exit timeout
    /// [`AGENT_EXIT_TIMEOUT_SECS`].
    #[must_use]
    pub fn agent(server: &str, pool: &str) -> Daemon {
        let words = [
            "--server",
            server,
            "--token-file",
            GATE,
            "--pool",
            pool,
            "--state-root",
            "/var/db/js-agent/state",
            "--scratch-root",
            "/var/db/js-agent/scratch",
            "--native-helper-socket",
            SESSION_SOCKET,
        ];
        Daemon {
            label: AGENT_LABEL.to_owned(),
            program: format!("{RELEASE_LINK}/js-agent"),
            args: words.iter().map(|w| (*w).to_owned()).collect(),
            exit_timeout_secs: AGENT_EXIT_TIMEOUT_SECS,
            log_stem: "js-agent".to_owned(),
        }
    }

    /// The root helper's daemon: its own socket, state directory and uid range, never the
    /// defaults of another helper on the Mac.
    #[must_use]
    pub fn session(daemon_requirement: &str, socket_group: &str) -> Daemon {
        let words = [
            "--socket",
            SESSION_SOCKET,
            "--socket-group",
            socket_group,
            "--state-dir",
            "/var/db/js-mac-session",
            "--uid-range",
            "600-699",
            "--homes",
            "/Users",
            "--daemon-requirement",
            daemon_requirement,
        ];
        Daemon {
            label: SESSION_LABEL.to_owned(),
            program: format!("{RELEASE_LINK}/js-mac-session"),
            args: words.iter().map(|w| (*w).to_owned()).collect(),
            exit_timeout_secs: 20,
            log_stem: "js-mac-session".to_owned(),
        }
    }

    /// The property list.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n",
        );
        let _ = writeln!(
            text,
            "\t<key>Label</key>\n\t<string>{}</string>",
            self.label
        );
        text.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
        for word in std::iter::once(&self.program).chain(&self.args) {
            let _ = writeln!(text, "\t\t<string>{}</string>", escape(word));
        }
        text.push_str("\t</array>\n\t<key>UserName</key>\n\t<string>root</string>\n");
        text.push_str("\t<key>RunAtLoad</key>\n\t<false/>\n");
        let _ = writeln!(
            text,
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>\n\t\t\t<key>{GATE}</key>\n\t\t\t<true/>\n\t\t</dict>\n\t</dict>"
        );
        let _ = writeln!(
            text,
            "\t<key>ThrottleInterval</key>\n\t<integer>{THROTTLE_SECS}</integer>"
        );
        let _ = writeln!(
            text,
            "\t<key>ExitTimeOut</key>\n\t<integer>{}</integer>",
            self.exit_timeout_secs
        );
        let _ = writeln!(
            text,
            "\t<key>StandardOutPath</key>\n\t<string>{LOG_DIR}/{}.log</string>",
            self.log_stem
        );
        let _ = writeln!(
            text,
            "\t<key>StandardErrorPath</key>\n\t<string>{LOG_DIR}/{}.err.log</string>",
            self.log_stem
        );
        text.push_str("</dict>\n</plist>\n");
        text
    }
}
