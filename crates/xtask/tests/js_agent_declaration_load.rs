//! The js-agent declaration through the host updater's own load and launch code (kbf's
//! `update-rights`, at the revision the root manifest pins): `load_dir` over a directory that
//! holds the shipped file, with the trust rules on the directory and the file, and the
//! `systemd-run` command `Launch::of` and `command` make of it, whose properties are the rights
//! the hook runs with and whose words the hook itself parses.
//!
//! The directory is the test's own, so the trusted account is the test's (its owner), not root.
//! The loader's rules for a hook that runs as root also look at what the declared paths are on
//! this machine (the directories above them, read only): on a host where they are root's and
//! closed to group and others, as on every CI runner and the farm, nothing is refused for them.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use js_agent::update_hook::{Invocation, Step as HookStep};
use js_testkit::dir::TestDir;
use update_hook::Step;
use update_rights::{EtcAccounts, Launch, Loaded, Policy, command, load_dir};
use xtask::release::stage::DECLARATION_SOURCE;

const RELEASE: &str = "/opt/js-agent/releases/1.4.0";
const PARAMS: &str = "/run/updater/params/js-agent.toml";

fn shipped() -> Vec<u8> {
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    std::fs::read(root.join(DECLARATION_SOURCE)).expect("the declaration")
}

fn accounts() -> EtcAccounts {
    EtcAccounts::new("root:x:0:0:root:/root:/bin/sh\n", "root:x:0:\n")
}

/// A directory of declarations as the updater's install step leaves it: mode 0755, the file
/// `name` mode 0644, both the test's own.
fn updater_d(test: &str, name: &str, bytes: &[u8]) -> (TestDir, PathBuf) {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), test);
    let dir = tmp.path().join("updater.d");
    std::fs::create_dir(&dir).expect("updater.d");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let file = dir.join(name);
    std::fs::write(&file, bytes).expect("declaration");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
    (tmp, dir)
}

fn owner(dir: &Path) -> u32 {
    std::fs::metadata(dir).expect("owner").uid()
}

fn load(dir: &Path, trusted_uid: u32) -> Loaded {
    let accounts = accounts();
    load_dir(
        dir,
        &Policy {
            trusted_uid,
            accounts: &accounts,
        },
    )
    .expect("the directory lists")
}

/// The reasons the loader refused the files of `dir` for.
fn refusals(dir: &Path, trusted_uid: u32) -> Vec<String> {
    let loaded = load(dir, trusted_uid);
    assert!(loaded.services.is_empty(), "{:?}", loaded.services.keys());
    loaded
        .refused
        .iter()
        .map(|refused| refused.reason.to_string())
        .collect()
}

/// The loader takes the shipped file, and the step it launches runs as root with no capability,
/// no environment, exactly the three write paths and the hook record, the three read paths, the
/// address families of dbus and the lease server, and the step's bound; the command runs the
/// release's `js-agent` with the declaration's flags, which the hook parses into the release
/// link and the connect values. Catches a declaration the loader refuses, or one that grants
/// more than the update needs.
#[test]
fn the_loader_takes_the_shipped_declaration_and_launches_each_step_with_exactly_its_rights() {
    let (_tmp, dir) = updater_d("decl-load", "js-agent.toml", &shipped());
    let loaded = load(&dir, owner(&dir));
    assert!(loaded.refused.is_empty(), "{:?}", loaded.refused);
    let declared = &loaded.services["js-agent"];
    assert_eq!(declared.hook().uid, 0);
    assert_eq!(declared.service().uid, 0);
    for (step, hook_step, bound) in [
        (Step::Prepare, HookStep::Prepare, 3900),
        (Step::Cutover, HookStep::Cutover, 4320),
        (Step::Verify, HookStep::Verify, 615),
        (Step::Rollback, HookStep::Rollback, 8220),
        (Step::Cleanup, HookStep::Cleanup, 120),
    ] {
        let launch = Launch::of(
            declared,
            step,
            7,
            Path::new(RELEASE),
            Some(Path::new(PARAMS)),
        );
        assert!(launch.root_hook());
        let run = command(&launch).expect("a command");
        let args: Vec<String> = run
            .args
            .iter()
            .map(|arg| arg.to_str().expect("utf-8").to_owned())
            .collect();
        let properties: Vec<&str> = args
            .iter()
            .filter_map(|arg| arg.strip_prefix("--property="))
            .collect();
        let bound = format!("RuntimeMaxSec={bound}");
        assert_eq!(
            properties,
            [
                "User=root",
                "Group=root",
                "AmbientCapabilities=",
                "CapabilityBoundingSet=",
                "NoNewPrivileges=yes",
                "ProtectSystem=strict",
                "ProtectHome=yes",
                "PrivateTmp=yes",
                "ReadOnlyPaths=/etc/js-agent /opt/js-agent /var/lib/js-agent",
                "ReadWritePaths=/opt/js-agent/link /opt/js-agent/releases /var/lib/js-agent-update",
                "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX",
                bound.as_str(),
                "ProtectKernelTunables=yes",
                "ProtectKernelModules=yes",
                "ProtectKernelLogs=yes",
                "ProtectControlGroups=yes",
                "RestrictNamespaces=yes",
                "RestrictRealtime=yes",
                "LockPersonality=yes",
                "MemoryDenyWriteExecute=yes",
                "SystemCallFilter=@system-service",
                "SystemCallArchitectures=native",
            ],
            "{step:?}"
        );
        // After `--`: the release's binary, the declaration's words, the updater's own.
        let dashes = args.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(args[dashes + 1], format!("{RELEASE}/js-agent"));
        assert_eq!(args[dashes + 2], "update-hook");
        let invocation = Invocation::parse(args[dashes + 3..].iter().map(OsString::from))
            .unwrap_or_else(|e| panic!("{step:?}: the hook refuses {args:?}: {e}"));
        assert_eq!(invocation.step, hook_step);
        assert_eq!(invocation.params, PathBuf::from(PARAMS));
        assert_eq!(
            invocation.artifact,
            (hook_step == HookStep::Prepare).then(|| PathBuf::from(RELEASE))
        );
        assert_eq!(
            invocation.flags.release_link,
            Some(PathBuf::from("/opt/js-agent/link/current"))
        );
        assert_eq!(
            invocation.flags.state_root,
            Some(PathBuf::from("/var/lib/js-agent/state"))
        );
    }
}

/// The loader's trust rules hold for this file as for any: a file or a directory the group can
/// write, a file owned by another account than the trusted one, and a file named for another
/// service are each refused, and the service is left unmanaged.
#[test]
fn the_loader_refuses_the_declaration_in_an_untrusted_place_or_under_another_name() {
    let bytes = shipped();

    let (_tmp, dir) = updater_d("decl-load-file-mode", "js-agent.toml", &bytes);
    std::fs::set_permissions(
        dir.join("js-agent.toml"),
        std::fs::Permissions::from_mode(0o664),
    )
    .expect("mode");
    assert_eq!(
        refusals(&dir, owner(&dir)),
        [
            "the file is not a regular file owned by the trusted account: mode 664 lets the group or others write it"
        ]
    );

    let (_tmp, dir) = updater_d("decl-load-dir-mode", "js-agent.toml", &bytes);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).expect("mode");
    assert_eq!(
        refusals(&dir, owner(&dir)),
        [format!(
            "the directory {}: mode 775 lets the group or others write it is not owned by the trusted account or is writable by others",
            dir.display()
        )]
    );

    let (_tmp, dir) = updater_d("decl-load-owner", "js-agent.toml", &bytes);
    let other = owner(&dir) + 1;
    let found = refusals(&dir, other);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains(&format!("not the trusted uid {other}")),
        "{found:?}"
    );

    let (_tmp, dir) = updater_d("decl-load-name", "js-agent-crun.toml", &bytes);
    assert_eq!(
        refusals(&dir, owner(&dir)),
        ["name: \"js-agent\" is in a file named for \"js-agent-crun\""]
    );
}
