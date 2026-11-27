//! The released js-agent beside the released tierstore, driven from outside, on a disposable
//! Linux runner, as root.
//!
//! Each test installs `js-agent` at the version under test from the apt suite `testing` and
//! `tierstore` at its newest release from the suite `stable` (packages.komira.ai), starts both by
//! their packaged systemd units, and starts the lease server's released binary beside them. The
//! agent's flags have the shape the farm's Linux nodes commit, and tierstore's file the shape of
//! the farm's build-node file, with `trees.max_placements = 2` (see [`agent`] and
//! [`tierstore`]). tierstore's upstream store is an S3 server in a container of the workflow's,
//! reached through a TCP proxy the test can stall ([`stall`]); the agent reaches tierstore's mount
//! provider through a proxy on its socket that records every call it passes on
//! ([`provider`]). Jobs are submitted to the lease server's job service, each with one input tree
//! from tierstore, and read back from it.
//!
//! The pass conditions are numbers, the plan's:
//!
//! - [`twenty_starts_at_two_placements_with_the_upstream_stalled`]: in no 30 s of the run do more
//!   than 20 Prepare calls reach tierstore; every job ends, after the Releases; and no job ends
//!   as its own fault (an end other than success, or a start that failed for the host's input).
//! - [`tierstore_restarted_while_starts_wait`]: after the restart every waiting start is
//!   prepared again (at least one Prepare per job reaches the new process) and every job
//!   succeeds.
//! - [`tierstore_unavailable_for_60_s`]: for 60 s tierstore answers the waiting starts
//!   unavailable (its own count of unavailable answers rises), and every job succeeds.
//!
//! Durations are deadlines that end a hang, and the windows the plan names; a slower runner sends
//! fewer calls in a window, never more, so no test passes or fails on how fast the runner is.
//!
//! What the test is given comes from the environment, as libtest takes no flags of its own (the
//! workflow sets each): `JS_BLACKBOX_AGENT_VERSION` (the js-agent version on `testing`),
//! `JS_BLACKBOX_SERVER_BIN` (the released `js-lease-server` binary of that version),
//! `JS_BLACKBOX_S3` (the upstream store's `http://127.0.0.1:<port>`) and
//! `JS_BLACKBOX_S3_KEY_FILE` (a file of two lines, the store's access key id and secret). A
//! test that is not root, or lacks any of them, fails saying so: it never skips.

#![cfg(target_os = "linux")]

#[path = "agent_beside_tierstore/agent.rs"]
mod agent;
#[path = "agent_beside_tierstore/host.rs"]
mod host;
#[path = "agent_beside_tierstore/http.rs"]
mod http;
#[path = "agent_beside_tierstore/identity.rs"]
mod identity;
#[path = "agent_beside_tierstore/provider.rs"]
mod provider;
#[path = "agent_beside_tierstore/rig.rs"]
mod rig;
#[path = "agent_beside_tierstore/server.rs"]
mod server;
#[path = "agent_beside_tierstore/stall.rs"]
mod stall;
#[path = "agent_beside_tierstore/tierstore.rs"]
mod tierstore;
#[path = "agent_beside_tierstore/upstream.rs"]
mod upstream;

use std::time::{Duration, Instant};

use rig::{End, Rig};

/// The window the plan counts Prepare calls in.
const WINDOW: Duration = Duration::from_secs(30);

/// The most Prepare calls that may reach tierstore in any [`WINDOW`]: one per job of the burst.
const MOST_PREPARES_IN_WINDOW: usize = 20;

/// The burst of the first test.
const BURST: usize = 20;

/// The jobs of the restart and outage tests: three times the placements tierstore may hold.
const WAITING: usize = 6;

/// How long tierstore answers unavailable in the outage test.
const OUTAGE: Duration = Duration::from_secs(60);

/// Catches an agent that sends Prepare again at once when tierstore cannot serve a start (the
/// retry storm of a provider whose placements are full, about 1,000 calls a second): 20 starts
/// wait on a tierstore whose upstream is stalled for 30 s and that then holds only two
/// placements at a time, and the most Prepare calls in any 30 s is counted at the provider's
/// socket. It also catches a store refusal that fails a start as the job's own fault, and a job
/// left waiting when the Releases free the placements.
#[test]
#[ignore = "needs root on a disposable Linux runner, the apt channel and the released lease server: run by js-agent-blackbox-tierstore.yml"]
fn twenty_starts_at_two_placements_with_the_upstream_stalled() {
    let rig = Rig::up("burst");
    rig.stall.hold();
    let jobs = rig.submit("burst", BURST);
    let first = rig.provider.wait_for_calls("Prepare", 1);
    rig.wait_until(first + WINDOW);
    rig.stall.release();
    let ends = rig.wait_for_ends(&jobs);
    let releases = rig.provider.wait_for_calls("Release", BURST);

    let prepares = rig.provider.reached("Prepare");
    let busiest = rig::busiest_window(&prepares, WINDOW);
    let mut wrong = Vec::new();
    if busiest.count > MOST_PREPARES_IN_WINDOW {
        wrong.push(format!(
            "{} Prepare calls reached tierstore in the 30 s from {:.1} s, more than {MOST_PREPARES_IN_WINDOW}",
            busiest.count,
            busiest.from.as_secs_f64()
        ));
    }
    wrong.extend(own_faults(&ends));
    let report = rig.report(&ends, &prepares, releases);
    assert!(wrong.is_empty(), "{}\n\n{report}", wrong.join("\n"));
    println!("{report}");
}

/// Catches an agent that, when tierstore restarts and forgets the starts it was preparing, waits
/// on a Mount that can never be served instead of sending Prepare again: six starts wait on a
/// stalled upstream, tierstore restarts, the upstream comes back, and every job must succeed
/// with at least one Prepare per job reaching the new process.
#[test]
#[ignore = "needs root on a disposable Linux runner, the apt channel and the released lease server: run by js-agent-blackbox-tierstore.yml"]
fn tierstore_restarted_while_starts_wait() {
    let rig = Rig::up("restart");
    rig.stall.hold();
    let jobs = rig.submit("restart", WAITING);
    rig.provider.wait_for_calls("Prepare", WAITING);
    rig.tierstore.restart();
    let restarted = Instant::now();
    rig.stall.release();
    let ends = rig.wait_for_ends(&jobs);
    let releases = rig.provider.wait_for_calls("Release", WAITING);

    let prepares = rig.provider.reached("Prepare");
    let again = prepares.iter().filter(|at| **at >= restarted).count();
    let mut wrong = Vec::new();
    if again < WAITING {
        wrong.push(format!(
            "{again} Prepare calls reached tierstore after its restart, fewer than the {WAITING} starts that waited"
        ));
    }
    wrong.extend(not_succeeded(&ends));
    let report = rig.report(&ends, &prepares, releases);
    assert!(wrong.is_empty(), "{}\n\n{report}", wrong.join("\n"));
    println!("{report}");
}

/// Catches a start that fails while tierstore answers unavailable for a minute: six starts wait
/// while the upstream is stalled for 60 s, through which tierstore answers their Mounts
/// unavailable, and every job must then succeed.
#[test]
#[ignore = "needs root on a disposable Linux runner, the apt channel and the released lease server: run by js-agent-blackbox-tierstore.yml"]
fn tierstore_unavailable_for_60_s() {
    let rig = Rig::up("outage");
    rig.stall.hold();
    let jobs = rig.submit("outage", WAITING);
    let first = rig.provider.wait_for_calls("Prepare", 1);
    let before = rig.tierstore.unavailable_answers();
    rig.wait_until(first + OUTAGE);
    let during = rig.tierstore.unavailable_answers() - before;
    rig.stall.release();
    let ends = rig.wait_for_ends(&jobs);
    let releases = rig.provider.wait_for_calls("Release", WAITING);

    let prepares = rig.provider.reached("Prepare");
    let mut wrong = Vec::new();
    if during == 0 {
        wrong.push(
            "tierstore answered no call unavailable while its upstream was stalled: the outage was not one"
                .to_owned(),
        );
    }
    wrong.extend(not_succeeded(&ends));
    let report = rig.report(&ends, &prepares, releases);
    assert!(
        wrong.is_empty(),
        "{}\n\n{during} unavailable answers in the outage\n{report}",
        wrong.join("\n")
    );
    println!("{during} unavailable answers in the outage\n{report}");
}

/// The jobs that ended as their own fault: every end but a success and a start that failed
/// because the host's input was unavailable (the host's fault, which the job service may retry
/// elsewhere).
fn own_faults(ends: &[End]) -> Vec<String> {
    ends.iter()
        .filter(|end| !end.succeeded() && !end.input_unavailable())
        .map(|end| format!("{} ended as its own fault: {}", end.id, end.text()))
        .collect()
}

/// The jobs that did not succeed.
fn not_succeeded(ends: &[End]) -> Vec<String> {
    ends.iter()
        .filter(|end| !end.succeeded())
        .map(|end| format!("{} did not succeed: {}", end.id, end.text()))
        .collect()
}
