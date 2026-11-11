//! `--host-port` reaches the crun runtime the agent opens.

use js_agent_wire::PortRange;

use super::crun_e2e::{NODE_IN_USE, Rig};
use crate::support::code;

/// The ranges of `entries`, each `PORT` or `LO-HI`.
fn ranges(entries: &[&str]) -> Vec<PortRange> {
    let parsed = entries.iter().map(|entry| PortRange::parse(entry));
    parsed.collect::<Option<_>>().expect("ranges")
}

/// Asserts that `--host-port` reaches the crun runtime: an agent started with
/// `--host-port 9000-9010 --host-port 5432 --host-port 5432` opens its runtime from crun
/// settings whose host ports are `5432` and `9000-9010` (sorted, each once), and the runtime
/// opened from them allows exactly those (what the admission holds a start to); one started
/// without the flag opens a runtime that allows none. It catches a flag that is checked and
/// never handed to the runtime, and settings the runtime does not read.
#[test]
fn the_host_ports_reach_the_crun_runtime() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let given = [
        "--host-port",
        "9000-9010",
        "--host-port",
        "5432",
        "--host-port",
        "5432",
    ];
    for (extra, want) in [
        (&given[..], ranges(&["5432", "9000-9010"])),
        (&[][..], Vec::new()),
    ] {
        let rig = Rig::new("jobs_crun_host_ports", extra);
        let seen = rig.machine.host_ports.lock().expect("host ports").clone();
        assert_eq!(seen, [(want.clone(), want)], "{extra:?}");
        let ran = rig.agent.stop();
        assert_eq!(ran.status(), code(0), "{}", ran.err);
    }
}
