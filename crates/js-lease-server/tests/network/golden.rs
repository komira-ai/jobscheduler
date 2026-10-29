//! The rendered script, masked, against files: the text the status page shows and the
//! text a machine runs differ only at the secret bytes, so the file is the contract.
//! The host's name is a hash, so the files hold `<HOST>` in its place.

use js_lease_server::bootstrap::{Fill, render};
use js_lease_server::network::{DefaultMode, NetworkMode};

use crate::fake::Fake;
use crate::support::{TEN_MINUTES, clock, ctx, host, mode, plan};

fn masked(plan: &js_elastic::BootPlan) -> String {
    render(plan, Fill::Mask)
        .unwrap()
        .replace(plan.host.as_str(), "<HOST>")
}

/// Asserts the whole masked script of a plan under the default mode: the trap first, the
/// cleanup lists, the identity files, no network steps, the agent checked by hash and the
/// unit. A change to the script is a change to this file.
#[test]
fn the_default_mode_script_matches_its_golden_file() {
    let host = host();
    let mut plan = plan(&host);
    let mode = DefaultMode::new("https://sched.example.org:7301", Vec::new());
    plan.network = mode.describe(&ctx(&host, TEN_MINUTES));
    assert_eq!(
        masked(&plan),
        include_str!("golden/default_masked.sh"),
        "the default mode's script changed"
    );
}

/// Asserts the whole masked script of a plan under the tailscale mode: the archive
/// fetched and checked, the key file written and removed, and the key file in the
/// cleanup list.
#[test]
fn the_tailscale_script_matches_its_golden_file() {
    let host = host();
    let fake = Fake::empty();
    let (_, shared) = clock();
    let mode = mode(&fake, shared);
    let mut plan = plan(&host);
    plan.network = mode.describe(&ctx(&host, TEN_MINUTES));
    assert_eq!(
        masked(&plan),
        include_str!("golden/tailscale_masked.sh"),
        "the tailscale script changed"
    );
    assert!(fake.seen().is_empty(), "a masked render calls nothing");
}

/// The unit section of a masked script: from `[Unit]` to the `UNIT` line.
fn unit_of(script: &str) -> &str {
    let start = script.find("[Unit]").unwrap();
    let end = script[start..].find("\nUNIT\n").unwrap();
    &script[start..start + end]
}

/// Asserts that the agent's unit restarts the agent always and lists no exit status that
/// stops the restart (the agent exits with status 2 on purpose and is to be started
/// again), under both modes. The golden files above pin the whole text; this names the
/// two lines the agent's docs require, so a change to either fails here with its reason.
#[test]
fn the_agent_unit_restarts_always_and_has_no_restart_prevent_status() {
    let host = host();
    let fake = Fake::empty();
    let (_, shared) = clock();
    let tailscale = mode(&fake, shared);
    let default = DefaultMode::new("https://sched.example.org:7301", Vec::new());
    let mut scripts = Vec::new();
    for steps in [
        default.describe(&ctx(&host, TEN_MINUTES)),
        tailscale.describe(&ctx(&host, TEN_MINUTES)),
    ] {
        let mut plan = plan(&host);
        plan.network = steps;
        scripts.push(masked(&plan));
    }
    for script in &scripts {
        let unit = unit_of(script);
        assert!(unit.lines().any(|line| line == "Restart=always"), "{unit}");
        assert!(!unit.contains("RestartPreventExitStatus"), "{unit}");
        assert!(!unit.contains("SuccessExitStatus"), "{unit}");
    }
}

/// Asserts that the agent's unit names the container program, the state root and the
/// scratch root, requires the scratch root's mount, and that the script makes both roots
/// before the unit is written: the agent does not start on a machine whose provider did
/// not mount a file system under the scratch root.
#[test]
fn the_agent_unit_names_its_roots_and_requires_the_scratch_mount() {
    let host = host();
    let script = masked(&plan(&host));
    let unit = unit_of(&script);
    let required = format!("RequiresMountsFor={}", js_elastic::SCRATCH_ROOT);
    assert!(unit.lines().any(|line| line == required), "{unit}");
    let exec = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .unwrap();
    for (flag, value) in [
        ("--container-bin", js_elastic::CONTAINER_BIN),
        ("--state-root", js_elastic::STATE_ROOT),
        ("--scratch-root", js_elastic::SCRATCH_ROOT),
    ] {
        assert!(exec.contains(&format!("{flag} {value}")), "{exec}");
    }
    let made = script
        .find(&format!(
            "install -d -m 0700 -- {} {}",
            js_elastic::STATE_ROOT,
            js_elastic::SCRATCH_ROOT
        ))
        .unwrap();
    assert!(made < script.find("[Unit]").unwrap());
}
