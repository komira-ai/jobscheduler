//! The block on the metadata address, through the rendered script: the rules, their
//! place before the agent, their persistence across a reboot, and the agent's unit
//! requiring them. That a container really cannot read the address is a live check.

use js_lease_server::bootstrap::{
    Fill, METADATA_ADDRESS, METADATA_RULES_PATH, METADATA_UNIT, NFT_BIN, render,
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

/// The text between a line that opens a heredoc and the line that closes it.
fn heredoc<'a>(script: &'a str, opens: &str, close: &str) -> &'a str {
    let start = script.find(opens).unwrap() + opens.len();
    let end = script[start..].find(&format!("\n{close}\n")).unwrap();
    &script[start..start + end + 1]
}

/// Asserts the rules: one table, a forward chain that drops the destination for every
/// container network, and an output chain that drops it for every uid but root, both
/// running before the container engine's own rules (priority below 0), both with an
/// accept policy so nothing else is dropped. The address is the named constant, and
/// the table is deleted and made again so that a second load changes nothing.
#[test]
fn the_rules_drop_the_address_from_containers_and_from_every_user_but_root() {
    let script = script();
    let rules = heredoc(
        &script,
        &format!("cat > {METADATA_RULES_PATH} <<'RULES'\n"),
        "RULES",
    );
    assert_eq!(METADATA_ADDRESS, "169.254.169.254");
    let lines: Vec<&str> = rules.lines().map(str::trim).collect();
    assert_eq!(lines[0], "table inet js_metadata");
    assert_eq!(lines[1], "delete table inet js_metadata");
    assert!(lines.contains(&"type filter hook forward priority -10; policy accept;"));
    assert!(lines.contains(&"type filter hook output priority -10; policy accept;"));
    assert!(lines.contains(&format!("ip daddr {METADATA_ADDRESS} drop").as_str()));
    assert!(lines.contains(&format!("meta skuid != 0 ip daddr {METADATA_ADDRESS} drop").as_str()));
    assert_eq!(rules.matches(" drop\n").count(), 2, "only these two drops");
    assert_eq!(rules.matches("accept;").count(), 2);
}

/// Asserts the order: the program is checked, the rules and the unit are written, the
/// unit is started and the loaded table is read back, all before the join token is
/// written, the network steps run, the agent is fetched or its unit exists.
#[test]
fn the_block_is_loaded_before_the_agent_exists() {
    let script = script();
    let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let order = [
        format!("test -x {NFT_BIN}"),
        format!("cat > {METADATA_RULES_PATH}"),
        format!("cat > /etc/systemd/system/{METADATA_UNIT}"),
        format!("systemctl enable --now {METADATA_UNIT}"),
        format!("{NFT_BIN} list table inet js_metadata"),
        "# identity".to_owned(),
        "# network".to_owned(),
        "curl ".to_owned(),
        "cat > /etc/systemd/system/js-agent.service".to_owned(),
        "systemctl enable --now js-agent.service".to_owned(),
    ];
    let places: Vec<usize> = order.iter().map(|n| at(n)).collect();
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
    assert!(at("# metadata block") < at("# identity"));
}

/// Asserts that the rules persist across a reboot: they are in a file, a oneshot unit
/// loads that file, the unit is enabled (not only started), stays active after the load,
/// and is ordered before the agent and before the network comes up.
#[test]
fn the_block_persists_and_is_ordered_before_the_agent() {
    let script = script();
    let unit = heredoc(
        &script,
        &format!("cat > /etc/systemd/system/{METADATA_UNIT} <<'BLOCK'\n"),
        "BLOCK",
    );
    let has = |line: &str| unit.lines().any(|l| l == line);
    assert!(has("Type=oneshot") && has("RemainAfterExit=yes"), "{unit}");
    assert!(has(&format!(
        "ExecStart={NFT_BIN} --file {METADATA_RULES_PATH}"
    )));
    assert!(has("WantedBy=multi-user.target"));
    assert!(has("Before=network-pre.target js-agent.service"), "{unit}");
    assert!(script.contains(&format!("systemctl enable --now {METADATA_UNIT}\n")));
}

/// Asserts that the agent's unit requires the block and starts after it, so the agent
/// never starts on a boot where the rules did not load, and that it passes no
/// `--container-network`: containers get the agent's default, no network at all.
#[test]
fn the_agent_unit_requires_the_block_and_gives_containers_no_network() {
    let script = script();
    let unit = heredoc(
        &script,
        "cat > /etc/systemd/system/js-agent.service <<'UNIT'\n",
        "UNIT",
    );
    let has = |line: &str| unit.lines().any(|l| l == line);
    assert!(has(&format!("Requires={METADATA_UNIT}")), "{unit}");
    assert!(has(&format!("After={METADATA_UNIT}")), "{unit}");
    assert!(!unit.contains("--container-network"), "{unit}");
}

/// Asserts that the block holds only constants: a plan whose values hold shell and unit
/// syntax changes nothing between the `# metadata block` line and `# identity`.
#[test]
fn nothing_from_the_plan_reaches_the_block() {
    let host = host();
    let mut odd = plan(&host);
    odd.pool = "p'; rm -rf /; echo '$(id)".to_owned();
    odd.scheduler_url = "https://h.example/%h\"$x".to_owned();
    let section = |text: &str| {
        let start = text.find("# metadata block").unwrap();
        let end = text.find("# identity").unwrap();
        text[start..end].to_owned()
    };
    assert_eq!(
        section(&render(&odd, Fill::Mask).unwrap()),
        section(&script())
    );
}
