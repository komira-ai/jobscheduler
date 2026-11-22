//! Full Disk Access: whether the agent holds it, the question it answers for itself at
//! start, how the answer is reported, and the refusal of every native job without it.
//!
//! macOS's privacy controls (TCC: Full Disk Access and the like) let a process do what its
//! *responsible* process was granted, and a process answers for whoever started it unless
//! its starter disclaims that when it starts it. The agent is its own launchd job, so it
//! answers for itself, and it is the one program of the package that holds a grant
//! (`docs/design/mac-one-program.md`): the release signs it, and a privacy profile that
//! device management installs (`packaging/js-agent-macos/mdm/`) gives Full Disk Access to it
//! by its installed path and its designated requirement. The agent holds it for deleting a
//! job's account, its home and its per-user folders (`docs/design/mac-account-per-job.md`),
//! which root without it may not do. The root helper `js-mac-session` is granted nothing: the
//! pool's reset in place, which it does, neither needs nor uses the grant.
//!
//! The job path keeps the responsibility: nothing between the agent and the job disclaims it.
//! The helper starts each run's start mode (`js-agent __job-start`) disclaimed, so the helper's
//! own grants (it holds none) never reach a run; that process is the agent's own program, and it
//! executes the run's program in its place with the responsibility kept
//! (`js_mac_session::disclaim::exec_kept`), so the run's processes, and the processes they
//! start, answer for the agent's program and hold its Full Disk Access. The one-program run
//! (`mac_job`) starts the same mode with the agent as the starter and keeps the responsibility
//! too. What keeps a run off what it may not read is its Seatbelt profile, not the disclaim.
//! The programs the agent starts for itself (its probes, the start-mode self-test, and `pfctl`
//! through `js_mac_provision`) and the helper's own (`launchctl`, `getconf`) are still
//! disclaimed. `cargo test -p xtask --test spawn_sites` fails when the agent, the helper or a
//! crate built into them starts a program another way. On a Mac, the tests read the responsible
//! process of what the helper's `run` and the agent start (`tests/agent/native/disclaim.rs`).
//! The check by hand is in `packaging/js-agent-macos/README.md`;
//! `docs/design/spawn-disclaim.md` has the design.
//!
//! The agent finds out at start whether it holds the grant the way every program does
//! ([`list`], then [`ask`]): it lists the directory of the privacy database
//! ([`PRIVACY_DIR`]), which only a process with Full Disk Access may list. It reports the
//! answer in its hello as the capability `full-disk-access` ([`report`]): `granted`,
//! `not-granted`, or `undetermined` (the listing failed for another reason). The job service
//! shows the capability in its host listing, so an operator sees which Macs lack the grant.
//! The helper's verb `full-disk-access` still answers for the helper, which holds no grant;
//! the agent no longer asks it.
//!
//! There is one mode. Only `granted` runs native jobs: on any other state every start is
//! refused first, `no-full-disk-access`, before a user, a journal line or a bundle pin is
//! made ([`admit`]), so a Mac whose agent cannot delete what a job leaves never runs one, by
//! accident or otherwise. The state is
//! the one found at start: a grant given or taken away afterwards is seen at the agent's
//! next start, which the operator makes by restarting the agent.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use js_agent_wire::refusal;
use js_caps::FULL_DISK_ACCESS;

pub use js_mac_session::full_disk_access::PRIVACY_DIR;
use js_mac_session::full_disk_access::{self, FullDiskAccess};

use crate::lifecycle::NativeRefusal;

/// Lists `dir`, the directory of the privacy database ([`PRIVACY_DIR`]) on a Mac: opens it
/// and reads one entry name. No file in it is opened and no database is read.
///
/// # Errors
///
/// The directory could not be opened or read: [`io::ErrorKind::PermissionDenied`] when
/// macOS refuses the listing to a process without Full Disk Access, another kind when it is
/// missing or not a directory. [`ask`] judges the result.
pub fn list(dir: &Path) -> io::Result<()> {
    std::fs::read_dir(dir)?.next().transpose().map(|_| ())
}

/// Whether the agent holds Full Disk Access, as a listing of the privacy database's
/// directory ([`list`]) shows: a listing that worked is a grant, a refusal for want of
/// permission is none, and any other failure is [`FullDiskAccess::Undetermined`], whose
/// cause is logged. The agent asks once, at start; the answer is logged as
/// `full disk access: <state>`.
#[must_use]
pub fn ask(listed: io::Result<()>) -> FullDiskAccess {
    let (state, why) = full_disk_access::judge(listed);
    match why {
        None => tracing::info!("full disk access: {}", state.as_str()),
        Some(why) => tracing::warn!("full disk access: {} ({why})", state.as_str()),
    }
    state
}

/// Puts `state` into `capabilities` as the capability `full-disk-access`.
pub fn report(capabilities: &mut BTreeMap<String, String>, state: FullDiskAccess) {
    capabilities.insert(FULL_DISK_ACCESS.to_owned(), state.as_str().to_owned());
}

/// Refuses a native start unless the agent holds Full Disk Access: `no-full-disk-access`,
/// with a text that names the state and what to do.
pub(super) fn admit(state: FullDiskAccess) -> Result<(), NativeRefusal> {
    match state {
        FullDiskAccess::Granted => Ok(()),
        other => Err(NativeRefusal {
            code: refusal::NO_FULL_DISK_ACCESS,
            why: format!(
                "js-agent does not hold Full Disk Access ({}): install the privacy profile \
                 packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig through \
                 device management and restart the agent",
                other.as_str()
            ),
        }),
    }
}
