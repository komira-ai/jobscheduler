//! The property lists of the package's two LaunchDaemons, against golden files, and the rules
//! they keep.

use js_mac_provision::daemon::{
    AGENT_LABEL, BIN_DIR, Daemon, FLAGS_FILE, GATE, LOG_DIR, PF_ANCHOR_VERB, PF_POOL_INTERVAL_SECS,
    PF_POOL_LABEL, THROTTLE_SECS, TOKEN_FILE, pf_pool_plist,
};

const AGENT_GOLDEN: &str = include_str!("golden/agent.plist");
const PF_POOL_GOLDEN: &str = include_str!("golden/pf-pool.plist");

fn agent() -> Daemon {
    Daemon::agent()
}

#[test]
fn the_agents_plist_is_the_golden_file() {
    assert_eq!(agent().render(), AGENT_GOLDEN);
}

#[test]
fn the_pf_daemons_plist_is_the_golden_file() {
    assert_eq!(pf_pool_plist(), PF_POOL_GOLDEN);
}

/// The pf daemon runs the agent's `__pf-anchor --flags-file` with the agent's own flags file, at load and every 30
/// seconds, and its label is one of the js-agent service's (`js-agent.` and more), which hostkeeper
/// requires of a label a package runs. Catches a plist that writes a policy of its own, so that the
/// anchor loaded and the anchor the agent checks could differ, or a label hostkeeper refuses.
#[test]
fn the_pf_daemon_reads_the_agents_flags_file_at_load_and_every_interval() {
    let text = pf_pool_plist();
    assert_eq!(PF_POOL_LABEL, "js-agent.pf-pool");
    assert!(PF_POOL_LABEL.starts_with(&format!("{AGENT_LABEL}.")));
    assert!(text.contains(&format!("<string>{PF_POOL_LABEL}</string>")));
    assert!(text.contains(&format!(
        "<string>{BIN_DIR}/js-agent</string>\n\t\t<string>{PF_ANCHOR_VERB}</string>\n\t\t\
         <string>--flags-file</string>\n\t\t<string>{FLAGS_FILE}</string>\n\t</array>"
    )));
    assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"));
    assert_eq!(PF_POOL_INTERVAL_SECS, 30);
    assert!(text.contains("<key>StartInterval</key>\n\t<integer>30</integer>"));
    for word in [
        "--uid-start",
        "--endpoint",
        "EnvironmentVariables",
        "UserName",
    ] {
        assert!(!text.contains(word), "{word}");
    }
}

#[test]
fn the_labels_are_fixed() {
    assert_eq!(agent().label, "js-agent");
    assert_eq!(AGENT_LABEL, "js-agent");
    assert_eq!(PF_POOL_LABEL, "js-agent.pf-pool");
}

/// Both daemons run the package's one program, the agent: the pf daemon through its hidden
/// verb, which is the name `js-agent` knows it by.
#[test]
fn both_daemons_run_the_agent_program_in_the_package_bin_directory() {
    assert_eq!(agent().program, format!("{BIN_DIR}/js-agent"));
    assert_eq!(BIN_DIR, "/Library/Application Support/js-agent/bin");
    assert_eq!(PF_ANCHOR_VERB, "__pf-anchor");
    let text = pf_pool_plist();
    assert!(text.contains(&format!("<string>{BIN_DIR}/js-agent</string>")));
    assert!(!text.contains("js-mac-"), "{text}");
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
fn the_agent_runs_only_while_the_gate_exists_and_not_at_load() {
    let text = agent().render();
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

/// The agent waits for the host's flags file, which every host has, and not for the join token,
/// which a Mac that joins by hostkeeper's host certificate does not have; the agent is still
/// told where a token would be. Catches a plist that holds a certificate-joined Mac's agent
/// back for good.
#[test]
fn the_gate_is_the_flags_file_not_the_join_token() {
    assert_eq!(GATE, "/etc/js-agent/flags");
    assert_eq!(GATE, FLAGS_FILE);
    assert_eq!(TOKEN_FILE, "/etc/js-agent/token");
    let text = agent().render();
    assert!(
        !text.contains(&format!("<key>{TOKEN_FILE}</key>")),
        "{text}"
    );
    assert!(agent().render().contains(&format!(
        "<string>--token-file</string>\n\t\t<string>{TOKEN_FILE}</string>"
    )));
}

#[test]
fn the_agents_logs_are_in_the_package_log_directory() {
    let text = agent().render();
    assert!(text.contains(&format!("<string>{LOG_DIR}/js-agent.log</string>")));
    assert!(text.contains(&format!("<string>{LOG_DIR}/js-agent.err.log</string>")));
}

/// The pf daemon's error output goes to its own file in `/var/log`, named for its label.
#[test]
fn the_pf_daemons_error_log_is_its_own_file_in_var_log() {
    assert!(pf_pool_plist().contains(
        "<key>StandardErrorPath</key>\n\t<string>/var/log/js-agent.pf-pool.log</string>"
    ));
}

#[test]
fn the_plists_carry_no_environment_variables() {
    assert!(!agent().render().contains("EnvironmentVariables"));
    assert!(!pf_pool_plist().contains("EnvironmentVariables"));
}

/// The `NumberOfFiles` integer of the `key` dictionary (`SoftResourceLimits` or
/// `HardResourceLimits`) of the plist `text`, or `None` when the plist sets none.
fn number_of_files(text: &str, key: &str) -> Option<u64> {
    let start =
        format!("\t<key>{key}</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>");
    let rest = &text[text.find(&start)? + start.len()..];
    let (number, after) = rest.split_once("</integer>")?;
    after
        .starts_with("\n\t</dict>\n")
        .then(|| number.parse().ok())?
}

/// The agent starts with 65536 open files, soft and hard, and not with launchd's default soft
/// limit of 256: it serves many runs at once, each with pipes, log streams, lease files and
/// connections. Catches a plist that
/// sets no `SoftResourceLimits`, or a soft limit above its hard one, which launchd refuses.
#[test]
fn the_agent_starts_with_65536_open_files_soft_and_hard() {
    let text = agent().render();
    let soft = number_of_files(&text, "SoftResourceLimits");
    let hard = number_of_files(&text, "HardResourceLimits");
    assert_eq!(soft, Some(65536), "{text}");
    assert_eq!(hard, Some(65536), "{text}");
    assert!(soft > Some(256) && soft <= hard, "{text}");
}

/// The check above reads a limit only where the plist sets one: a plist with no resource limits,
/// as the pf daemon's is (it runs once and exits), reads as none,
/// and a malformed dictionary reads as none too, so the test above cannot pass on either.
#[test]
fn a_plist_without_resource_limits_reads_as_none() {
    let pf = pf_pool_plist();
    assert_eq!(number_of_files(&pf, "SoftResourceLimits"), None);
    assert_eq!(number_of_files(&pf, "HardResourceLimits"), None);
    let unclosed = "\t<key>SoftResourceLimits</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>65536</integer>\n\t<key>Label</key>\n";
    assert_eq!(number_of_files(unclosed, "SoftResourceLimits"), None);
    let not_a_number = "\t<key>SoftResourceLimits</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>many</integer>\n\t</dict>\n";
    assert_eq!(number_of_files(not_a_number, "SoftResourceLimits"), None);
    let unterminated = "\t<key>SoftResourceLimits</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>65536";
    assert_eq!(number_of_files(unterminated, "SoftResourceLimits"), None);
}
