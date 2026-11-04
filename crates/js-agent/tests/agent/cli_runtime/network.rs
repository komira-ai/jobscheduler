//! The network of a job run through the container engine (Docker or Podman): always none.
//! The engines have no network setting and no firewall read-back; a job that needs a network
//! runs on the `crun` runtime, which wires one per job.

use js_agent::lifecycle::{Lifecycle, NoChecks, Runtime};
use js_agent_wire::Command;

use super::scripted::{DOCKER, Scripted};
use super::{made_at, open_at};
use crate::lifecycle::fake::ManualClock;
use crate::lifecycle::{config_in, job, names, reply};
use crate::support::Scratch;

const PODMAN: &str = "podman version 4.9.3\n";

/// Asserts, for Docker and for Podman, over the whole log of the calls the runtime makes,
/// from its open, through the check the agent makes when it starts, to two jobs started by
/// the lifecycle: every `create` carries exactly one `--network`, and it is `none`; no other
/// call names a network; and no call is a network or firewall read-back (`network inspect`,
/// an `nft` listing of a table) or turns IPv6 off for a named network. It catches a code
/// path that still takes a network name, and the old check run at start or before a job.
#[test]
fn a_fallback_runtime_job_gets_network_none() {
    for (n, version) in [DOCKER, PODMAN].into_iter().enumerate() {
        let dir = Scratch::new(&format!("cli-network-none-{n}"));
        let world = Scripted::new(version);
        let runtime = open_at(&world, "linux", made_at);
        // What the worker asks of the runtime when the agent starts.
        runtime.check_egress().expect("nothing to check");
        let (mut life, _) = Lifecycle::adopt(
            config_in(&dir, false),
            Box::new(runtime),
            Box::new(NoChecks),
            Box::new(ManualClock::default()),
        )
        .unwrap();
        for run_id in ["r1", "r2"] {
            let events = life.apply(&reply(vec![Command::Start(Box::new(job(run_id)))]));
            assert_eq!(names(&events), [format!("started {run_id}")], "{version}");
        }
        let calls = world.calls();
        let creates = calls.iter().filter(|args| args[0] == "create").count();
        assert_eq!(creates, 2, "{version}: {calls:?}");
        for args in &calls {
            let networks: Vec<&str> = args
                .windows(2)
                .filter(|pair| pair[0] == "--network")
                .map(|pair| pair[1].as_str())
                .collect();
            let want: &[&str] = match args[0].as_str() {
                "create" => &["none"],
                _ => &[],
            };
            assert_eq!(networks, want, "{version}: {args:?}");
            let named = |word: &String| {
                matches!(word.as_str(), "network" | "nft" | "-j" | "table")
                    || word.starts_with("--network=")
                    || word.contains("js_egress")
                    || word.contains("disable_ipv6")
            };
            assert!(!args.iter().any(named), "{version}: {args:?}");
        }
    }
}
