//! `release plan` when one of the commands it runs fails or prints what it should not.
//!
//! The commands are stand-ins: a script that behaves as the real `git` except for one
//! sub-command. This file holds one test and so runs no other thread: a script that is
//! being written is never executed while another thread has a child process starting
//! (which fails with "text file busy").

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use support::Repo;
use xtask::release::{Error, plan};

/// A script `name` in `dir`: for a command line containing `needle` it runs `reply`,
/// otherwise the real `git`.
fn fake_git(dir: &std::path::Path, name: &str, needle: &str, reply: &str) -> PathBuf {
    let path = dir.join(name);
    let script =
        format!("#!/bin/sh\ncase \"$*\" in\n  *\"{needle}\"*) {reply} ;;\nesac\nexec git \"$@\"\n");
    std::fs::write(&path, script).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path
}

/// Catches a plan that carries on, or reports success, when a command it depends on
/// fails: every command in turn (the checkout, the tags, the tag's commit, the diff and the
/// workspace metadata) failing is an error naming the command, and one that prints bytes
/// that are not text is refused.
#[test]
fn every_failing_command_fails_the_plan() {
    let repo = Repo::new("failures");
    repo.tag("v0.1.0");
    repo.write("crates/app/src/lib.rs", "// code\n");
    repo.commit("code");
    let bin = repo.dir.join("../failures-bin");
    std::fs::create_dir_all(&bin).expect("bin");

    let boom = "echo boom >&2; exit 1";
    for needle in ["rev-parse", "tag --list", "rev-list", "diff"] {
        let mut args = repo.plan();
        args.git = fake_git(
            &bin,
            &format!("git-{}", needle.replace(' ', "-")),
            needle,
            boom,
        );
        let Err(Error::Failed {
            command, stderr, ..
        }) = plan::run(&args)
        else {
            panic!("{needle} should fail the plan")
        };
        assert!(command.contains(needle), "{command}");
        assert_eq!(stderr, "boom");
    }

    for needle in ["rev-parse", "diff"] {
        let mut args = repo.plan();
        args.git = fake_git(
            &bin,
            &format!("git-text-{needle}"),
            needle,
            "printf '\\377'",
        );
        assert!(
            matches!(plan::run(&args), Err(Error::NotUtf8 { .. })),
            "{needle}"
        );
    }

    let mut args = repo.plan();
    args.cargo = fake_git(&bin, "cargo-fails", "metadata", boom);
    let Err(Error::Failed { command, .. }) = plan::run(&args) else {
        panic!("a failing cargo should fail the plan")
    };
    assert!(
        command.ends_with("metadata --no-deps --format-version 1"),
        "{command}"
    );
}
