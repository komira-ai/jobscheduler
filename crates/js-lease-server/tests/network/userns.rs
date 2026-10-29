//! The user the jobs' ids are mapped onto, through the rendered script: the agent
//! refuses to start with `--container-bin` and without `--container-subuid`, and refuses
//! a range that is too small, begins at 0 or overlaps a host id, so the script makes the
//! user, the entries and the daemon's remap before the agent's unit exists.

use js_agent::cli_runtime::{Mapping, RANGE_LEN};
use js_lease_server::bootstrap::{Fill, SUBID_COUNT, SUBID_START, SUBID_USER, render};
use js_lease_server::network::{DefaultMode, NetworkMode};

use crate::support::{TEN_MINUTES, ctx, host, plan};

fn script() -> String {
    let host = host();
    let mut plan = plan(&host);
    plan.network = DefaultMode::new("https://sched.example.org:7301", Vec::new())
        .describe(&ctx(&host, TEN_MINUTES));
    render(&plan, Fill::Mask).unwrap()
}

/// The lines of the script from the user namespace heading to the identity heading.
fn section(script: &str) -> &str {
    let start = script.find("# container user namespace\n").unwrap();
    let end = script.find("# identity\n").unwrap();
    &script[start..end]
}

/// The whole user namespace section, pinned character for character: the check, the user,
/// both entries, the constant `python3` merge into `daemon.json` and the restart. A change
/// to any line, including a dropped or reordered one, is a change to this text.
const SECTION: &str = r#"# container user namespace
for f in /etc/subuid /etc/subgid; do awk -F: -v u=js-jobs -v lo=2000000000 -v hi=2000065536 '$1 == u || ($2 + 0 < hi + 0 && $2 + $3 > lo + 0) { bad = 1 } END { exit bad }' "$f" || { echo "bootstrap: $f is unreadable, holds an entry for js-jobs, or holds one overlapping 2000000000:65536" >&2; exit 1; }; done
useradd --system --no-create-home --shell /usr/sbin/nologin js-jobs
printf '%s\n' js-jobs:2000000000:65536 >> /etc/subuid
printf '%s\n' js-jobs:2000000000:65536 >> /etc/subgid
mkdir -p /etc/docker
python3 - <<'PY'
import json, os, sys
path = "/etc/docker/daemon.json"
user = "js-jobs"
try:
    with open(path) as f:
        conf = json.load(f)
except FileNotFoundError:
    conf = {}
if not isinstance(conf, dict):
    sys.exit("bootstrap: " + path + " is not a JSON object")
if conf.get("userns-remap", user) != user:
    sys.exit("bootstrap: " + path + " already sets userns-remap to " + str(conf["userns-remap"]))
conf["userns-remap"] = user
tmp = path + ".js-tmp"
with open(tmp, "w") as f:
    json.dump(conf, f, indent=2)
    f.write("\n")
os.chmod(tmp, 0o644)
os.rename(tmp, path)
PY
systemctl restart docker.service
"#;

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
/// files, exactly one range of exactly 65536 ids, not from 0, ending far below the
/// largest id, and in a high region neither `useradd`'s login users (from 100000) nor
/// LXD's conventional `root:1000000:1000000000` reaches.
#[test]
fn the_entry_is_one_range_of_65536_in_both_files() {
    let script = script();
    assert_eq!(SUBID_COUNT, 65_536);
    assert!(
        std::hint::black_box(SUBID_START) >= 1_001_000_000,
        "the start is inside the range LXD conventionally gives root (1000000 to 1001000000)"
    );
    assert!(u64::from(SUBID_START) + u64::from(SUBID_COUNT) < u64::from(u32::MAX) - 65_536);
    for file in ["/etc/subuid", "/etc/subgid"] {
        let line = format!("printf '%s\\n' {SUBID_USER}:{SUBID_START}:{SUBID_COUNT} >> {file}\n");
        assert_eq!(script.matches(&line).count(), 1, "{line}");
    }
}

/// Asserts the whole section as text: the daemon's remap is merged by the constant
/// heredoc, no `chmod` or drop-in line is left, and nothing derived at run time is in it.
#[test]
fn the_section_is_pinned_exactly() {
    let script = script();
    assert_eq!(section(&script), SECTION);
    assert!(!script.contains("dockerd"));
    assert!(!script.contains("docker.service.d"));
}

/// Asserts the order: the check, then the user, then both id lines, then the daemon
/// configuration, then the restart, then the identity and the agent's unit.
#[test]
fn the_daemon_is_restarted_under_the_remap_before_the_agent() {
    let script = script();
    let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
    let order = [
        "# container user namespace\n".to_owned(),
        "for f in /etc/subuid /etc/subgid; do awk".to_owned(),
        format!("useradd --system --no-create-home --shell /usr/sbin/nologin {SUBID_USER}\n"),
        format!("{SUBID_USER}:{SUBID_START}:{SUBID_COUNT} >> /etc/subuid\n"),
        format!("{SUBID_USER}:{SUBID_START}:{SUBID_COUNT} >> /etc/subgid\n"),
        "mkdir -p /etc/docker\n".to_owned(),
        "python3 - <<'PY'\n".to_owned(),
        "\nPY\n".to_owned(),
        "systemctl restart docker.service\n".to_owned(),
        "# identity\n".to_owned(),
        "cat > /etc/systemd/system/js-agent.service".to_owned(),
    ];
    let places: Vec<usize> = order.iter().map(|n| at(n)).collect();
    assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
}

/// Asserts the entry against the agent's own Docker rule (`read_exact`), fed the entry the
/// script writes beside the files of a stock image (an `ubuntu` user with
/// `ubuntu:100000:65536`): accepted, onto the range the script names. A range the agent
/// would refuse (overlapping `ubuntu`, a passwd id inside it) is caught here, not on a
/// live host.
#[test]
fn the_agents_docker_rule_accepts_the_entry() {
    let script = script();
    let entry = format!("{SUBID_USER}:{SUBID_START}:{SUBID_COUNT}");
    assert_eq!(RANGE_LEN, SUBID_COUNT);
    let written = |file: &str| -> String {
        script
            .lines()
            .filter(|l| l.starts_with("printf '%s\\n' ") && l.ends_with(&format!(" >> {file}")))
            .map(|l| {
                l.trim_start_matches("printf '%s\\n' ")
                    .trim_end_matches(&format!(" >> {file}"))
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(written("/etc/subuid"), entry);
    let passwd = "root:x:0:0:root:/root:/bin/bash\nubuntu:x:1000:1000:Ubuntu:/home/ubuntu:/bin/bash\n\
        js-jobs:x:999:988::/home/js-jobs:/usr/sbin/nologin\n";
    let group = "root:x:0:\nubuntu:x:1000:\njs-jobs:x:988:\n";
    let read = |path: &str| -> Result<String, String> {
        Ok(match path {
            "/etc/subuid" | "/etc/subgid" => {
                format!("ubuntu:100000:65536\n{}\n", written(path))
            }
            "/etc/passwd" => passwd.to_owned(),
            "/etc/group" => group.to_owned(),
            other => return Err(other.to_owned()),
        })
    };
    let mapping = Mapping::read_exact(SUBID_USER, &read).unwrap();
    assert_eq!(mapping, Mapping::new(SUBID_START, SUBID_START));
}
