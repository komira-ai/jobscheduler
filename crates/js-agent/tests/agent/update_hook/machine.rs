//! The hook on a machine: the steps over the real lease client, the real agent state, the real
//! release link and the host's flags file read from disk. A real lease server runs in the test
//! process; the unit manager is the one thing left out (a step that stops or starts a unit is
//! tested over the fakes), so the steps run here are those that never touch it.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use js_agent::lifecycle::StateRoot;
use js_agent::own_host::{Lease, LeaseError};
use js_agent::update_hook::{
    Class, ConnectParams, Done, Invocation, Machine, MachineLease, Outcome, Params, Reason, Step,
    own_host_id, run_step,
};
use js_agent_wire::Taint;

use super::step_rig::as_root;
use crate::cordon::http::{Enrolled, enrol};
use crate::jobs::private;
use crate::support::{FakeFiles, MACHINE_ID, Scratch, linux_host};

const UNIT: &str = "js-agent-crun.service";

/// A tree of one test: the state directory of the hook, the staged release (an agent that prints
/// its version), the host's flags file and join token, and a place for the params files.
pub(super) struct Tree {
    pub(super) files: Scratch,
    pub(super) root: PathBuf,
}

impl Tree {
    pub(super) fn new(name: &str) -> Tree {
        let files = Scratch::new(name);
        let root = files.path("tree");
        fs::create_dir_all(root.join("releases/1.4.0")).unwrap();
        fs::create_dir_all(root.join("link")).unwrap();
        // The state directory is private, as the hook requires.
        private(&files, "tree/state");
        // The agent is written by a child that has finished before it is run, so that no
        // fork of another test holds its file open for writing.
        fs::create_dir_all(root.join("releases/1.4.0/bin")).unwrap();
        let agent = root.join("releases/1.4.0/bin/js-agent");
        let script =
            "printf '#!/bin/sh\nprintf \"js-agent 1.4.0\\nruntimes: crun docker native\\n\"\n'";
        let made = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{script} > '{}'", agent.display()))
            .status()
            .unwrap();
        assert!(made.success());
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        // The unit each release carries: the same, so the update restarts nothing new.
        for release in ["releases/1.4.0", "releases/1.3.0"] {
            fs::create_dir_all(root.join(release).join("units")).unwrap();
            fs::write(
                root.join(release).join("units").join(UNIT),
                "[Service]\nExecStart=/opt/agent\n",
            )
            .unwrap();
        }
        // The join token a first install needs, as the operator places it.
        let token = root.join("token");
        fs::write(&token, "tok-0123456789\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        Tree { files, root }
    }

    pub(super) fn staged(&self) -> PathBuf {
        self.root.join("releases/1.4.0")
    }

    pub(super) fn record(&self) -> PathBuf {
        self.root.join("state/record.json")
    }

    /// The host's flags file.
    pub(super) fn flags(&self) -> PathBuf {
        self.root.join("flags")
    }

    /// The params line that names the tree's own (private) state directory as the state root.
    pub(super) fn state_line(&self) -> String {
        format!("state_root = \"{}\"\n", self.root.join("state").display())
    }

    /// The params of an update from 1.3.0 (or a first install). `extra` is lines of the
    /// agent's flags the way the old table gave them: `state_root` and `token_file` are params
    /// keys (the tree's own state root is not there, and its token file is a good one, unless
    /// they say otherwise), `server`, `ca_cert` and `host_id` go to the tree's flags file, which
    /// always names the pool, and any other line is put in the table as it is.
    pub(super) fn params(&self, previous: bool, extra: &str) -> Params {
        let previous = if previous {
            format!(
                "\"updater.previous_release\" = \"{}/releases/1.3.0\"\n\"updater.previous_version\" = \"1.3.0\"\n",
                self.root.display()
            )
        } else {
            String::new()
        };
        let mut state_root = format!("\"{}\"", self.root.join("no-such-state").display());
        let mut token_file = format!("\"{}\"", self.root.join("token").display());
        let mut flags = vec!["--pool=farm".to_owned()];
        let mut rest = String::new();
        for line in extra.lines() {
            match line.split_once(" = ") {
                Some(("state_root", value)) => state_root = value.to_owned(),
                Some(("token_file", value)) => token_file = value.to_owned(),
                Some(("server", value)) => {
                    flags.push(format!("--server={}", value.trim_matches('"')))
                }
                Some(("ca_cert", value)) => {
                    flags.push(format!("--ca-cert={}", value.trim_matches('"')))
                }
                Some(("host_id", value)) => {
                    flags.push(format!("--host-id={}", value.trim_matches('"')))
                }
                _ => rest.push_str(&format!("{line}\n")),
            }
        }
        fs::write(self.flags(), format!("{}\n", flags.join("\n"))).unwrap();
        fs::set_permissions(self.flags(), fs::Permissions::from_mode(0o600)).unwrap();
        Params::parse(&format!(
            concat!(
                "cordon_lease_secs = 300\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\n",
                "defer_retry_secs = 300\nrelease_link = \"{root}/link/current\"\n",
                "unit = \"{unit}\"\nstate_dir = \"{root}/state\"\n",
                "flags_file = \"{flags}\"\nstate_root = {state_root}\ntoken_file = {token_file}\n",
                "runtime = \"crun\"\nprepare_timeout_secs = 3900\ncutover_timeout_secs = 4320\n",
                "verify_timeout_secs = 615\nrollback_timeout_secs = 8220\n",
                "cleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
                "\"updater.artifact_digest\" = \"sha256:{digest}\"\n",
                "\"updater.staged_release\" = \"{staged}\"\n",
                "\"updater.target_version\" = \"1.4.0\"\n{previous}{rest}"
            ),
            root = self.root.display(),
            unit = UNIT,
            flags = self.flags().display(),
            state_root = state_root,
            token_file = token_file,
            digest = "0123456789abcdef".repeat(4),
            staged = self.staged().display(),
            previous = previous,
            rest = rest,
        ))
        .unwrap()
    }

    /// Runs `step` on a machine whose id is `own`.
    pub(super) fn run(&self, step: Step, params: &Params, own: Result<&str, &str>) -> Done {
        let invocation = Invocation {
            step,
            artifact: (step == Step::Prepare).then(|| self.staged()),
            params: PathBuf::from("/unused"),
        };
        let own_host_id = || own.map(str::to_owned).map_err(str::to_owned);
        let machine = Machine {
            stat: &as_root,
            own_host_id: &own_host_id,
            ..Machine::system()
        };
        run_step(&invocation, params, &machine)
    }
}

/// The params lines that reach the enrolled server as its host `mac-1`.
pub(super) fn reaching(enrolled: &Enrolled) -> String {
    format!(
        "server = \"{}\"\nstate_root = \"{}\"\nhost_id = \"mac-1\"\n",
        enrolled.server.url(),
        enrolled.state.display()
    )
}

/// Makes the record say that `cutover` and `verify` have passed, as `cleanup` requires.
fn verified(tree: &Tree) {
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(tree.record()).unwrap()).unwrap();
    for mark in ["stop_begun", "link_moved", "start_issued", "verify_ok"] {
        record[mark] = serde_json::Value::Bool(true);
    }
    fs::write(tree.record(), serde_json::to_vec(&record).unwrap()).unwrap();
}

#[test]
fn the_machine_cordons_the_host_on_prepare_and_lifts_the_cordon_on_cleanup_over_a_real_server() {
    let enrolled = enrol("hook_machine_prepare_rollback");
    let tree = Tree::new("hook-machine-prepare-rollback");
    let params = tree.params(true, &reaching(&enrolled));
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    let listed = enrolled.server.host("mac-1");
    assert!(listed["cordon"]["expires_at_ms"].is_u64(), "{listed}");
    assert!(tree.record().exists());

    verified(&tree);
    let done = tree.run(Step::Cleanup, &params, Err("unused"));
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
    let done = tree.run(Step::Prepare, &params, Err("no machine id"));
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
    let params = tree.params(true, &tree.state_line());
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
fn every_step_the_updater_can_ask_for_is_run_by_its_own_body() {
    let tree = Tree::new("hook-machine-dispatch");
    let params = tree.params(true, &tree.state_line());
    // With no record: `cutover` and `verify` have no `prepare` behind them and `cleanup` has
    // nothing to finish.
    for (step, outcome, reason) in [
        (Step::Cutover, Outcome::Failure, Some(Reason::StateInvalid)),
        (Step::Verify, Outcome::Failure, Some(Reason::StateInvalid)),
        (Step::Rollback, Outcome::Success, None),
        (Step::Cleanup, Outcome::Success, None),
    ] {
        let done = tree.run(step, &params, Err("unused"));
        assert_eq!(done.outcome, outcome, "{step}");
        assert_eq!(done.report.reason, reason, "{step}");
        // The step's own body answered, not the check of the state root before it.
        let detail = done.report.detail.as_deref().unwrap_or_default();
        assert!(detail.contains("there is no record"), "{step}: {detail}");
    }
}

fn connect(params: &ConnectParams, own: Result<&str, &str>) -> MachineLease {
    MachineLease::connect(
        params,
        &|| own.map(str::to_owned).map_err(str::to_owned),
        js_agent::own_host::RequestLimit::DEFAULT,
    )
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
    // A credential in a state root that others can read is not used.
    let open_state = super::machine_root::open_state_root(&files);
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
                state_root: Some(open_state),
                ..full.clone()
            },
            "state_root: ",
        ),
        (
            ConnectParams {
                token_file: Some(files.path("no-such-token")),
                ..full.clone()
            },
            "token_file: ",
        ),
        // A state root that is not there holds no credential: the token file is read.
        (
            ConnectParams {
                state_root: Some(files.path("no-such-state")),
                token_file: Some(files.path("no-such-token")),
                ..full.clone()
            },
            "token_file: ",
        ),
        (
            ConnectParams {
                server: Some("https://127.0.0.1:1".to_owned()),
                token_file: Some(files.write("token", "tok-0123456789", 0o600)),
                ca_cert: Some(ca),
                ..full.clone()
            },
            "--ca-cert: ",
        ),
        (
            ConnectParams {
                host_id: None,
                ..full.clone()
            },
            "no id",
        ),
        (
            ConnectParams {
                host_id: Some("Not:A:Host".to_owned()),
                ..full
            },
            "--host-id",
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
fn the_machine_looks_at_directories_files_and_users_as_the_system_does() {
    let root = File::open("/").unwrap();
    let stat = (Machine::system().stat)(Path::new("/"), std::os::fd::AsFd::as_fd(&root)).unwrap();
    assert!(stat.is_dir && !stat.is_symlink);
    // The machine's own id is read the way the agent reads it, on this platform: the same id,
    // or the same error when this host has none.
    assert_eq!(
        (Machine::system().own_host_id)(),
        own_host_id(
            &js_agent::Platform::current(),
            &js_agent::SystemCommands::default(),
            &js_agent::SystemFiles
        )
    );
}

#[test]
fn the_machines_own_host_id_is_the_agents_from_the_machine_id_it_reads() {
    let host = linux_host();
    assert_eq!(
        own_host_id(&host.platform, &host.commands, &host.files),
        Ok(js_agent::host_id(MACHINE_ID))
    );
    let bare = FakeFiles::default();
    let Err(why) = own_host_id(&host.platform, &host.commands, &bare) else {
        panic!("a machine with no machine id has no host id");
    };
    assert!(why.contains("machine-id"), "{why}");
}

#[test]
fn the_machines_host_runs_rollback_as_the_real_step_over_the_files_of_a_first_install() {
    let tree = Tree::new("hook-machine-rollback-real");
    // A first install that has written its record and moved nothing: a real rollback deletes
    // the record and succeeds, reversibly. It asks no lease server and no unit manager.
    let params = tree.params(false, "");
    assert_eq!(
        tree.run(Step::Prepare, &params, Err("unused")).outcome,
        Outcome::Success
    );
    assert!(tree.record().exists());
    let done = tree.run(Step::Rollback, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.reason, None);
    assert!(done.report.reversible);
    assert!(!tree.record().exists());
}
