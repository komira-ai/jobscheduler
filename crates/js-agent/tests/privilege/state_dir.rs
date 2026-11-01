//! Duty 5: the state directory (`--state-root`): the taint marker and what must outlive a
//! restart. The agent refuses a directory it does not own or that anyone else can enter, so
//! the owner is whoever the agent runs as.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use js_agent::lifecycle::StateRoot;

use crate::harness::{AGENT, Kv, arg, kv, make_dir, require_root, run_agent, verdict, work_dir};

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "state_open" => {
            let opened = StateRoot::open(&arg("DIR"));
            Some(vec![kv("open", verdict(&opened))])
        }
        _ => None,
    }
}

/// A: a non-root agent refuses a state directory root owns (the one an agent that ran as
/// root has made), naming the owner as the reason. This is the migration step the move to
/// shape A needs: the directory is handed to the agent's user first (a unit's
/// `StateDirectory=` does this), or the agent starts with a fresh one.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_refuses_the_state_dir_of_a_root_agent() {
    let work = work_dir("priv-state-a-refuse");
    let dir = work.path().join("state");
    make_dir(&dir, 0o700, 0);
    let report = run_agent(&[], "state_open", &[("DIR", &dir)]);
    report.expect_prefix(
        "open",
        "err:--state-root: not owned by the user running the agent",
    );
}

/// A: the same directory handed to the agent's user (owner changed, mode 0700 kept) opens,
/// and the owner the kernel reports is the agent's. It proves the one-step migration.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_non_root_agent_opens_a_state_dir_handed_to_its_user() {
    let work = work_dir("priv-state-a-open");
    let dir = work.path().join("state");
    make_dir(&dir, 0o700, 0);
    std::os::unix::fs::lchown(&dir, Some(AGENT), Some(AGENT)).unwrap();
    let report = run_agent(&[], "state_open", &[("DIR", &dir)]);
    report.expect("open", "ok");
    assert_eq!(fs::metadata(&dir).unwrap().uid(), AGENT);
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// B: a root agent opens a root-owned 0700 state directory as it is today, and refuses one
/// the A-era agent's user owns: rolling back from A to B needs the directory handed back.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn a_root_agent_opens_a_root_owned_state_dir_and_refuses_the_agents() {
    require_root();
    let work = work_dir("priv-state-b");
    let root_owned = work.path().join("root-owned");
    make_dir(&root_owned, 0o700, 0);
    assert!(StateRoot::open(&root_owned).is_ok());
    let agent_owned = work.path().join("agent-owned");
    make_dir(&agent_owned, 0o700, AGENT);
    let why = StateRoot::open(&agent_owned).unwrap_err();
    assert!(
        why.contains("not owned by the user running the agent"),
        "{why}"
    );
}
