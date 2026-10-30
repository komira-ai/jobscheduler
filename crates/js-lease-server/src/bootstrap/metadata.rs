//! The block on the provider's metadata address: what the script writes and starts
//! before the agent's unit exists.
//!
//! A process that reaches the metadata address can read the machine's user data, which
//! holds the join token and, under the tailscale mode, the Tailscale key. The host runs
//! jobs in containers, so the script drops traffic to that address from every container
//! network and from every user but root, with `nftables` (`nft` is part of the Ubuntu 24.04
//! server image; the script fails at `test -x` if it is not there, and the agent never
//! starts).
//!
//! Two chains, both at priority -10 so they run before the container engine's own rules
//! (a drop is final):
//!
//! - `forward`: a container's packets are forwarded by the host, whatever bridge the
//!   engine made, so one rule on the destination covers every bridge network. It does
//!   not cover every network an engine can make: a rootful `slirp4netns` or `pasta`
//!   network egresses as host uid 0, which the `output` chain exempts, and a `macvlan`
//!   or `ipvlan` network never passes the host's forward hook. The agent does not refuse
//!   them by name; the agent's unit passes no network at all, and a pool that names a
//!   network must name a bridge.
//! - `output`: packets of a process on the host, dropped for every uid but 0. The agent
//!   runs as root and the bootstrap's own steps run as root; once the script is done
//!   neither needs the address, so it is not opened to them by name.
//!
//! The rules live in a file loaded by `js-metadata-block.service`, a oneshot enabled for
//! boot, ordered `After=nftables.service` (the image's own service, when enabled, flushes
//! the whole ruleset as it starts) and `Before=` the agent; the agent's unit `Requires=` it, so the agent
//! does not start on a boot where the rules failed to load. The table is deleted and made
//! again on every load, so a second load changes nothing.
//!
//! Nothing here comes from the plan: every line is a constant, so no value can add syntax.

use super::Writer;

/// The metadata address dropped. UNVERIFIED for Lambda (design, Open questions 5): it is
/// the address clouds of this kind use; a live run on the stock image confirms or
/// replaces it. The IPv6 address of the same service is not blocked: the design does not
/// call for it.
pub const METADATA_ADDRESS: &str = "169.254.169.254";

/// Where the rules are written.
pub const METADATA_RULES_PATH: &str = "/etc/jobscheduler/metadata-block.nft";

/// The unit that loads the rules at boot.
pub const METADATA_UNIT: &str = "js-metadata-block.service";

/// The firewall program the unit runs.
pub const NFT_BIN: &str = "/usr/sbin/nft";

/// The name of the table the rules are in.
pub const METADATA_TABLE: &str = "js_metadata";

/// How many `drop` rules the table holds once loaded: the forward chain's and the output
/// chain's. The script reads the table back and fails if it holds another number.
pub const DROP_RULES: usize = 2;

/// The rules file, as the machine reads it.
pub fn rules() -> String {
    let table = METADATA_TABLE;
    let address = METADATA_ADDRESS;
    format!(
        "table inet {table}\n\
         delete table inet {table}\n\
         table inet {table} {{\n\
         \x20 chain forward {{\n\
         \x20   type filter hook forward priority -10; policy accept;\n\
         \x20   ip daddr {address} drop\n\
         \x20 }}\n\
         \x20 chain output {{\n\
         \x20   type filter hook output priority -10; policy accept;\n\
         \x20   meta skuid != 0 ip daddr {address} drop\n\
         \x20 }}\n\
         }}\n"
    )
}

/// The unit that loads the rules.
pub fn unit() -> String {
    format!(
        "[Unit]\n\
         Description=jobscheduler metadata address block\n\
         DefaultDependencies=no\n\
         After=nftables.service\n\
         Before=network-pre.target js-agent.service\n\
         Wants=network-pre.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={NFT_BIN} --file {METADATA_RULES_PATH}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Writes the block's lines: the program checked, the rules and the unit written, the
/// unit enabled and started, and the loaded table read back: both drop rules must be in
/// it (`nft` lists them, `grep` counts the lines that drop, `test` compares with the two
/// the rules file has). Any failing line stops the
/// script (`set -e`) before the agent's unit is written.
pub(super) fn write(w: &mut Writer<'_>) {
    w.line("# metadata block");
    w.line(&format!("test -x {NFT_BIN}"));
    w.line("mkdir -p /etc/jobscheduler");
    w.line(&format!("cat > {METADATA_RULES_PATH} <<'RULES'"));
    w.out.push_str(&rules());
    w.line("RULES");
    w.line(&format!("chmod 0644 {METADATA_RULES_PATH}"));
    w.line(&format!(
        "cat > /etc/systemd/system/{METADATA_UNIT} <<'BLOCK'"
    ));
    w.out.push_str(&unit());
    w.line("BLOCK");
    w.line(&format!("chmod 0644 /etc/systemd/system/{METADATA_UNIT}"));
    w.line("systemctl daemon-reload");
    w.line(&format!("systemctl enable --now {METADATA_UNIT}"));
    w.line(&format!(
        "test \"$({NFT_BIN} list table inet {METADATA_TABLE} | grep -c drop)\" = {DROP_RULES}"
    ));
}
