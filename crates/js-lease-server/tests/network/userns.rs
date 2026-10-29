//! The user the jobs' ids are mapped onto, through the rendered script: the agent
//! refuses to start with `--container-bin` and without `--container-subuid`, and refuses
//! a range that is too small, begins at 0 or overlaps a host id, so the script makes the
//! user, the entries and the daemon's remap before the agent's unit exists.

use js_lease_server::bootstrap::{
    DOCKER_DROPIN, Fill, SUBID_COUNT, SUBID_START, SUBID_USER, render,
};
use js_lease_server::network::{DefaultMode, NetworkMode};

use crate::support::{TEN_MINUTES, ctx, host, plan};

fn script() -> String {
    let host = host();
    let mut plan = plan(&host);
    plan.network = DefaultMode::new("https://sched.example.org:7301", Vec::new())
        .describe(&ctx(&host, TEN_MINUTES));
    render(&plan, Fill::Mask).unwrap()
}

/// Asserts that the agent's unit passes `--container-subuid` with the user the script
/// made, once: without the flag the agent refuses to start and no launched host joins.
#[test]
fn the_agent_unit_passes_the_subuid_user() {
    let script = script();
    let exec = script
        .lines()
        .find(|line| line.starts_with("ExecStart=/usr/local/bin/js-agent"))
        .unwrap();
    assert_eq!(SUBID_USER, "js-jobs");
    assert_eq!(exec.matches("--container-subuid").count(), 1, "{exec}");
    assert!(
        exec.contains(&format!("--container-subuid {SUBID_USER} ")),
        "{exec}"
    );
    assert!(script.contains(&format!(
        "useradd --system --no-create-home --shell /usr/sbin/nologin {SUBID_USER}\n"
    )));
}

/// Asserts the entry the agent's start-up check accepts: the same entry in both id
/// files, exactly one range of exactly 65536 ids (Docker maps the whole allocation),
/// not from 0, below the largest id, and above the ids `useradd` gives login users
/// (from 100000, 65536 each, so ten login users end at 750000).
#[test]
fn the_entry_is_one_range_of_65536_in_both_files() {
    let script = script();
    assert_eq!(SUBID_COUNT, 65_536);
    assert!(SUBID_START >= 750_000, "inside the login users' ranges");
    assert!(u64::from(SUBID_START) + u64::from(SUBID_COUNT) < u64::from(u32::MAX));
    for file in ["/etc/subuid", "/etc/subgid"] {
        let line = format!("printf '%s\\n' {SUBID_USER}:{SUBID_START}:{SUBID_COUNT} >> {file}\n");
        assert_eq!(script.matches(&line).count(), 1, "{line}");
    }
}

/// Asserts the daemon runs under the remap of that same user: a drop-in that clears the
/// stock `ExecStart=` before it sets its own, the daemon restarted after the unit files
/// are reloaded, all before the agent's unit is written.
#[test]
fn the_daemon_is_restarted_under_the_remap_before_the_agent() {
    let script = script();
    let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let dropin = {
        let opens = format!("cat > {DOCKER_DROPIN} <<'REMAP'\n");
        let start = at(&opens) + opens.len();
        let end = script[start..].find("\nREMAP\n").unwrap();
        &script[start..start + end + 1]
    };
    let lines: Vec<&str> = dropin.lines().collect();
    assert_eq!(lines[0], "[Service]");
    assert_eq!(lines[1], "ExecStart=");
    assert!(
        lines[2].starts_with("ExecStart=/usr/bin/dockerd "),
        "{dropin}"
    );
    assert!(lines[2].ends_with(&format!(" --userns-remap={SUBID_USER}")));
    assert_eq!(lines.len(), 3);
    let order = [
        "# container user namespace".to_owned(),
        format!("cat > {DOCKER_DROPIN}"),
        "systemctl restart docker.service".to_owned(),
        "# identity".to_owned(),
        "cat > /etc/systemd/system/js-agent.service".to_owned(),
    ];
    let places: Vec<usize> = order.iter().map(|n| at(n)).collect();
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
    let reload = script[at(&format!("cat > {DOCKER_DROPIN}"))..]
        .find("systemctl daemon-reload\n")
        .unwrap();
    let restart = script[at(&format!("cat > {DOCKER_DROPIN}"))..]
        .find("systemctl restart docker.service\n")
        .unwrap();
    assert!(reload < restart);
}
