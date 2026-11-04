//! Duty 6 (read side): the egress check. Before every create on the `crun` runtime the
//! agent runs `nft -j list table inet js_crun_egress` (and the same for its NAT table) and
//! refuses the job unless the rules are the ones it loaded. Reading a table is a netlink
//! request to the kernel's netfilter, which the kernel answers only to a process holding
//! `CAP_NET_ADMIN` in the host's network namespace: without it the request fails with
//! "Operation not permitted" whether the table is loaded or not; with it the kernel answers,
//! with the table or with "No such file or directory" when no agent has loaded it on this
//! host (the privilege job runs no agent with `--egress`).

use std::process::{Command, Output};

use crate::harness::{Kv, cap, kv, require_root, run_agent};

/// The program the agent's `--egress-nft` names by default.
const NFT: &str = "/usr/sbin/nft";

/// The table the `crun` runtime's egress loads and reads back.
const TABLE: &str = "js_crun_egress";

/// `nft -j list table inet js_crun_egress`, as the agent's check runs it.
fn list() -> std::io::Result<Output> {
    Command::new(NFT)
        .args(["-j", "list", "table", "inet", TABLE])
        .output()
}

/// `ok` for the table, `absent` when the kernel answered that it is not loaded, otherwise
/// what failed: the kernel answered either way, so both mean the read was allowed.
fn said(out: std::io::Result<Output>) -> String {
    match out {
        Ok(out) if out.status.success() => "ok".to_owned(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            match stderr.contains("No such file or directory") {
                true => "absent".to_owned(),
                false => format!("err:{stderr}"),
            }
        }
        Err(why) => format!("spawn:{why}"),
    }
}

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "nft_list" => Some(vec![kv("nft", said(list()))]),
        _ => None,
    }
}

/// Whether the kernel answered the read: the table, or that it is not loaded.
fn answered(said: &str) -> bool {
    matches!(said, "ok" | "absent")
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
    assert!(answered(report.get("nft")), "{}", report.get("nft"));
}

/// B: a root agent reads the firewall as it does today.
#[test]
#[ignore = "needs root and nft: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_reads_the_firewall() {
    require_root();
    let read = said(list());
    assert!(answered(&read), "{read}");
}
