//! The hook on a Mac, through [`run_step`] with the host's flags file read from disk: the
//! decision of an update between two agents that list `native-handover`, with and without
//! `--vm-driver` in the flags file. The lease server named in the flags does not answer: an
//! update that hands over never asks it anything, and one that drains asks it for the host
//! before it places the cordon, which ends the step `lease_unreachable`.

use std::fs;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use js_testkit::dir::TestDir;

use super::{Machine, read_flags, run_step, vms_of};
use crate::update_hook::{Class, DirStat, Invocation, Outcome, Params, Reason, Step};

/// The detail of a `prepare` that hands the runs over (`step::prepare`'s note).
const HANDOVER_NOTE: &str =
    "the cutover hands the running jobs over to the new agent: no cordon and no drain";

/// What both releases' agents print to `--version`.
const VERSION: &str = "js-agent {v}\nruntimes: native\nstops: drain native-handover\n";

/// The hook's state directory as the hook requires it, whoever runs the test.
fn as_root(_: &Path, _: BorrowedFd<'_>) -> std::io::Result<DirStat> {
    Ok(DirStat {
        is_symlink: false,
        is_dir: true,
        uid: 0,
        gid: 0,
        mode: 0o700,
    })
}

/// A private directory of the test's user.
fn private(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// An update from 1.3.0 to 1.4.0 of the agent `js-agent` on the native runtime, both releases
/// archive releases with the same plist and an agent that lists the word, and the host's flags
/// file holding `flags` after the server, pool and host id: the params of the update.
fn update(dir: &TestDir, flags: &str) -> Params {
    let root = dir.path();
    for version in ["1.3.0", "1.4.0"] {
        let release = root.join("releases").join(version);
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::create_dir_all(release.join("units")).unwrap();
        fs::write(release.join("units/js-agent.plist"), "<plist/>\n").unwrap();
        // Written by a child that has ended before it is run, so no other test's fork holds
        // the file open for writing.
        let agent = release.join("bin/js-agent");
        let text = VERSION.replace("{v}", version).replace('\n', "\\n");
        let script = format!("printf '#!/bin/sh\\nprintf \"{text}\"\\n'");
        let made = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{script} > '{}'", agent.display()))
            .status()
            .unwrap();
        assert!(made.success());
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    }
    private(&root.join("state"));
    private(&root.join("agent-state"));
    fs::create_dir_all(root.join("link")).unwrap();
    let file = root.join("flags");
    fs::write(
        &file,
        format!("--server=http://127.0.0.1:1\n--pool=farm\n--host-id=mac-1\n{flags}"),
    )
    .unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    Params::parse(&format!(
        concat!(
            "cordon_lease_secs = 300\ncordon_max_secs = 17355\ndrain_wait_secs = 3600\n",
            "defer_retry_secs = 300\nrelease_link = \"{root}/link/current\"\n",
            "unit = \"js-agent\"\nstate_dir = \"{root}/state\"\n",
            "flags_file = \"{root}/flags\"\nstate_root = \"{root}/agent-state\"\n",
            "token_file = \"{root}/token\"\n",
            "runtime = \"native\"\nprepare_timeout_secs = 3900\ncutover_timeout_secs = 4320\n",
            "verify_timeout_secs = 615\nrollback_timeout_secs = 8220\n",
            "cleanup_timeout_secs = 120\nverify_deadline_secs = 9000\n",
            "\"updater.artifact_digest\" = \"sha256:{digest}\"\n",
            "\"updater.staged_release\" = \"{root}/releases/1.4.0\"\n",
            "\"updater.target_version\" = \"1.4.0\"\n",
            "\"updater.previous_release\" = \"{root}/releases/1.3.0\"\n",
            "\"updater.previous_version\" = \"1.3.0\"\n",
        ),
        root = root.display(),
        digest = "0123456789abcdef".repeat(4),
    ))
    .unwrap()
}

/// `prepare` of `params` on this Mac, its state directory looked at as root's.
fn prepare(params: &Params) -> crate::update_hook::Done {
    let invocation = Invocation {
        step: Step::Prepare,
        artifact: Some(params.staged_release.clone()),
        params: "/unused".into(),
    };
    let machine = Machine {
        stat: &as_root,
        ..Machine::system()
    };
    run_step(&invocation, params, &machine)
}

/// Asserts the decision an update on a Mac takes from the host's flags file, read from disk as
/// the hook reads it: without `--vm-driver` the update between two agents that list
/// `native-handover` hands over (success, the hand-over note, the record says `handover`, and
/// the lease server is never asked); with `--vm-driver` and `--vm-store` the flags are read
/// (they are the agent's) and say the VM backend is on, and the same update drains: it asks the
/// lease server for the host to cordon it, and ends `lease_unreachable` with no record, since
/// this server does not answer. Catches a flags file whose VM backend the hook does not see,
/// which would hand a VM run to an agent that destroys it.
#[test]
fn a_mac_update_hands_over_unless_the_flags_file_turns_the_vm_backend_on() {
    let plain = TestDir::under(std::env::temp_dir(), "hook-mac-flags-plain");
    let params = update(&plain, "");
    let done = prepare(&params);
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    assert_eq!(done.report.detail.as_deref(), Some(HANDOVER_NOTE));
    let record = fs::read_to_string(plain.path().join("state/record.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["handover"], true, "{record}");

    let vms = TestDir::under(std::env::temp_dir(), "hook-mac-flags-vms");
    let store = vms.path().join("vm-store");
    fs::create_dir_all(&store).unwrap();
    let params = update(
        &vms,
        &format!(
            "--vm-driver=/usr/bin/true\n--vm-store={}\n",
            store.display()
        ),
    );
    let flags = read_flags(&params.flags_file, &Machine::system()).unwrap();
    assert!(vms_of(&flags), "the flags file is read and turns VMs on");
    let done = prepare(&params);
    assert_eq!(done.outcome, Outcome::Failure, "{done:?}");
    assert_eq!(
        done.report.reason,
        Some(Reason::LeaseUnreachable),
        "{done:?}"
    );
    assert_eq!(done.report.class, Class::Environment);
    assert!(!vms.path().join("state/record.json").exists());
}
