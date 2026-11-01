//! The hook on a machine: the steps over the real lease client, the real agent state, the real
//! release link and the declaration read from a file. A real lease server runs in the test
//! process; the unit manager is the one thing left out (a step that stops or starts a unit is
//! tested over the fakes), so the steps run here are those that never touch it.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_agent::lifecycle::StateRoot;
use js_agent::own_host::{Lease, LeaseError};
use js_agent::update_hook::{
    Class, ConnectParams, DECLARATION_PATH, Done, Invocation, Machine, MachineLease, Outcome,
    Params, Reason, Step, run_step,
};
use js_agent_wire::Taint;

use super::step_rig::as_root;
use crate::cordon::http::{Enrolled, enrol};
use crate::jobs::private;
use crate::support::Scratch;

const UNIT: &str = "js-agent-crun.service";

/// A tree of one test: the declaration, the state directory of the hook, the staged release
/// (an agent that prints its version), and a place for the params files.
struct Tree {
    _files: Scratch,
    root: PathBuf,
}

impl Tree {
    fn new(name: &str) -> Tree {
        let files = Scratch::new(name);
        let root = files.path("tree");
        fs::create_dir_all(root.join("releases/1.4.0")).unwrap();
        fs::create_dir_all(root.join("link")).unwrap();
        // The state directory is private, as the hook requires.
        private(&files, "tree/state");
        // The agent is written by a child that has finished before it is run, so that no
        // fork of another test holds its file open for writing.
        let agent = root.join("releases/1.4.0/js-agent");
        let script = "printf '#!/bin/sh\\nprintf \"js-agent 1.4.0\\\\nruntimes: crun docker native\\\\n\"\\n'";
        let made = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{script} > '{}'", agent.display()))
            .status()
            .unwrap();
        assert!(made.success());
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        let tree = Tree {
            _files: files,
            root,
        };
        fs::write(
            tree.declaration(),
            format!(
                concat!(
                    "state_dir = \"{root}/state\"\n[hook]\nunits = [\"{unit}\"]\n",
                    "write_paths = [\"{root}/releases\", \"{root}/link\", \"{root}/state\"]\n",
                    "[hook.timeout_secs]\nprepare = 3900\ncutover = 4320\nverify = 615\n",
                    "rollback = 8220\ncleanup = 120\n"
                ),
                root = tree.root.display(),
                unit = UNIT,
            ),
        )
        .unwrap();
        tree
    }

    fn declaration(&self) -> PathBuf {
        self.root.join("declaration.toml")
    }

    fn staged(&self) -> PathBuf {
        self.root.join("releases/1.4.0")
    }

    fn record(&self) -> PathBuf {
        self.root.join("state/record.json")
    }

    /// The params of an update from 1.3.0 (or a first install), plus `extra` lines.
    fn params(&self, previous: bool, extra: &str) -> Params {
        let previous = if previous {
            format!(
                "\"updater.previous_release\" = \"{}/releases/1.3.0\"\n\"updater.previous_version\" = \"1.3.0\"\n",
                self.root.display()
            )
        } else {
            String::new()
        };
        Params::parse(&format!(
            concat!(
                "cordon_lease_secs = 300\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\n",
                "defer_retry_secs = 300\nrelease_link = \"{root}/link/current\"\n",
                "\"updater.artifact_digest\" = \"sha256:{digest}\"\n",
                "\"updater.staged_release\" = \"{staged}\"\n",
                "\"updater.target_version\" = \"1.4.0\"\n{previous}{extra}"
            ),
            root = self.root.display(),
            digest = "0123456789abcdef".repeat(4),
            staged = self.staged().display(),
            previous = previous,
            extra = extra,
        ))
        .unwrap()
    }

    /// Runs `step` on a machine whose id is `own`, with the declaration of the tree.
    fn run(&self, step: Step, params: &Params, own: Result<&str, &str>) -> Done {
        let invocation = Invocation {
            step,
            artifact: (step == Step::Prepare).then(|| self.staged()),
            params: PathBuf::from("/unused"),
        };
        let declaration = self.declaration();
        let own_host_id = || own.map(str::to_owned).map_err(str::to_owned);
        let machine = Machine {
            declaration: &declaration,
            stat: &as_root,
            own_host_id: &own_host_id,
        };
        run_step(&invocation, params, &machine)
    }
}

/// The params lines that reach the enrolled server as its host `mac-1`.
fn reaching(enrolled: &Enrolled) -> String {
    format!(
        "server = \"{}\"\nstate_root = \"{}\"\nhost_id = \"mac-1\"\n",
        enrolled.server.url(),
        enrolled.state.display()
    )
}

#[test]
fn the_machine_cordons_the_host_on_prepare_and_lifts_the_cordon_on_rollback_over_a_real_server() {
    let enrolled = enrol("hook_machine_prepare_rollback");
    let tree = Tree::new("hook-machine-prepare-rollback");
    let params = tree.params(true, &reaching(&enrolled));
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let listed = enrolled.server.host("mac-1");
    assert!(listed["cordon"]["expires_at_ms"].is_u64(), "{listed}");
    assert!(tree.record().exists());

    let done = tree.run(Step::Rollback, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(enrolled.server.host("mac-1").get("cordon").is_none());
    assert!(!tree.record().exists());
}

#[test]
fn the_host_the_params_do_not_name_is_the_machines_own() {
    let enrolled = enrol("hook_machine_own_id");
    let tree = Tree::new("hook-machine-own-id");
    let lines = reaching(&enrolled).replace("host_id = \"mac-1\"\n", "");
    let params = tree.params(true, &lines);
    let done = tree.run(Step::Prepare, &params, Ok("mac-1"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(enrolled.server.host("mac-1")["cordon"].is_object());
    // A machine whose id cannot be read cannot reach the server: its error is the lease's.
    let done = tree.run(Step::Rollback, &params, Err("no machine id"));
    assert_eq!(
        done.report.reason,
        Some(Reason::LeaseUnreachable),
        "{done:?}"
    );
    assert!(
        done.report
            .detail
            .as_deref()
            .unwrap()
            .contains("no machine id")
    );
}

#[test]
fn a_first_install_needs_neither_a_server_nor_an_agent_state() {
    let tree = Tree::new("hook-machine-first-install");
    // No server, no state root: the step never asks the lease server or the taint marker.
    let params = tree.params(false, "");
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert!(tree.record().exists());
    let absent = tree.root.join("no-such-state");
    let named = format!("state_root = \"{}\"\n", absent.display());
    let done = tree.run(Step::Prepare, &tree.params(false, &named), Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}

#[test]
fn an_update_without_a_server_ends_as_a_lease_failure_of_the_environment() {
    let tree = Tree::new("hook-machine-no-server");
    let params = tree.params(true, "");
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::LeaseUnreachable));
    assert_eq!(done.report.class, Class::Environment);
    assert!(done.report.detail.as_deref().unwrap().contains("server"));
}

#[test]
fn a_state_root_that_is_not_private_fails_the_step_and_one_that_is_tainted_halts_it() {
    let enrolled = enrol("hook_machine_state");
    let tree = Tree::new("hook-machine-state");
    let params = tree.params(true, &reaching(&enrolled));
    let mode = fs::metadata(&enrolled.state).unwrap().permissions().mode();
    fs::set_permissions(&enrolled.state, fs::Permissions::from_mode(0o755)).unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{done:?}");
    assert!(!tree.record().exists());
    fs::set_permissions(&enrolled.state, fs::Permissions::from_mode(mode & 0o777)).unwrap();

    StateRoot::open(&enrolled.state)
        .unwrap()
        .write(&Taint {
            reason: "residue".to_owned(),
            run_id: None,
            check: None,
        })
        .unwrap();
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Halt, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::Tainted));
}

#[test]
fn a_declaration_that_cannot_be_read_ends_the_step_before_it_looks_at_anything() {
    let tree = Tree::new("hook-machine-no-declaration");
    fs::remove_file(tree.declaration()).unwrap();
    for step in Step::ALL {
        let done = tree.run(step, &tree.params(true, ""), Err("unused"));
        assert_eq!(done.outcome, Outcome::Failure, "{step}");
        assert_eq!(done.report.reason, Some(Reason::StateInvalid), "{step}");
        assert!(
            done.report
                .detail
                .as_deref()
                .unwrap()
                .contains("declaration"),
            "{step}"
        );
    }
}

#[test]
fn every_step_the_updater_can_ask_for_is_run_by_its_own_body() {
    let tree = Tree::new("hook-machine-dispatch");
    let params = tree.params(true, "");
    // With no record: `cutover` and `verify` have no `prepare` behind them, `rollback` and
    // `cleanup` have nothing to finish.
    for (step, outcome, reason) in [
        (Step::Cutover, Outcome::Failure, Some(Reason::StateInvalid)),
        (Step::Verify, Outcome::Failure, Some(Reason::StateInvalid)),
        (Step::Rollback, Outcome::Success, None),
        (Step::Cleanup, Outcome::Success, None),
    ] {
        let done = tree.run(step, &params, Err("unused"));
        assert_eq!(done.outcome, outcome, "{step}");
        assert_eq!(done.report.reason, reason, "{step}");
    }
}

fn connect(params: &ConnectParams, own: Result<&str, &str>) -> MachineLease {
    MachineLease::connect(params, &|| own.map(str::to_owned).map_err(str::to_owned))
}

fn unreachable(why: Result<impl std::fmt::Debug, LeaseError>) -> String {
    match why {
        Err(LeaseError::Unreachable(text)) => text,
        other => panic!("not an unreachable lease: {other:?}"),
    }
}

#[test]
fn a_lease_that_could_not_be_made_answers_every_call_with_the_reason_it_could_not() {
    let files = Scratch::new("hook-machine-broken-lease");
    let empty_state = private(&files, "empty-state");
    let ca = files.path("no-such-ca.pem");
    let full = ConnectParams {
        server: Some("http://127.0.0.1:1".to_owned()),
        state_root: Some(empty_state.clone()),
        token_file: None,
        ca_cert: None,
        host_id: Some("mac-1".to_owned()),
    };
    let cases = [
        (
            ConnectParams {
                server: None,
                ..full.clone()
            },
            "server",
        ),
        (
            ConnectParams {
                state_root: None,
                ..full.clone()
            },
            "token_file",
        ),
        (full.clone(), "token_file"),
        (
            ConnectParams {
                token_file: Some(files.path("no-such-token")),
                ..full.clone()
            },
            "no-such-token",
        ),
        (
            ConnectParams {
                token_file: Some(files.write("token", "tok-0123456789", 0o600)),
                ca_cert: Some(ca),
                ..full.clone()
            },
            "ca",
        ),
        (
            ConnectParams {
                host_id: None,
                ..full
            },
            "no id",
        ),
    ];
    for (params, word) in cases {
        let lease = connect(&params, Err("no id"));
        for text in [
            unreachable(lease.host()),
            unreachable(lease.cordon(60)),
            unreachable(lease.uncordon()),
        ] {
            assert!(text.contains(word), "{word}: {text}");
        }
    }
}

#[test]
fn a_lease_made_from_the_params_reaches_the_server_and_the_join_token_is_not_a_host_credential() {
    let enrolled = enrol("hook_machine_lease");
    let lease = connect(
        &ConnectParams {
            server: Some(enrolled.server.url()),
            state_root: Some(enrolled.state.clone()),
            host_id: Some("mac-1".to_owned()),
            ..ConnectParams::default()
        },
        Err("unused"),
    );
    assert_eq!(lease.host().unwrap().id, "mac-1");
    assert!(!lease.cordon(60).unwrap().already_cordoned);
    assert!(lease.uncordon().unwrap().lifted);
    // A state root with no credential in it: the token file is used, and the server refuses
    // the pool's join token on the host's own routes.
    let files = Scratch::new("hook-machine-join-token");
    let bare = private(&files, "bare-state");
    let lease = connect(
        &ConnectParams {
            server: Some(enrolled.server.url()),
            state_root: Some(bare),
            token_file: Some(enrolled.server.join_file()),
            host_id: Some("mac-1".to_owned()),
            ..ConnectParams::default()
        },
        Err("unused"),
    );
    assert!(matches!(lease.host(), Err(LeaseError::Refused(_))));
}

#[test]
fn the_machine_reads_the_declaration_the_updater_installs_and_looks_at_directories_as_root_does() {
    assert_eq!(Machine::system().declaration, Path::new(DECLARATION_PATH));
    let root = File::open("/").unwrap();
    let stat = (Machine::system().stat)(Path::new("/"), std::os::fd::AsFd::as_fd(&root)).unwrap();
    assert!(stat.is_dir && !stat.is_symlink);
    // The machine's own id is a host id the way the agent makes one, or it cannot be read.
    if let Ok(id) = (Machine::system().own_host_id)() {
        assert!(id.starts_with("h-") && id.len() == 34, "{id}");
    }
}
