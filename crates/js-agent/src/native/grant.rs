//! Full Disk Access: whether the root helper holds it, the question the agent asks at
//! start, how the answer is reported, and the refusal of every native job without it.
//!
//! macOS's privacy controls (TCC: Full Disk Access and the like) let a process do what its
//! *responsible* process was granted, and a process answers for whoever started it unless
//! its starter disclaims that when it starts it. The helper `js-mac-session` is its own
//! launchd job, so it answers for itself, and it is the one program of the agent that holds
//! a grant: the release signs it, and a privacy profile that device management installs
//! (`packaging/js-agent-macos/mdm/`) gives Full Disk Access to it by its installed path and
//! its designated requirement. The helper holds it for deleting a job's account, its home
//! and its per-user folders (`docs/design/mac-account-per-job.md`), which root without it
//! may not do; the pool's reset in place neither needs nor uses it.
//!
//! The grant never reaches a job. Every process of a run is started through
//! `js_mac_session::disclaim`, which disclaims the responsibility, so the run's processes,
//! and the processes they start, answer for the run's program and hold no grant of the
//! helper's or the agent's; what the run may do is what its own program was granted, which
//! this runtime neither gives nor takes away. The same holds for the programs the agent
//! starts itself (its probes, the `sandbox-exec` self-test, and `pfctl` through
//! `js_mac_provision`) and for the helper's own (`launchctl`, `getconf`). `cargo test -p xtask
//! --test spawn_sites` fails when the agent, the helper or a crate built into them starts a
//! program another way. On a Mac, the tests read the responsible process of what the helper's
//! `run` and the agent start (`tests/agent/native/disclaim.rs`). The check by hand is in
//! `packaging/js-agent-macos/README.md`; `docs/design/spawn-disclaim.md` has the design.
//!
//! The agent asks the helper at start ([`ask`]: the verb `full-disk-access`, which the
//! helper answers by listing the privacy database's directory, and which it answers before
//! its boot reset too) and reports the answer in its hello as the capability
//! `full-disk-access` ([`report`]): `granted`, `not-granted`, or `undetermined` (the helper
//! could not tell, or is from before the verb). The job service shows the capability in its
//! host listing, so an operator sees which Macs lack the grant.
//!
//! There is one mode. Only `granted` runs native jobs: on any other state every start is
//! refused first, `no-full-disk-access`, before a user, a journal line or a bundle pin is
//! made ([`admit`]), so a Mac whose helper cannot delete what a job leaves never runs one, by
//! accident or otherwise. The state is
//! the one found at start: a grant given or taken away afterwards is seen at the agent's
//! next start, which the operator makes by restarting the agent.

use std::collections::BTreeMap;

use js_agent_wire::refusal;
use js_caps::FULL_DISK_ACCESS;
use js_mac_session::client::Client;
use js_mac_session::full_disk_access::FullDiskAccess;

use crate::lifecycle::NativeRefusal;

/// Asks the helper whether it holds Full Disk Access.
///
/// A helper from before the question refuses the verb as unknown, which is
/// [`FullDiskAccess::Undetermined`]: the agent then refuses its native jobs, as it does for
/// any state but granted, and says so in its hello.
///
/// # Errors
///
/// The helper could not be asked (its socket is missing, it was refused, or it answered
/// something else). The text names `--native-helper-socket` and the cause; the agent does not
/// start, and its service manager starts it again.
pub fn ask(helper: &Client) -> Result<FullDiskAccess, String> {
    match helper.full_disk_access() {
        Ok(state) => Ok(state),
        Err(why) if why.unknown_to_helper() => Ok(FullDiskAccess::Undetermined),
        Err(why) => Err(format!(
            "--native-helper-socket: asking the helper whether it holds Full Disk Access: {why}"
        )),
    }
}

/// Puts `state` into `capabilities` as the capability `full-disk-access`.
pub fn report(capabilities: &mut BTreeMap<String, String>, state: FullDiskAccess) {
    capabilities.insert(FULL_DISK_ACCESS.to_owned(), state.as_str().to_owned());
}

/// Refuses a native start unless the helper holds Full Disk Access: `no-full-disk-access`,
/// with a text that names the state and what to do.
pub(super) fn admit(state: FullDiskAccess) -> Result<(), NativeRefusal> {
    match state {
        FullDiskAccess::Granted => Ok(()),
        other => Err(NativeRefusal {
            code: refusal::NO_FULL_DISK_ACCESS,
            why: format!(
                "the root helper js-mac-session does not hold Full Disk Access ({}): install the \
                 privacy profile packaging/js-agent-macos/mdm/js-agent-full-disk-access.mobileconfig \
                 through device management and restart the agent",
                other.as_str()
            ),
        }),
    }
}
