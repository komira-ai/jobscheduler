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

/// Where the agent's unit starts in a masked script (the metadata block's unit comes
/// before it and has a `[Unit]` of its own).
fn agent_unit_at(script: &str) -> usize {
    let at = script
        .find("cat > /etc/systemd/system/js-agent.service <<'UNIT'")
        .unwrap();
    at + script[at..].find("[Unit]").unwrap()
}

/// The agent's unit section of a masked script: from `[Unit]` to the `UNIT` line.
fn unit_of(script: &str) -> &str {
    let start = agent_unit_at(script);
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
        ("--runtime", js_elastic::RUNTIME),
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
    assert!(made < agent_unit_at(&script));
}

/// The masked tailscale script as lines.
fn tailscale_lines() -> Vec<String> {
    let host = host();
    let fake = Fake::empty();
    let (_, shared) = clock();
    let mut plan = plan(&host);
    plan.network = mode(&fake, shared).describe(&ctx(&host, TEN_MINUTES));
    masked(&plan).lines().map(str::to_owned).collect()
}

/// Asserts that the wait for the daemon's socket comes after the daemon is started and
/// before `tailscale up`, that it is a loop of at most 30 one-second checks, and that
/// it ends in a `logger` line and a non-zero exit. A wait moved, widened to no bound,
/// or without its log line fails here.
#[test]
fn the_socket_wait_follows_the_daemon_and_precedes_up_and_is_bounded() {
    let lines = tailscale_lines();
    let at = |prefix: &str| {
        let found: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].starts_with(prefix))
            .collect();
        assert_eq!(found.len(), 1, "{prefix}");
        found[0]
    };
    let daemon = at("systemd-run --unit=tailscaled ");
    let wait = at("sh -c 'i=0; until [ -S ");
    let up = at("/usr/local/bin/tailscale up ");
    assert!(daemon < wait && wait < up, "{daemon} {wait} {up}");
    let line = &lines[wait];
    assert!(line.contains(r#"[ "$i" -gt 30 ]"#), "{line}");
    assert!(line.contains("sleep 1"), "{line}");
    assert!(line.contains("logger -t jobscheduler"), "{line}");
    assert!(line.contains("exit 1"), "{line}");
    assert!(
        line.ends_with(" sh /var/run/tailscale/tailscaled.sock"),
        "the socket is an argument, not part of the program: {line}"
    );
}

/// Asserts that `tailscale up` accepts the tailnet's DNS, so the host resolves the
/// MagicDNS name of the server's advertised URL, and never turns it off.
#[test]
fn tailscale_up_accepts_dns() {
    let lines = tailscale_lines();
    let up = lines
        .iter()
        .find(|l| l.starts_with("/usr/local/bin/tailscale up "))
        .unwrap();
    assert!(up.ends_with(" --accept-dns=true"), "{up}");
    assert!(!up.contains("--accept-dns=false"), "{up}");
}
