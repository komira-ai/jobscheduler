//! The update hook's look at the agent's state root (`state_root` in its params) when the
//! state root belongs to another user, or when the path cannot be looked at by the user the
//! hook runs as. Both need users other than the one running the tests, so they are here; the
//! other shapes of an unusable state root are in `tests/agent/update_hook/machine_root.rs`.

use std::fs;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use js_agent::update_hook::{
    DirStat, Done, Invocation, Machine, Outcome, Params, Reason, Step, fd_stat, run_step,
};

use crate::harness::{AGENT, Kv, STRANGER, arg, kv, make_dir, require_root, run_agent, work_dir};

pub fn probe(name: &str) -> Option<Vec<Kv>> {
    match name {
        "hook_root" => Some(every_step(&arg("TREE"), &arg("ROOT"))),
        _ => None,
    }
}

/// The hook's refusal of the state root `path` with the agent's words `why`.
fn refusal(path: &Path, why: &str) -> String {
    format!("state_root: {}: {why}", path.display())
}

/// Writes the declaration of a hook whose state directory is `tree/state` (root's, 0700).
fn hook_tree(tree: &Path) {
    make_dir(&tree.join("state"), 0o700, 0);
    fs::write(
        tree.join("declaration.toml"),
        format!(
            concat!(
                "state_dir = \"{root}/state\"\n[hook]\nunits = [\"js-agent-crun.service\"]\n",
                "write_paths = [\"{root}/state\"]\n",
                "[hook.timeout_secs]\nprepare = 3900\ncutover = 4320\nverify = 615\n",
                "rollback = 8220\ncleanup = 120\n"
            ),
            root = tree.display(),
        ),
    )
    .unwrap();
}

/// The params of a first install or of an update from 1.3.0 whose state root is `root`.
fn params(tree: &Path, root: &Path, update: bool) -> Params {
    let previous = if update {
        "\"updater.previous_release\" = \"/opt/js-agent/releases/1.3.0\"\n\
         \"updater.previous_version\" = \"1.3.0\"\n"
    } else {
        ""
    };
    Params::parse(&format!(
        concat!(
            "cordon_lease_secs = 300\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\n",
            "defer_retry_secs = 300\nrelease_link = \"{tree}/current\"\n",
            "\"updater.artifact_digest\" = \"sha256:{digest}\"\n",
            "\"updater.staged_release\" = \"{tree}/releases/1.4.0\"\n",
            "\"updater.target_version\" = \"1.4.0\"\n{previous}state_root = \"{root}\"\n"
        ),
        tree = tree.display(),
        digest = "0123456789abcdef".repeat(4),
        previous = previous,
        root = root.display(),
    ))
    .unwrap()
}

fn real_stat(_: &Path, fd: BorrowedFd<'_>) -> io::Result<DirStat> {
    fd_stat(fd)
}

/// Runs `step` with the declaration in `tree` and `params`, as the machine does.
fn run(tree: &Path, step: Step, params: &Params) -> Done {
    let declaration = tree.join("declaration.toml");
    let own = || Err("unused".to_owned());
    let machine = Machine {
        declaration: &declaration,
        stat: &real_stat,
        own_host_id: &own,
    };
    let invocation = Invocation {
        step,
        artifact: (step == Step::Prepare).then(|| tree.join("releases/1.4.0")),
        params: PathBuf::from("/unused"),
    };
    run_step(&invocation, params, &machine)
}

/// How every step of a first install and of an update ends with the state root `root`, one
/// line each: the exit, the reason, `reversible` and the detail.
fn every_step(tree: &Path, root: &Path) -> Vec<Kv> {
    let mut lines = Vec::new();
    for update in [false, true] {
        let params = params(tree, root, update);
        for step in Step::ALL {
            let done = run(tree, step, &params);
            lines.push(kv(
                &format!("{step}-update-{update}"),
                format!(
                    "{:?} {:?} {} {}",
                    done.outcome,
                    done.report.reason,
                    done.report.reversible,
                    done.report.detail.unwrap_or_default()
                ),
            ));
        }
    }
    lines.push(kv(
        "state_dir_entries",
        fs::read_dir(tree.join("state")).unwrap().count(),
    ));
    lines
}

/// What [`every_step`] reports when every step is refused with `said`.
fn refused_everywhere(said: &str) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    for update in [false, true] {
        for step in Step::ALL {
            lines.push((
                format!("{step}-update-{update}"),
                format!(
                    "{:?} {:?} true {said}",
                    Outcome::Failure,
                    Some(Reason::StateInvalid)
                ),
            ));
        }
    }
    lines.push(("state_dir_entries".to_owned(), "0".to_owned()));
    lines
}

/// Asserts that a hook run as root refuses a state root that another user owns (mode 0700, a
/// directory, not a link), on every step of a first install and of an update, in exactly the
/// words of the hook's param and the agent's check, reversibly and with nothing made in the
/// hook's state directory. It catches an owner check that the hook skips, or a first install
/// that takes another user's directory for no state root.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn the_hook_refuses_a_state_root_another_user_owns_on_every_step() {
    require_root();
    let work = work_dir("priv-hook-root-owner");
    let tree = work.path().to_owned();
    hook_tree(&tree);
    let theirs = tree.join("their-state");
    make_dir(&theirs, 0o700, STRANGER);
    let said = refusal(&theirs, "not owned by the user running the agent");
    let got = every_step(&tree, &theirs);
    assert_eq!(got, refused_everywhere(&said));
}

/// Asserts that a hook that cannot look at the path of its state root (the agent's user, below
/// a directory only root may enter, where the lookup is refused with permission denied rather
/// than not found) refuses it on every step of a first install and of an update, in exactly the
/// words of the hook's param and the agent's check, reversibly and with nothing made in the
/// hook's state directory. It catches a lookup that takes any error for "nothing is there",
/// which would let a first install go on and an update read no taint.
#[test]
#[ignore = "needs root: see the module docs of tests/privilege/main.rs"]
fn the_hook_refuses_a_state_root_it_is_denied_a_look_at_on_every_step() {
    let work = work_dir("priv-hook-root-denied");
    let tree = work.path().to_owned();
    hook_tree(&tree);
    // The hook's state directory is the agent's here, as the hook runs as the agent.
    std::os::unix::fs::lchown(tree.join("state"), Some(AGENT), Some(AGENT)).unwrap();
    let closed = tree.join("closed");
    make_dir(&closed, 0o700, 0);
    let root = closed.join("state");
    make_dir(&root, 0o700, AGENT);
    let report = run_agent(&[], "hook_root", &[("TREE", &tree), ("ROOT", &root)]);
    for (key, want) in refused_everywhere(&refusal(&root, "cannot read the directory")) {
        report.expect(&key, &want);
    }
}
