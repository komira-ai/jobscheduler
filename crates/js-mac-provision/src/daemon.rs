//! The LaunchDaemons of the js-agent package: the agent, the root helper it dials, and the
//! daemon that keeps the pool's pf anchor loaded ([`pf_pool_plist`]).
//!
//! The agent and the helper are described by flags and nothing else: no environment variable;
//! the agent's plist names a flags file ([`FLAGS_FILE`]), which holds only more flags. Neither of
//! the two runs at load. `KeepAlive` holds each of them while [`GATE`], the host's flags file, exists, which is
//! launchd's form of the unit's `ConditionPathExists`. The gate is the flags file and not the
//! join token ([`TOKEN_FILE`]): a Mac joins by hostkeeper's host certificate, which the flags
//! file names, and has no token; every host has its flags, which name its server and pool. The
//! package runs no install script: the update hook bootstraps the plists, and a daemon starts
//! once the host has its flags. launchd then starts a daemon again after every exit, a clean
//! one included (the agent exits 0 after it has drained), at most once in
//! [`THROTTLE_SECS`]. The programs are the package's own files under [`BIN_DIR`]; the
//! agent's host-specific flags (server, pool, network mode) are read from [`FLAGS_FILE`],
//! which a per-group configuration package installs.
//!
//! Both start with [`OPEN_FILES`] open files, soft and hard (`SoftResourceLimits` and
//! `HardResourceLimits`, `NumberOfFiles`), not with launchd's default soft limit of 256: the agent
//! serves many runs at once, each with pipes, log streams, lease files and connections, and the
//! jobs the helper starts inherit its limit. The kernel holds the limit to its own per-process
//! cap (`kern.maxfilesperproc`) where that is lower.
//!
//! The pf daemon is of another kind: it runs at load and every [`PF_POOL_INTERVAL_SECS`]
//! seconds, and each run is `js-mac-provision install --flags-file` [`FLAGS_FILE`], which loads
//! the anchor the agent's own flags name unless it is in force already, and does nothing while
//! the file is absent or sets no `--native-network=on` (see [`crate::agent_flags`]).

use std::fmt::Write as _;

use crate::launchd::{LABEL, escape, render_plist};

/// The label of the agent's daemon, and the name of its plist without `.plist`.
pub const AGENT_LABEL: &str = "js-agent";
/// The label of the root helper's daemon.
pub const SESSION_LABEL: &str = "js-agent.session";
/// Everything the package installs, apart from the plists, lies under this directory.
pub const PREFIX: &str = "/Library/Application Support/js-agent";
/// The directory of the package's programs.
pub const BIN_DIR: &str = "/Library/Application Support/js-agent/bin";
/// Where the daemons write their output.
pub const LOG_DIR: &str = "/Library/Application Support/js-agent/log";
/// The update hook's state directory.
pub const UPDATE_DIR: &str = "/Library/Application Support/js-agent/update";
/// Where launchd reads the daemons from.
pub const LAUNCH_DAEMONS_DIR: &str = "/Library/LaunchDaemons";
/// The file of the agent's host-specific flags, which a configuration package installs.
pub const FLAGS_FILE: &str = "/etc/js-agent/flags";
/// The path whose existence lets the daemons run: the flags file.
pub const GATE: &str = FLAGS_FILE;
/// The join token file the agent's `--token-file` names. A host whose flags name its host
/// certificate (`--host-cert`, `--host-key`) has none, and the agent then reads none.
pub const TOKEN_FILE: &str = "/etc/js-agent/token";
/// Seconds launchd gives the agent to exit after SIGTERM before it kills it (`ExitTimeOut`): the
/// unit's `TimeoutStopSec`, the drain the agent's default flags ask for, 10 + 10 + 30 s, with room
/// for the engine calls. `launchctl bootout` returns when the agent has exited, so whatever stops
/// it must wait at least this long.
pub const AGENT_EXIT_TIMEOUT_SECS: u32 = 120;
/// The open-files limit, soft and hard, of the agent and the helper.
const OPEN_FILES: u32 = 65536;
/// Seconds launchd waits between two starts of a daemon.
pub const THROTTLE_SECS: u32 = 30;
/// The helper's socket, which the agent dials.
pub const SESSION_SOCKET: &str = "/var/run/js-mac-session/socket";
/// The group that may reach the helper's socket.
pub const SOCKET_GROUP: &str = "_jsagent";
/// The label of the daemon that keeps the pool's pf anchor loaded.
pub const PF_POOL_LABEL: &str = LABEL;
/// Seconds between two runs of the pf daemon, after the one at load.
pub const PF_POOL_INTERVAL_SECS: u32 = 30;

/// The property list of the pf daemon: `js-mac-provision install --flags-file` with the agent's
/// flags file, at load and every [`PF_POOL_INTERVAL_SECS`] seconds, as root. Fixed, like the
/// agent's: the policy is read from the flags file at each run, never written into the plist.
#[must_use]
pub fn pf_pool_plist() -> String {
    let args = ["install", "--flags-file", FLAGS_FILE].map(str::to_owned);
    render_plist(
        &format!("{BIN_DIR}/js-mac-provision"),
        &args,
        PF_POOL_INTERVAL_SECS,
    )
}

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
    /// The agent's daemon, with the exit timeout [`AGENT_EXIT_TIMEOUT_SECS`]. The host's own
    /// flags are in [`FLAGS_FILE`].
    #[must_use]
    pub fn agent() -> Daemon {
        let words = [
            "--flags-file",
            FLAGS_FILE,
            "--token-file",
            TOKEN_FILE,
            "--state-root",
            "/var/db/js-agent/state",
            "--scratch-root",
            "/var/db/js-agent/scratch/runs",
        ];
        Daemon {
            label: AGENT_LABEL.to_owned(),
            program: format!("{BIN_DIR}/js-agent"),
            args: words.iter().map(|w| (*w).to_owned()).collect(),
            exit_timeout_secs: AGENT_EXIT_TIMEOUT_SECS,
            log_stem: "js-agent".to_owned(),
        }
    }

    /// The root helper's daemon, holding its caller to `daemon_requirement`: its own socket, state directory and uid range, never the
    /// defaults of another helper on the Mac.
    #[must_use]
    pub fn session(daemon_requirement: &str) -> Daemon {
        let words = [
            "--socket",
            SESSION_SOCKET,
            "--socket-group",
            SOCKET_GROUP,
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
            program: format!("{BIN_DIR}/js-mac-session"),
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
        for key in ["SoftResourceLimits", "HardResourceLimits"] {
            let _ = writeln!(
                text,
                "\t<key>{key}</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>{OPEN_FILES}</integer>\n\t</dict>"
            );
        }
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
