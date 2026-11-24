//! The agent's unit the bootstrap writes against the unit the js-agent package installs
//! (`packaging/js-agent/js-agent.service`, read from the repository): the settings both carry
//! have the same values, so the elastic host and the packaged host wait for the same engine,
//! need the same scratch mount, drain for as long and restart at the same pace. The packaged
//! unit's comments give the reasons; crates/xtask/tests/js_agent_package.rs pins its values.

use std::collections::BTreeSet;
use std::path::Path;

use js_lease_server::bootstrap::{Fill, METADATA_UNIT, render};

use crate::support::{host, plan};

/// The settings, by section and key, that both units carry once with the same value.
const SHARED: [(&str, &str); 8] = [
    ("Unit", "Description"),
    ("Unit", "Wants"),
    ("Unit", "RequiresMountsFor"),
    ("Unit", "ConditionPathIsMountPoint"),
    ("Service", "TimeoutStopSec"),
    ("Service", "Restart"),
    ("Service", "RestartSec"),
    ("Install", "WantedBy"),
];

/// The packaged unit as it is in the repository.
fn packaged() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/js-agent/js-agent.service");
    std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{}: {why}", path.display()))
}

/// The agent's unit of a masked bootstrap script: the text of its heredoc.
fn bootstrap() -> String {
    let script = render(&plan(&host()), Fill::Mask).unwrap();
    let open = "cat > /etc/systemd/system/js-agent.service <<'UNIT'\n";
    let start = script.find(open).unwrap() + open.len();
    let end = start + script[start..].find("\nUNIT\n").unwrap();
    script[start..=end].to_owned()
}

/// The settings of a unit file as (section, key, value), in file order; blank lines and
/// comments (`#` or `;`) are skipped.
fn entries(text: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        out.push((
            section.clone(),
            key.trim().to_owned(),
            value.trim().to_owned(),
        ));
    }
    out
}

/// Every value `key` has in `section`, in file order.
fn values<'a>(entries: &'a [(String, String, String)], section: &str, key: &str) -> Vec<&'a str> {
    entries
        .iter()
        .filter(|(s, k, _)| s == section && k == key)
        .map(|(_, _, v)| v.as_str())
        .collect()
}

/// The units the `After=` lines of `[Unit]` name, over all its lines.
fn after(entries: &[(String, String, String)]) -> BTreeSet<&str> {
    values(entries, "Unit", "After")
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect()
}

/// Where the bootstrap's unit differs from the packaged one, one line per setting: a
/// [`SHARED`] setting the bootstrap does not carry exactly once with the packaged unit's
/// value, and an ordering other than the packaged unit's plus the metadata block's unit
/// (which only the bootstrap writes). Empty when they agree.
fn drift(bootstrap: &str, packaged: &str) -> Vec<String> {
    let (ours, theirs) = (entries(bootstrap), entries(packaged));
    let mut out = Vec::new();
    for (section, key) in SHARED {
        let (b, p) = (values(&ours, section, key), values(&theirs, section, key));
        if b.len() != 1 || b != p {
            out.push(format!(
                "[{section}] {key}: bootstrap {b:?}, packaged {p:?}"
            ));
        }
    }
    let mut wanted = after(&theirs);
    wanted.insert(METADATA_UNIT);
    let got = after(&ours);
    if got != wanted {
        out.push(format!(
            "[Unit] After: bootstrap {got:?}, packaged and the block {wanted:?}"
        ));
    }
    out
}

/// Asserts that the bootstrap's agent unit has the packaged unit's engine ordering
/// (`docker.service` and `containerd.service`), scratch mount condition, stop timeout and
/// restart pace, and the other settings both carry. It catches either unit changing one of
/// them without the other, such as the bootstrap's former `RestartSec=5`.
#[test]
fn the_bootstrap_agent_unit_has_the_packaged_units_shared_settings() {
    assert_eq!(drift(&bootstrap(), &packaged()), Vec::<String>::new());
}

/// Asserts that a restart pace differing between the units is reported with both values, by
/// the whole line.
#[test]
fn a_different_restart_pace_is_drift() {
    let packaged = packaged().replace("\nRestartSec=30\n", "\nRestartSec=5\n");
    assert_eq!(
        drift(&bootstrap(), &packaged),
        vec![r#"[Service] RestartSec: bootstrap ["30"], packaged ["5"]"#.to_owned()]
    );
}

/// Asserts that a setting one unit lacks is drift, whichever unit lacks it.
#[test]
fn a_setting_missing_from_either_unit_is_drift() {
    let no_timeout = packaged().replace("\nTimeoutStopSec=120\n", "\n");
    assert_eq!(
        drift(&bootstrap(), &no_timeout),
        vec![r#"[Service] TimeoutStopSec: bootstrap ["120"], packaged []"#.to_owned()]
    );
    let bootstrap = bootstrap().replace(
        "\nConditionPathIsMountPoint=/var/lib/js-agent/scratch\n",
        "\n",
    );
    assert_eq!(
        drift(&bootstrap, &packaged()),
        vec![
            r#"[Unit] ConditionPathIsMountPoint: bootstrap [], packaged ["/var/lib/js-agent/scratch"]"#
                .to_owned()
        ]
    );
}

/// Asserts that a setting both units lack is drift: the shared settings are required, so
/// the check cannot pass by both units dropping one.
#[test]
fn a_setting_missing_from_both_units_is_drift() {
    let drop = |text: String| text.replace("\nRestartSec=30\n", "\n");
    assert_eq!(
        drift(&drop(bootstrap()), &drop(packaged())),
        vec!["[Service] RestartSec: bootstrap [], packaged []".to_owned()]
    );
}

/// Asserts that a setting in the wrong section, or given twice (systemd keeps the last), is
/// drift, and that a commented-out line counts for nothing.
#[test]
fn a_setting_in_another_section_or_given_twice_is_drift_and_comments_are_skipped() {
    let condition = "ConditionPathIsMountPoint=/var/lib/js-agent/scratch\n";
    let moved = bootstrap()
        .replace(&format!("\n{condition}"), "\n")
        .replace("[Service]\n", &format!("[Service]\n{condition}"));
    assert_eq!(
        drift(&moved, &packaged()),
        vec![
            r#"[Unit] ConditionPathIsMountPoint: bootstrap [], packaged ["/var/lib/js-agent/scratch"]"#
                .to_owned()
        ]
    );
    let twice = bootstrap().replace("\nRestartSec=30\n", "\nRestartSec=30\nRestartSec=30\n");
    assert_eq!(
        drift(&twice, &packaged()),
        vec![r#"[Service] RestartSec: bootstrap ["30", "30"], packaged ["30"]"#.to_owned()]
    );
    let commented = bootstrap().replace(
        "\nRestartSec=30\n",
        "\n# RestartSec=5\n; RestartSec=5\nRestartSec=30\n",
    );
    assert_eq!(drift(&commented, &packaged()), Vec::<String>::new());
}

/// Asserts that an engine unit missing from the bootstrap's ordering is drift, that the
/// metadata block's unit is required in the bootstrap's ordering, and that the order of the
/// names and how they are split over `After=` lines are not.
#[test]
fn the_ordering_is_compared_as_a_set_with_the_block_added() {
    let no_containerd =
        bootstrap().replace(" docker.service containerd.service\n", " docker.service\n");
    assert_eq!(
        drift(&no_containerd, &packaged()),
        vec![format!(
            r#"[Unit] After: bootstrap {{"docker.service", "{METADATA_UNIT}", "network-online.target"}}, packaged and the block {{"containerd.service", "docker.service", "{METADATA_UNIT}", "network-online.target"}}"#
        )]
    );
    let no_block = bootstrap().replace(&format!("\nAfter={METADATA_UNIT}\n"), "\n");
    assert_eq!(drift(&no_block, &packaged()).len(), 1, "{no_block}");
    let reordered = bootstrap().replace(
        "After=network-online.target docker.service containerd.service\n",
        "After=containerd.service\nAfter=docker.service network-online.target\n",
    );
    assert_eq!(drift(&reordered, &packaged()), Vec::<String>::new());
}

/// Asserts that the bootstrap's stop timeout covers the drain its `ExecStart` asks of the
/// agent, as the packaged unit's comment requires: the drain wait, the stop grace, the
/// agent's wait for the server to acknowledge the results, and 30 seconds for the engine's
/// calls. A flag added to the bootstrap's command line that lengthens the drain fails here
/// unless the timeout grows with it.
#[cfg(target_os = "linux")]
#[test]
fn the_bootstrap_stop_timeout_covers_the_agents_drain() {
    use clap::Parser as _;
    let unit = bootstrap();
    let entries = entries(&unit);
    let exec = values(&entries, "Service", "ExecStart");
    let [exec] = exec.as_slice() else {
        panic!("{unit}")
    };
    // The unit quotes its values; systemd hands the agent the words without the quotes.
    let words = exec.split_whitespace().map(|word| word.trim_matches('"'));
    let cli = js_agent::Cli::try_parse_from(words)
        .unwrap_or_else(|why| panic!("ExecStart is not a command line of js-agent: {why}"));
    let grace = cli
        .stop_grace
        .unwrap_or(js_agent::lifecycle::DEFAULT_STOP_GRACE.as_secs());
    let drain = cli.drain_wait.unwrap_or(grace) + grace + js_agent::DRAIN_SETTLE.as_secs();
    let timeout: u64 = values(&entries, "Service", "TimeoutStopSec")
        .concat()
        .parse()
        .unwrap_or_else(|why| panic!("{unit}: {why}"));
    assert!(
        timeout >= drain + 30,
        "TimeoutStopSec={timeout} for a drain of {drain} s"
    );
}
