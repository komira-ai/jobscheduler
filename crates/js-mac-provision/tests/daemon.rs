//! The property lists of the package's two LaunchDaemons, against golden files, and the
//! rules every one of them keeps.

use js_mac_provision::daemon::{
    AGENT_LABEL, Daemon, GATE, LOG_DIR, RELEASE_LINK, SESSION_LABEL, THROTTLE_SECS,
};

const AGENT_GOLDEN: &str = include_str!("golden/agent.plist");
const SESSION_GOLDEN: &str = include_str!("golden/session.plist");

fn agent() -> Daemon {
    Daemon::agent("https://leases.example:7300", "mac")
}

fn session() -> Daemon {
    Daemon::session("cdhash H\"abc\" & x", "_jsagent")
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
    assert_eq!(agent().label, "com.komira.js-agent");
    assert_eq!(session().label, "com.komira.js-mac-session");
    assert_eq!(AGENT_LABEL, "com.komira.js-agent");
    assert_eq!(SESSION_LABEL, "com.komira.js-mac-session");
}

#[test]
fn both_programs_run_through_the_release_link_the_update_hook_moves() {
    for (daemon, name) in [(agent(), "js-agent"), (session(), "js-mac-session")] {
        assert_eq!(daemon.program, format!("{RELEASE_LINK}/{name}"));
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
