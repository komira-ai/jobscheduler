//! Duty 6 (read side): the egress check. Before every contained job the agent runs `nft -j
//! list table inet js_egress` and refuses the job unless the rules are the expected ones.
//! Reading the rules is a netlink request to the kernel's netfilter, which the kernel
//! answers only to a process holding `CAP_NET_ADMIN` in the host's network namespace.

use std::process::Command;

use crate::harness::{Kv, cap, kv, require_root, run_agent};

/// The program the agent's `--container-nft` names by default.
const NFT: &str = "/usr/sbin/nft";

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "nft_list" => {
            let out = Command::new(NFT).args(["-j", "list", "tables"]).output();
            let said = match out {
                Ok(out) if out.status.success() => "ok".to_owned(),
                Ok(out) => format!("err:{}", String::from_utf8_lossy(&out.stderr)),
                Err(why) => format!("spawn:{why}"),
            };
            Some(vec![kv("nft", said)])
        }
        _ => None,
    }
}

/// A: a non-root agent without `CAP_NET_ADMIN` cannot even READ the firewall (`nft list`
/// fails with "Operation not permitted"), so the agent's check before every job could never
/// pass: under shape A the check needs the capability, or a root helper that makes the
/// check and answers for it.
#[test]
#[ignore = "needs root and nft: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_without_cap_net_admin_cannot_read_the_firewall() {
    let report = run_agent(&[], "nft_list", &[]);
    report.expect_prefix("nft", "err:");
    assert!(
        report.get("nft").contains("Operation not permitted"),
        "{}",
        report.get("nft")
    );
}

/// A: `CAP_NET_ADMIN` alone lets the agent read the firewall. (It also lets it change it,
/// which is why a job-facing agent that holds it is not a small privilege.)
#[test]
#[ignore = "needs root and nft: see the module docs of tests/privilege/main.rs"]
fn cap_net_admin_alone_reads_the_firewall() {
    let report = run_agent(&[cap::NET_ADMIN], "nft_list", &[]);
    report.expect("nft", "ok");
}

/// B: a root agent reads the firewall as it does today.
#[test]
#[ignore = "needs root and nft: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_reads_the_firewall() {
    require_root();
    let out = Command::new(NFT)
        .args(["-j", "list", "tables"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
