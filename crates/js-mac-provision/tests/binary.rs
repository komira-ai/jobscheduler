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
        "100",
        "--endpoint",
        "198.51.100.7:8443",
        "--endpoint",
        "192.0.2.10:443",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let golden = "\
# js-mac-provision: egress of pool uids 600-699 (anchor com.apple/com.komira.js.pool). Generated; do not edit.
block return out quick inet proto { tcp, udp } from any to 169.254.0.0/16 user 600:699
pass out quick on lo0 inet proto { tcp, udp } from 127.0.0.0/8 to 127.0.0.0/8 user 600:699 keep state
pass out quick inet proto tcp from any to 192.0.2.10 port 443 user 600:699 keep state
pass out quick inet proto tcp from any to 198.51.100.7 port 8443 user 600:699 keep state
block return out quick inet6 proto { tcp, udp } from any to any user 600:699
block return out quick proto { tcp, udp } from any to any user 600:699
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
    drop(l);
    let out = run(&["probe", &addr]);
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
    assert!(plist.contains("<string>com.komira.js.pf-pool</string>"));
    assert!(plist.contains("<integer>300</integer>"));
}
