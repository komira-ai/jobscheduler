//! The egress flags of an agent of the crun runtime: the runtime takes them, and an
//! allow-list the host cannot set up ends the agent before it registers. The node and the
//! stand-in `crun` are `crun_e2e`'s.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use js_agent::Machine;
use js_agent::crun_host::flags;

use super::crun_e2e::{NODE_IN_USE, crun_flags, host, node};
use super::crun_machine::CrunMachine;
use crate::crun_host::{Fixture, cli};
use crate::support::{
    CountingSleeper, Lowest, Script, ScriptConnector, code, flags as agent_flags, run_on_defaulted,
    welcome,
};

/// Asserts that the crun runtime takes the egress flags: `--egress none` and an allow-list
/// pass the runtime's own flag check (the allow-list's flags are checked, and its firewall
/// set up, later in the agent's start). It catches an allow-list still refused as a flag the
/// crun runtime does not take, which would leave a crun job no way to reach anything.
#[test]
fn crun_accepts_egress_allow_list() {
    let ok = [
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
/// it registers, naming the flag and the egress step that failed, and sends nothing: here the
/// `ip` program it is given fails, so the start fails at its first step after the host's
/// egress is held (listing the interfaces an earlier agent left). The transport is scripted, so no
/// listener is needed. It catches an agent that would run its jobs with a firewall it never
/// loaded, or with no network at all, when the operator asked for an allow-list.
#[test]
fn an_allow_list_that_cannot_be_set_up_ends_the_crun_agent_with_status_2() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
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
    let given = crun_flags("h-egress", &allow_list);
    let given: Vec<&str> = given.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine: Arc<dyn Machine> = CrunMachine::new();
    let ran = run_on_defaulted(
        &agent_flags(&given),
        &host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
        (machine, None),
    );
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    // The step that fails is the listing of the interfaces, or, on a machine where an agent
    // holds the host's egress, the hold before it: a start error either way, which is what
    // this asserts (the steps are the egress module's unit tests).
    assert!(
        ran.err.contains("--egress allow-list: egress: "),
        "{}",
        ran.err
    );
    assert!(sent.borrow().is_empty(), "something was sent: {sent:?}");
}

/// The allow-list flags of an agent whose egress tools all fail.
const FAILING_TOOLS: [&str; 10] = [
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

/// Runs a crun agent with `given` as its flags against a scripted transport; its end and what
/// it sent.
fn run_crun_agent(given: &[String]) -> (crate::support::Ran, usize) {
    let given: Vec<&str> = given.iter().map(String::as_str).collect();
    let (script, sent) = Script::new(vec![welcome(1)]);
    let mut sleeper = CountingSleeper::stopping_at(1);
    let machine: Arc<dyn Machine> = CrunMachine::new();
    let ran = run_on_defaulted(
        &agent_flags(&given),
        &host(),
        &ScriptConnector::of(script),
        &mut sleeper,
        &mut Lowest,
        (machine, None),
    );
    let sent = sent.borrow().len();
    (ran, sent)
}

/// Asserts that an agent with an allow-list opens the egress store in its state root before
/// it starts the egress: the directory `egress` is there afterwards, mode 0700, though the
/// start failed. It catches an agent that keeps no record of its wiring for the next process
/// to take over.
#[test]
fn an_allow_list_agent_opens_its_egress_store_in_the_state_root() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let given = crun_flags("h-egress-store", &FAILING_TOOLS);
    let (ran, sent) = run_crun_agent(&given);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    assert_eq!(sent, 0, "something was sent");
    let dir = node().state.join("egress");
    let mode = fs::metadata(&dir)
        .expect("the store directory")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700, "{mode:o}");
}

/// Asserts that an agent with an allow-list whose state root holds a regular file named
/// `egress` ends with status 2 and the usage error naming the egress store and why it cannot
/// be opened, and sends nothing. It catches an agent that starts its egress with no store
/// when the store cannot be had, and so wires runs it cannot hand over.
#[test]
fn an_egress_store_that_cannot_be_opened_ends_the_crun_agent_with_status_2() {
    let _own = NODE_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
    let state = node()
        .state
        .parent()
        .expect("a parent")
        .join("state-blocked");
    fs::create_dir_all(&state).expect("state root");
    // A state root is the agent's alone, as the start check requires.
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("mode");
    fs::write(state.join("egress"), b"not a directory").expect("file");
    let mut given = crun_flags("h-egress-blocked", &FAILING_TOOLS);
    let at = given
        .iter()
        .position(|flag| flag == "--state-root")
        .expect("flag");
    given[at + 1] = state.display().to_string();
    let (ran, sent) = run_crun_agent(&given);
    assert_eq!(ran.status(), code(2), "{}", ran.err);
    let want = format!(
        "--egress allow-list: the egress store: {}/egress is not a directory",
        state.display()
    );
    assert!(ran.err.contains(&want), "{}", ran.err);
    assert_eq!(sent, 0, "something was sent");
}
