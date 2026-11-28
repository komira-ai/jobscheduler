//! `prepare` on a machine whose flags file names a path or a user of the host's neighbours (a
//! mount root, a mount provider's directory or user) that the agent's start refuses: the update
//! is deferred `host_setup_missing` before the step looks at anything else, so a cutover never
//! stops an agent for one that cannot start. This is the Macs' failed update of tierstore's trees
//! root: the start check refused the missing mount, and so did the rollback's start of the old
//! agent on the same flags.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use js_agent::update_hook::{Class, Done, Outcome, Reason, Step};

use super::machine::Tree;

/// The flags file of `tree`: a server, the pool and `lines`, owner-only.
fn flags(tree: &Tree, lines: &[String]) {
    let mut text = "--server=https://leases.example:7300\n--pool=farm\n".to_owned();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    fs::write(tree.flags(), text).unwrap();
    fs::set_permissions(tree.flags(), fs::Permissions::from_mode(0o600)).unwrap();
}

/// Asserts the deferral of a prepare whose flags name `problem`: exit 75, `host_setup_missing`,
/// an environment matter, the params' retry hint, the whole detail, and no record written.
fn assert_waits_for(done: &Done, tree: &Tree, problem: &str) {
    assert_eq!(done.outcome, Outcome::Deferred, "{done:?}");
    assert_eq!(done.report.reason, Some(Reason::HostSetupMissing));
    assert_eq!(done.report.class, Class::Environment);
    assert_eq!(done.report.retry_after_secs, Some(300));
    assert_eq!(
        done.report.detail.as_deref(),
        Some(
            format!(
                "{problem}: the agent does not start on the flags of {} without it, so the \
                 update waits for it and nothing is stopped",
                tree.flags().display()
            )
            .as_str()
        )
    );
    assert!(!tree.record().exists(), "no record is written");
}

/// An update (and a first install) on a host whose flags name a read-only mount root that is
/// not there, as tierstore's trees mount is not while tierstore is down, is deferred
/// `host_setup_missing` naming the root, with no record written; once the directory is there
/// the same prepare goes on. Every other step is not held by it: the cleanup of an update that
/// got that far is the record's.
///
/// Catches a prepare that lets the cutover stop the running agent for one that refuses to start
/// on the host's flags (the rollback's start of the old agent then refuses too, and the host has
/// no agent).
#[test]
fn an_update_on_a_host_whose_mount_root_is_missing_waits_with_nothing_changed() {
    let tree = Tree::new("hook-machine-neighbour-root");
    let trees = tree.files.path("tierstore-fs");
    for previous in [true, false] {
        // An update needs the state root of the agent that runs, a first install none.
        let lines = if previous {
            tree.state_line()
        } else {
            String::new()
        };
        let params = tree.params(previous, &lines);
        flags(
            &tree,
            &[format!("--mount-root-ro=trees={}", trees.display())],
        );
        let done = tree.run(Step::Prepare, &params, Err("unused"));
        assert_waits_for(
            &done,
            &tree,
            "--mount-root-ro trees: not an existing directory",
        );
    }
    fs::create_dir_all(&trees).unwrap();
    let params = tree.params(false, "");
    flags(
        &tree,
        &[format!("--mount-root-ro=trees={}", trees.display())],
    );
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
    // Only `prepare` runs the check: a later step of the same flags is not deferred by it.
    fs::remove_dir(&trees).unwrap();
    let done = tree.run(Step::Cleanup, &params, Err("unused"));
    assert_ne!(
        done.report.reason,
        Some(Reason::HostSetupMissing),
        "{done:?}"
    );
}

/// A writable mount root that is not there and a mount provider whose socket's directory is not
/// there each defer the prepare naming the flag, as the agent's start words it; a provider whose
/// directory is there, its user given by number, does not. (A user given by name is looked up in
/// the host's account database, which a test does not read.)
///
/// Catches a check that looks only at the read-only roots, or not at the providers.
#[test]
fn a_missing_mount_root_or_provider_directory_defers_the_prepare() {
    let tree = Tree::new("hook-machine-neighbour-provider");
    let run = tree.files.path("tierstore-run");
    let socket = run.join("provider.sock");
    let cases = [
        (
            vec![format!(
                "--mount-root=work={}",
                tree.files.path("work").display()
            )],
            "--mount-root work: not an existing directory".to_owned(),
        ),
        (
            vec![format!("--mount-provider=tierstore={}", socket.display())],
            format!(
                "--mount-provider tierstore: {} is not an existing directory",
                run.display()
            ),
        ),
    ];
    for (lines, problem) in cases {
        let params = tree.params(true, &tree.state_line());
        flags(&tree, &lines);
        let done = tree.run(Step::Prepare, &params, Err("unused"));
        assert_waits_for(&done, &tree, &problem);
    }
    fs::create_dir_all(&run).unwrap();
    let params = tree.params(false, "");
    flags(
        &tree,
        &[
            format!("--mount-provider=tierstore={}", socket.display()),
            "--mount-provider-user=tierstore=4321".to_owned(),
        ],
    );
    let done = tree.run(Step::Prepare, &params, Err("unused"));
    assert_eq!(done.outcome, Outcome::Success, "{done:?}");
}
