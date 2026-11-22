//! The profile text: the two kinds, the job folder by parameter, the network only as named.

use super::{JOB_PARAM, Kind, render};
use crate::mac_job::spec::Network;

const KINDS: [Kind; 2] = [Kind::ConfinedWrites, Kind::DenyDefault];

/// The lines of `text` that grant or deny file writes.
fn write_rules(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.contains("file-write")).collect()
}

#[test]
fn confined_writes_allows_by_default_and_writes_only_in_the_job_folder() {
    let text = render(Kind::ConfinedWrites, &Network::default(), "");
    assert!(text.starts_with("(version 1)\n(allow default)\n"));
    assert_eq!(
        write_rules(&text),
        [
            "(deny file-write*)",
            "(allow file-write* (subpath (param \"JOB\"))",
            "(deny file-read* file-write* (subpath \"/private/var/protected/sfanalytics\") (subpath \"/private/var/protected/trustd/private\"))",
        ]
    );
    for denied in [
        "(deny user-preference-write)",
        "(deny job-creation)",
        "(deny lsopen)",
        "(deny mach-register)",
        "(deny file-mount)",
        "(deny ipc-posix-shm-write-create)",
        "(deny network*)",
    ] {
        assert!(text.lines().any(|l| l == denied), "{denied}");
    }
}

#[test]
fn deny_default_denies_by_default_and_writes_only_in_the_job_folder() {
    let text = render(Kind::DenyDefault, &Network::default(), "");
    assert!(text.starts_with("(version 1)\n(deny default)\n"));
    assert_eq!(JOB_PARAM, "JOB");
    assert_eq!(
        write_rules(&text),
        ["(allow file-write* (subpath (param \"JOB\"))"]
    );
    for absent in ["user-preference-write", "mach-register", "ipc-posix-sem"] {
        assert!(!text.contains(absent), "{absent}");
    }
}

#[test]
fn no_network_adds_no_network_rule() {
    for kind in KINDS {
        let text = render(kind, &Network::default(), "");
        for absent in [
            "network-bind",
            "remote tcp",
            "dnssd",
            "mDNSResponder",
            "added for this run",
        ] {
            assert!(!text.contains(absent), "{kind:?} {absent}");
        }
    }
}

#[test]
fn loopback_allows_localhost_only() {
    for kind in KINDS {
        let text = render(
            kind,
            &Network {
                loopback: true,
                outbound_ports: Vec::new(),
            },
            "",
        );
        assert!(text.ends_with(
            "(allow network-bind network-inbound (local ip \"localhost:*\"))\n\
             (allow network-outbound (remote ip \"localhost:*\"))\n"
        ));
        assert!(!text.contains("remote tcp"));
    }
}

#[test]
fn outbound_ports_allow_name_lookups_and_each_port() {
    for kind in KINDS {
        let text = render(
            kind,
            &Network {
                loopback: false,
                outbound_ports: vec![8980, 443],
            },
            "",
        );
        assert!(
            text.ends_with(
                "(allow mach-lookup (global-name \"com.apple.dnssd.service\"))\n\
             (allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n\
             (allow network-outbound (remote tcp \"*:8980\"))\n\
             (allow network-outbound (remote tcp \"*:443\"))\n"
            ),
            "{kind:?}"
        );
        assert!(!text.contains("localhost:*"));
    }
}

#[test]
fn added_rules_come_last_each_ended_by_a_newline() {
    for kind in KINDS {
        let base = render(kind, &Network::default(), "");
        let want = format!("{base}; added for this run\n(allow a)\n");
        assert_eq!(render(kind, &Network::default(), "(allow a)"), want);
        assert_eq!(render(kind, &Network::default(), "(allow a)\n"), want);
    }
}
