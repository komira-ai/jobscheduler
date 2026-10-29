//! The user whose subordinate ids every job's user namespace is mapped onto.
//!
//! The agent refuses to start with `--container-bin` and without `--container-subuid`
//! (`SettingsError::NoSubuid`), and it refuses to start unless `/etc/subuid` and
//! `/etc/subgid` give that user at least 65536 ids, not from 0, none of them an id of
//! `/etc/passwd` or `/etc/group`, and, for Docker, exactly one entry of exactly 65536
//! ids with the daemon running `userns-remap` set to that user. The stock image has none
//! of this, so the script makes it before the agent's unit exists:
//!
//! - a check that neither id file holds an entry for [`SUBID_USER`] or any entry that
//!   overlaps the range, which stops the script before it writes anything;
//! - a system user, [`SUBID_USER`] (a system user gets no entry of its own, so the only
//!   entry is the one written here);
//! - the entry `js-jobs:2000000000:65536` in both id files (a high region that neither
//!   `useradd`'s login users, from 100000, nor LXD's conventional
//!   `root:1000000:1000000000` reaches, and far below the largest id);
//! - the key `"userns-remap": "js-jobs"` merged into `/etc/docker/daemon.json` with every
//!   other key kept (the NVIDIA toolkit writes `runtimes` there), refused when the file
//!   already names a different remap, and a restart of the daemon.
//!
//! The merge is one constant `python3` heredoc (python3 is on the stock Ubuntu image), so
//! the script names no path of `dockerd` and no unit file: the agent reads the remap from
//! `docker info`, which reports it however the daemon was configured. Every line is a
//! constant.

use super::Writer;

/// The user the jobs' ids are mapped onto, passed as `--container-subuid`.
pub const SUBID_USER: &str = "js-jobs";

/// The first subordinate id of the range.
pub const SUBID_START: u32 = 2_000_000_000;

/// How many ids the range holds: the ids 0 to 65535 of a job's namespace.
pub const SUBID_COUNT: u32 = 65_536;

/// The daemon's configuration file, which the remap is merged into.
pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";

/// The merge, with `@USER@` where the remap user goes. Reads the file (absent means an
/// empty object), refuses another remap, sets the key and replaces the file by a rename
/// of a complete temporary file.
const MERGE: &str = r#"import json, os, sys
path = "/etc/docker/daemon.json"
user = "@USER@"
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
"#;

/// The merge script with the user filled in.
pub fn merge() -> String {
    MERGE.replace("@USER@", SUBID_USER)
}

/// The line that stops the script when an id file already holds an entry for the user or
/// an entry that overlaps the range.
fn check() -> String {
    let end = u64::from(SUBID_START) + u64::from(SUBID_COUNT);
    let range = format!("{SUBID_START}:{SUBID_COUNT}");
    format!(
        "for f in /etc/subuid /etc/subgid; do awk -F: -v u={SUBID_USER} -v lo={SUBID_START} -v hi={end} '$1 == u || ($2 + 0 < hi + 0 && $2 + $3 > lo + 0) {{ bad = 1 }} END {{ exit bad }}' \"$f\" || {{ echo \"bootstrap: $f is unreadable, holds an entry for {SUBID_USER}, or holds one overlapping {range}\" >&2; exit 1; }}; done"
    )
}

/// Writes the lines that check the id files, make the user and the id entries, merge the
/// remap into the daemon's configuration and restart the daemon. Any failing line stops
/// the script (`set -e`).
pub(super) fn write(w: &mut Writer<'_>) {
    let entry = format!("{SUBID_USER}:{SUBID_START}:{SUBID_COUNT}");
    w.line("# container user namespace");
    w.line(&check());
    w.line(&format!(
        "useradd --system --no-create-home --shell /usr/sbin/nologin {SUBID_USER}"
    ));
    w.line(&format!("printf '%s\\n' {entry} >> /etc/subuid"));
    w.line(&format!("printf '%s\\n' {entry} >> /etc/subgid"));
    w.line("mkdir -p /etc/docker");
    w.line("python3 - <<'PY'");
    w.out.push_str(&merge());
    w.line("PY");
    w.line("systemctl restart docker.service");
}
