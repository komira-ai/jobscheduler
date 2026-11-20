//! The property lists of the package's two LaunchDaemons, against golden files, and the
//! rules every one of them keeps.

use js_mac_provision::daemon::{
    AGENT_LABEL, BIN_DIR, Daemon, FLAGS_FILE, GATE, LOG_DIR, SESSION_LABEL, THROTTLE_SECS,
    TOKEN_FILE,
};

const AGENT_GOLDEN: &str = include_str!("golden/agent.plist");
const SESSION_GOLDEN: &str = include_str!("golden/session.plist");

fn agent() -> Daemon {
    Daemon::agent()
}

fn session() -> Daemon {
    Daemon::session("cdhash H\"abc\" & x")
}

#[test]
fn the_agents_plist_is_the_golden_file() {
    assert_eq!(agent().render(), AGENT_GOLDEN);
}

#[test]
fn the_helpers_plist_is_the_golden_file_with_xml_characters_escaped() {
    assert_eq!(session().render(), SESSION_GOLDEN);
}

#[test]
fn the_labels_are_fixed() {
    assert_eq!(agent().label, "js-agent");
    assert_eq!(session().label, "js-agent.session");
    assert_eq!(AGENT_LABEL, "js-agent");
    assert_eq!(SESSION_LABEL, "js-agent.session");
}

#[test]
fn both_programs_are_the_packages_own_files() {
    for (daemon, name) in [(agent(), "js-agent"), (session(), "js-mac-session")] {
        assert_eq!(daemon.program, format!("{BIN_DIR}/{name}"));
    }
    assert_eq!(BIN_DIR, "/Library/Application Support/js-agent/bin");
}

#[test]
fn the_agents_host_flags_come_from_the_flags_file_not_the_plist() {
    let text = agent().render();
    assert!(text.contains(&format!(
        "<string>--flags-file</string>\n\t\t<string>{FLAGS_FILE}</string>"
    )));
    for flag in ["--server", "--pool", "SET-THE-SERVER"] {
        assert!(!text.contains(flag), "{flag}");
    }
}

#[test]
fn a_daemon_runs_only_while_the_gate_exists_and_not_at_load() {
    for daemon in [agent(), session()] {
        let text = daemon.render();
        assert!(text.contains("<key>RunAtLoad</key>\n\t<false/>"), "{text}");
        assert!(
            text.contains(&format!(
                "<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>\n\t\t\t<key>{GATE}</key>\n\t\t\t<true/>"
            )),
            "{text}"
        );
        assert!(text.contains(&format!("<integer>{THROTTLE_SECS}</integer>")));
        assert!(text.contains("<key>UserName</key>\n\t<string>root</string>"));
    }
}

/// The daemons wait for the host's flags file, which every host has, and not for the join token,
/// which a Mac that joins by hostkeeper's host certificate does not have; the agent is still
/// told where a token would be. Catches a plist that holds a certificate-joined Mac's agent and
/// helper back for good.
#[test]
fn the_gate_is_the_flags_file_not_the_join_token() {
    assert_eq!(GATE, "/etc/js-agent/flags");
    assert_eq!(GATE, FLAGS_FILE);
    assert_eq!(TOKEN_FILE, "/etc/js-agent/token");
    for daemon in [agent(), session()] {
        let text = daemon.render();
        assert!(
            !text.contains(&format!("<key>{TOKEN_FILE}</key>")),
            "{text}"
        );
    }
    assert!(agent().render().contains(&format!(
        "<string>--token-file</string>\n\t\t<string>{TOKEN_FILE}</string>"
    )));
}

#[test]
fn the_logs_are_in_the_package_log_directory_one_pair_per_daemon() {
    let text = agent().render();
    assert!(text.contains(&format!("<string>{LOG_DIR}/js-agent.log</string>")));
    assert!(text.contains(&format!("<string>{LOG_DIR}/js-agent.err.log</string>")));
    let text = session().render();
    assert!(text.contains(&format!("<string>{LOG_DIR}/js-mac-session.log</string>")));
    assert!(text.contains(&format!(
        "<string>{LOG_DIR}/js-mac-session.err.log</string>"
    )));
}

#[test]
fn the_plist_carries_no_environment_variables() {
    for daemon in [agent(), session()] {
        assert!(!daemon.render().contains("EnvironmentVariables"));
    }
}
