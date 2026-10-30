//! Pieces the experiments share: the allow-list, the fixture of endpoints, entering a
//! container's namespaces to configure them, and the assertions every egress path must pass.

use std::collections::BTreeMap;
use std::process::Output;

use crate::bundle::Outcome;
use crate::host::{self, HostAddrs, Servers, Sysctl, endpoints};

/// The allow-list, as nft loads it: the table the job must not be able to change.
///
/// Output from the namespace may go to the two allowed endpoints and nowhere else: the
/// metadata range is refused first and by name, IPv6 is dropped, everything else is rejected
/// (a refusal, not a silent drop, so a probe sees it at once). Every rule counts, so the host
/// can tell that a refusal was the rule's doing.
pub const ALLOW_LIST: &str = "table inet js_egress {\n\
  chain output {\n\
    type filter hook output priority 0; policy drop;\n\
    oifname \"lo\" accept\n\
    ct state established,related accept\n\
    meta nfproto ipv6 counter drop comment \"ipv6\"\n\
    ip daddr 169.254.0.0/16 counter reject with icmp type admin-prohibited comment \"metadata\"\n\
    ip daddr 198.51.100.10 tcp dport 9300 counter accept comment \"allow-a\"\n\
    ip daddr 198.51.100.20 tcp dport 9443 counter accept comment \"allow-b\"\n\
    meta nfproto ipv4 counter reject with icmp type admin-prohibited comment \"default-reject\"\n\
  }\n\
  chain input {\n\
    type filter hook input priority 0; policy drop;\n\
    iifname \"lo\" accept\n\
    ct state established,related accept\n\
  }\n\
}\n";

/// Host addresses, listeners and the sysctl that lets a listener take port 80, for one test.
pub struct Fixture {
    _port: Sysctl,
    _addrs: HostAddrs,
    _servers: Servers,
}

impl Fixture {
    /// Makes the five endpoints exist on the host.
    pub fn new() -> Self {
        let port = Sysctl::set("net.ipv4.ip_unprivileged_port_start", "0");
        let addrs = HostAddrs::add(&endpoints::ADDRS);
        let servers = Servers::start(&[
            endpoints::ALLOWED_A,
            endpoints::OTHER_PORT,
            endpoints::ALLOWED_B,
            endpoints::OTHER_HOST,
            endpoints::METADATA,
        ]);
        Self {
            _port: port,
            _addrs: addrs,
            _servers: servers,
        }
    }
}

/// Enters the namespaces of `pid` as the same user (the owner of its user namespace, so it
/// has every capability there) and runs `program args` with `stdin`.
pub fn enter_as_owner(pid: u32, program: &str, args: &[&str], stdin: &str) -> Output {
    let pid = pid.to_string();
    let program = host::bin(program);
    let mut all = vec![
        "--target",
        pid.as_str(),
        "--user",
        "--net",
        "--",
        program.as_str(),
    ];
    all.extend_from_slice(args);
    host::run_with_stdin(&host::bin("nsenter"), &all, stdin)
}

/// The same as root (the privileged helper), without entering the user namespace.
pub fn enter_as_root(pid: u32, program: &str, args: &[&str], stdin: &str) -> Output {
    let pid = pid.to_string();
    let resolved = host::bin(program);
    let mut all = vec!["--target", pid.as_str(), "--net", "--", resolved.as_str()];
    all.extend_from_slice(args);
    host::helper("nsenter", &all, stdin)
}

/// The packet count of the rule commented `comment` in an `nft list table` text.
pub fn counter(listing: &str, comment: &str) -> Option<u64> {
    let line = listing
        .lines()
        .find(|line| line.contains(&format!("comment \"{comment}\"")))?;
    let after = line.split("packets ").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

/// What the host reads back from the namespace's table after the job ran.
pub fn listing(output: &Output) -> String {
    host::text(output)
}

/// Whether a probe value is a successful connect.
pub fn connected(value: &str) -> bool {
    value == "ok"
}

/// The probe's view of the five endpoints, `before` or `after` the tamper attempts.
pub fn reach(outcome: &Outcome, phase: &str) -> BTreeMap<&'static str, String> {
    [
        ("allowed-a", endpoints::ALLOWED_A),
        ("allowed-b", endpoints::ALLOWED_B),
        ("other-port", endpoints::OTHER_PORT),
        ("other-host", endpoints::OTHER_HOST),
        ("metadata", endpoints::METADATA),
    ]
    .into_iter()
    .map(|(name, target)| (name, outcome.get(&format!("{phase}.{target}"))))
    .collect()
}

/// Asserts what an enforced allow-list gives, `phase` being `before` or `after` the job's
/// attempts to change it: the two allowed endpoints answer; another port, another host and
/// the metadata address are refused.
pub fn assert_allow_list_enforced(outcome: &Outcome, phase: &str) {
    let seen = reach(outcome, phase);
    assert!(
        connected(&seen["allowed-a"]),
        "{phase}: allowed-a: {seen:?}\n{outcome:?}"
    );
    assert!(
        connected(&seen["allowed-b"]),
        "{phase}: allowed-b: {seen:?}"
    );
    for name in ["other-port", "other-host", "metadata"] {
        assert!(
            !connected(&seen[name]) && seen[name] != "-",
            "{phase}: {name} was not refused: {seen:?}"
        );
    }
}

/// Asserts that IPv6 is off in the job's namespace: no address but loopback, the switch set,
/// and no connect.
pub fn assert_ipv6_off(outcome: &Outcome) {
    assert_eq!(outcome.get("net.ipv6.addrs"), "0", "{outcome:?}");
    assert_ne!(outcome.get("net.ipv6.connect"), "ok");
    assert_ne!(outcome.get("net.ipv6.connect"), "-");
    let switch = outcome.get("net.ipv6.disable_all");
    assert!(
        switch == "1" || switch == "absent",
        "disable_ipv6 = {switch}"
    );
}

/// Asserts that every attempt to change the firewall and the links failed in the job.
pub fn assert_job_cannot_tamper(outcome: &Outcome) {
    assert_eq!(
        outcome.get("tamper.succeeded"),
        "0",
        "{:?}",
        outcome.probe()
    );
    for key in [
        "nft_flush",
        "nft_delete_table",
        "nft_add_accept",
        "ip_lo_down",
        "ip_route_flush",
    ] {
        let value = outcome.get(&format!("tamper.{key}"));
        assert!(
            value.contains("Operation not permitted"),
            "tamper.{key} was not refused with EPERM: {value}"
        );
    }
}

/// Records every probe key of the suite as a result row of `experiment`.
pub fn record_suite(experiment: &str, outcome: &Outcome) {
    for (key, value) in outcome.probe() {
        crate::report::row(experiment, &key, value);
    }
}

/// A process that keeps a user namespace and a network namespace alive, both owned by the
/// runner user: what a non-root daemon can make with no privilege at all (`unshare`), then
/// give the ids of its subordinate range with `newuidmap`/`newgidmap`.
pub struct Holder {
    child: std::process::Child,
}

impl Holder {
    /// Starts the holder and maps container ids 0..65535 onto the runner's subordinate ids.
    pub fn start() -> Self {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("own path"));
        cmd.args([
            "--exact",
            "probe_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("JS_EGRESS_PROBE", "hold")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        // SAFETY: the closure makes one system call.
        unsafe {
            cmd.pre_exec(|| {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let child = cmd
            .spawn()
            .unwrap_or_else(|error| panic!("holder: unshare failed: {error}"));
        let pid = child.id().to_string();
        let (uid, gid) = crate::bundle::sub_ids();
        for (tool, first) in [("newuidmap", uid), ("newgidmap", gid)] {
            let output = std::process::Command::new(host::bin(tool))
                .args([pid.as_str(), "0", &first.to_string(), "65536"])
                .output()
                .expect("run the id map tool");
            assert!(output.status.success(), "{tool}: {}", host::text(&output));
        }
        Self { child }
    }

    /// The holder's pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The path of one of its namespace files.
    pub fn ns(&self, kind: &str) -> String {
        format!("/proc/{}/ns/{kind}", self.pid())
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A spec whose container joins the holder's user and network namespaces by path.
pub fn pair_spec(name: &str, probe: &str, holder: &Holder) -> crate::bundle::Spec {
    let mut spec = crate::bundle::Spec::new(name, probe);
    spec.net = crate::bundle::Join::Path(holder.ns("net"));
    spec.user = crate::bundle::Join::Path(holder.ns("user"));
    spec.env
        .push(("JS_EGRESS_TARGETS".to_owned(), endpoints::ALL.to_owned()));
    spec
}

/// The table as the daemon reads it back from the holder's namespace.
pub fn table_of(holder: &Holder) -> String {
    listing(&enter_as_owner(
        holder.pid(),
        "nft",
        &["list", "table", "inet", "js_egress"],
        "",
    ))
}
