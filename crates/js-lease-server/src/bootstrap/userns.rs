//! The user whose subordinate ids every job's user namespace is mapped onto.
//!
//! The agent refuses to start with `--container-bin` and without `--container-subuid`
//! (`SettingsError::NoSubuid`), and it refuses to start unless `/etc/subuid` and
//! `/etc/subgid` give that user at least 65536 ids, not from 0, none of them an id of
//! `/etc/passwd` or `/etc/group`, and, for Docker, exactly one entry of exactly 65536
//! ids with the daemon running `userns-remap` set to that user. The stock image has none
//! of this, so the script makes it before the agent's unit exists:
//!
//! - a system user, [`SUBID_USER`] (a system user gets no entry of its own, so the only
//!   entry is the one written here);
//! - the entry `js-jobs:1000000:65536` in both id files (above the ranges `useradd` gives
//!   login users, which start at 100000, and far below the largest id);
//! - a drop-in of `docker.service` that sets `--userns-remap` on the daemon's command
//!   line, so the image's `daemon.json` (its runtimes) is not edited, and a restart of
//!   the daemon.
//!
//! The drop-in repeats the daemon's stock command line (`-H fd://` and the containerd
//! socket); that line is UNVERIFIED for the stock Lambda image (design, Open questions 5)
//! and a live run confirms it. Every line is a constant.

use super::Writer;

/// The user the jobs' ids are mapped onto, passed as `--container-subuid`.
pub const SUBID_USER: &str = "js-jobs";

/// The first subordinate id of the range.
pub const SUBID_START: u32 = 1_000_000;

/// How many ids the range holds: the ids 0 to 65535 of a job's namespace.
pub const SUBID_COUNT: u32 = 65_536;

/// The drop-in that sets the daemon's user namespace remap.
pub const DOCKER_DROPIN: &str = "/etc/systemd/system/docker.service.d/js-userns.conf";

/// The daemon's command line with the remap.
fn dockerd() -> String {
    format!(
        "/usr/bin/dockerd -H fd:// --containerd=/run/containerd/containerd.sock --userns-remap={SUBID_USER}"
    )
}

/// The drop-in, as systemd reads it: the empty `ExecStart=` clears the stock command.
pub fn dropin() -> String {
    format!("[Service]\nExecStart=\nExecStart={}\n", dockerd())
}

/// Writes the lines that make the user and the id entries and restart the daemon under
/// the remap. Any failing line stops the script (`set -e`).
pub(super) fn write(w: &mut Writer<'_>) {
    let entry = format!("{SUBID_USER}:{SUBID_START}:{SUBID_COUNT}");
    w.line("# container user namespace");
    w.line(&format!(
        "useradd --system --no-create-home --shell /usr/sbin/nologin {SUBID_USER}"
    ));
    w.line(&format!("printf '%s\\n' {entry} >> /etc/subuid"));
    w.line(&format!("printf '%s\\n' {entry} >> /etc/subgid"));
    w.line("mkdir -p /etc/systemd/system/docker.service.d");
    w.line(&format!("cat > {DOCKER_DROPIN} <<'REMAP'"));
    w.out.push_str(&dropin());
    w.line("REMAP");
    w.line(&format!("chmod 0644 {DOCKER_DROPIN}"));
    w.line("systemctl daemon-reload");
    w.line("systemctl restart docker.service");
}
