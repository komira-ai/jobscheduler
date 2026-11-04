//! The egress flags of an agent started with `--runtime crun`: the runtime takes them, and an
//! allow-list the host cannot set up ends the agent before it registers. The node and the
//! stand-in `crun` are `crun_e2e`'s.

use std::sync::{Arc, Mutex};

use js_agent::crun_host::flags;

use super::crun_e2e::{CrunMachine, NODE_IN_USE, crun_flags, host};
use crate::crun_host::{Fixture, cli};
use crate::server::{Agent, POOL_RUNS_TOML, Redirect, Server};
use crate::support::code;

/// Asserts that `--runtime crun` takes the egress flags: `--egress none` and an allow-list
/// pass the runtime's own flag check (the allow-list's flags are checked, and its firewall
/// set up, later in the agent's start). It catches an allow-list still refused as a flag the
/// crun runtime does not take, which would leave a crun job no way to reach anything.
#[test]
fn crun_accepts_egress_allow_list() {
    let ok = [
        "--runtime",
        "crun",
        "--job-user",
        Fixture::USER,
        "--state-root",
        "/s",
        "--scratch-root",
        "/r",
    ];
    let allow_list = ["--egress", "allow-list", "--egress-allow", "192.0.2.10:443"];
    for egress in [&["--egress", "none"][..], &allow_list[..]] {
        let mut all = ok.to_vec();
        all.extend_from_slice(egress);
        assert!(flags(&cli(&all)).is_ok(), "{egress:?}");
    }
}

/// Asserts that a crun agent given an allow-list it cannot set up ends with status 2 before
/// it registers, naming the flag and the step that failed: here the `ip` program it is given
/// fails, so the first step of the start (listing the interfaces an earlier agent left)
/// does. It catches an agent that would run its jobs with a firewall it never loaded, or
/// with no network at all, when the operator asked for an allow-list.
#[test]
fn an_allow_list_that_cannot_be_set_up_ends_the_crun_agent_with_status_2() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::start_pool("jobs_crun_egress", &[], POOL_RUNS_TOML);
    let address = Arc::new(Mutex::new(server.url()));
    let allow_list = [
        "--egress",
        "allow-list",
        "--egress-allow",
        "192.0.2.10:443",
        "--egress-ip",
        "/bin/false",
        "--egress-nft",
        "/bin/false",
        "--egress-nsenter",
        "/bin/false",
    ];
    let flags = crun_flags("h-egress", &allow_list);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let ran = Agent::spawn_machine(
        server.url(),
        server.join_file(),
        &flags,
        host,
        Redirect(address),
        CrunMachine::new(),
    )
    .finish();
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert!(
        ran.err
            .contains("--egress allow-list: egress: listing the interfaces: /bin/false"),
        "{}",
        ran.err
    );
}
