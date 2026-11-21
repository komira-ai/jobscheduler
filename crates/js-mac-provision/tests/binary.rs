//! The program as a process: the exit codes, what goes to which stream, and the verbs
//! that need no root (`render`, `launchd-plist`, `probe`), plus the ones that fail
//! cleanly when `pfctl` is not there. No test here needs a Mac.

use std::net::TcpListener;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_js-mac-provision");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run the program")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

#[test]
fn render_prints_the_anchor_for_a_policy() {
    let out = run(&[
        "render",
        "--uid-start",
        "600",
        "--uid-count",
        "3",
        "--endpoint",
        "198.51.100.7:8443",
        "--endpoint",
        "192.0.2.10:443",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let golden = "\
# js-mac-provision: egress of pool uids 600-602 (anchor com.apple/com.komira.js.pool). Generated; do not edit.
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user { 600, 601, 602 }
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user { 600, 601, 602 } keep state
pass out quick inet proto tcp from any to 192.0.2.10 port 443 user { 600, 601, 602 } keep state
pass out quick inet proto tcp from any to 198.51.100.7 port 8443 user { 600, 601, 602 } keep state
block return out quick inet6 proto { tcp, udp } from any to any user { 600, 601, 602 }
block return out quick proto { tcp, udp } from any to any user { 600, 601, 602 }
";
    assert_eq!(text(&out.stdout), golden);
    assert!(out.stderr.is_empty());
}

#[test]
fn a_refused_policy_exits_1_and_names_why_on_stderr() {
    for (args, why) in [
        (
            vec!["render", "--uid-start", "600", "--uid-count", "0"],
            "uid range is empty",
        ),
        (
            vec!["render", "--uid-start", "499", "--uid-count", "5"],
            "below 500",
        ),
        (
            vec![
                "render",
                "--uid-start",
                "600",
                "--uid-count",
                "5",
                "--endpoint",
                "[2001:db8::1]:443",
            ],
            "not an IPv4",
        ),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty());
        let err = text(&out.stderr);
        assert!(err.starts_with("error: "), "{err}");
        assert!(err.contains(why), "{err}");
    }
}

#[test]
fn a_usage_error_exits_2_and_help_exits_0() {
    let out = run(&["render"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("--uid-start"));
    let help = run(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(text(&help.stdout).contains("launchd-plist"));
}

#[test]
fn a_missing_pfctl_fails_install_check_and_remove_with_its_name() {
    for verb in ["install", "check"] {
        let dir = std::env::temp_dir().join(format!(
            "js-mac-provision-bin-{verb}-{}",
            std::process::id()
        ));
        let out = run(&[
            verb,
            "--uid-start",
            "600",
            "--uid-count",
            "10",
            "--pfctl",
            "/nonexistent/pfctl",
            "--state-dir",
            dir.to_str().unwrap(),
        ]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.status.code(), Some(1), "{verb}");
        let err = text(&out.stderr);
        assert!(err.contains("could not run pfctl"), "{err}");
    }
    let out = run(&["remove", "--pfctl", "/nonexistent/pfctl"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("-a com.apple/com.komira.js.pool -F rules"));
}

#[test]
fn probe_exits_0_for_a_connect_and_1_for_a_refusal() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let out = run(&["probe", &addr]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "connected\n");
    // A port nothing can take: dropping `l` would free it for any other test to bind.
    let dead = js_testkit::port::dead_addr().to_string();
    let out = run(&["probe", &dead]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "refused\n");
}

#[test]
fn launchd_plist_prints_a_property_list() {
    let out = run(&[
        "launchd-plist",
        "--uid-start",
        "600",
        "--uid-count",
        "100",
        "--program",
        "/usr/local/bin/js-mac-provision",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let plist = text(&out.stdout);
    assert!(plist.starts_with("<?xml"));
    assert!(plist.contains("<string>js-agent.pf-pool</string>"));
    assert!(plist.contains("<integer>30</integer>"));
}

#[test]
fn pf_pool_plist_prints_the_packages_fixed_property_list() {
    let out = run(&["pf-pool-plist"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), include_str!("golden/pf-pool.plist"));
}

/// The pf daemon runs every 30 seconds from the package's install on, before the host's flags file
/// arrives: a run with no flags file, or with a flags file that names no anchor, succeeds and
/// runs no `pfctl` (the one named here is not there). A refused flags file exits 1.
#[test]
fn install_from_an_absent_or_off_flags_file_exits_0_and_a_refused_one_exits_1() {
    let dir =
        std::env::temp_dir().join(format!("js-mac-provision-bin-flags-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let flags = dir.join("flags");
    let state = dir.join("state");
    // As the agent's own: not writable by group or others, whatever the umask.
    let write = |text: &str| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(&flags, text).unwrap();
        std::fs::set_permissions(&flags, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    let install = |pfctl: &str| {
        run(&[
            "install",
            "--pfctl",
            pfctl,
            "--state-dir",
            state.to_str().unwrap(),
            "--flags-file",
            flags.to_str().unwrap(),
        ])
    };
    let out = install("/nonexistent/pfctl");
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).ends_with(": nothing to install\n"));
    write("--native-network=off\n");
    let out = install("/nonexistent/pfctl");
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("--native-network=off"));
    write("--native-network=on\n");
    let out = install("/nonexistent/pfctl");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("--native-pf-uid-start: required with --native-network on"));
    write(&format!(
        "--native-network=on\n--native-pf-uid-start=600\n--native-pf-uid-count=1\n\
             --native-pfctl=/nonexistent/pfctl\n--native-pf-state-dir={}\n",
        state.display()
    ));
    let out = install("/nonexistent/pfctl");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("could not run pfctl"));
}
