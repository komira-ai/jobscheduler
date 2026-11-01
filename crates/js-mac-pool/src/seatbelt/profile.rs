//! The profile text and the `sandbox-exec` argument vector.

use std::ffi::OsString;

use super::allow;
use super::net::Net;
use super::policy::{Policy, ReadModel};

/// The program that applies a profile.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Lines before every grant: allow everything, then take away what a run gets back only
/// narrowly. `job-creation` and `lsopen` are also in the floor: launchd refuses a job from
/// a sandboxed process regardless, and Launch Services would start an application outside
/// the sandbox.
///
/// IPC is not denied: on a hosted runner `(deny ipc-sysv-shm)` did not stop `shmget`, so
/// no rule is claimed. A System V or POSIX object of a user outlives its processes, and the
/// helper reuses uids; the privileged tests measure what a reused uid finds.
const HEAD: [&str; 6] = [
    "(version 1)",
    "(allow default)",
    "(deny file-write*)",
    "(deny job-creation)",
    "(deny lsopen)",
    "(deny user-preference-write)",
];

/// Process visibility. Measured on a hosted runner: with this rule, with narrower ones
/// and with none, `pgrep` listed the same processes. These lines do not hide other
/// users' processes; the user boundary does that work, and a test of it needs two users.
const PROCESS_INFO: [&str; 2] = [
    "(deny process-info* (target others))",
    "(allow process-info* (target self) (target same-sandbox))",
];

/// Only the system log by path, never the unix-socket class: a class-wide allow reaches
/// the agent's own sockets.
const OFF_RULES: [&str; 2] = [
    "(deny network*)",
    "(allow network-outbound (literal \"/private/var/run/syslog\"))",
];

/// The floor, emitted last and not removable: nothing in a policy reaches this constant.
/// Denied services: registering a mach name, mounting, creating a launchd job, opening an
/// application through Launch Services, the OS-owned protected stores, and what reaches
/// an attached device (CoreDevice, `remoted`, the `usbmuxd` socket).
const FLOOR: [&str; 8] = [
    "(deny mach-register)",
    "(deny file-mount)",
    "(deny job-creation)",
    "(deny lsopen)",
    "(deny file-read* file-write* (subpath \"/private/var/protected/sfanalytics\") (subpath \"/private/var/protected/trustd/private\"))",
    "(deny mach-lookup (global-name-prefix \"com.apple.CoreDevice.\"))",
    "(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))",
    "(deny network-outbound (literal \"/private/var/run/usbmuxd\"))",
];

/// Why a policy has no profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The policy asks for the allow read model and has no grants
    /// ([`Policy::allowing`]), so no macOS version's base set: it is refused rather than
    /// run under the open model.
    #[error("the allow read model needs its grants (Policy::allowing)")]
    AllowWithoutGrants,
}

/// A profile and the `-D` parameters it refers to. Every path of the policy is a
/// parameter, named by the builder and referenced as `(param "NAME")`; no path text is
/// part of the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    text: String,
    params: Vec<(String, String)>,
}

impl Profile {
    /// The profile of `policy`, in a fixed order: version and `(allow default)`; the
    /// write denies; the write grants (the run directory, the user's folder entry,
    /// `/dev/null` and `/dev/fd`); the read denies, each with a write deny of the same
    /// path (a read-denied path inside a granted directory is not writable either); the
    /// write protection of inputs; the network; a deny of unix-socket connect and bind
    /// under each read-denied path (after the network rules, so no network allow
    /// reaches the agent's sockets); process info; the floor.
    ///
    /// A later rule wins, so the floor beats everything before it.
    ///
    /// Under [`ReadModel::Allow`] the reads are an allow-list: after the write grants come
    /// the default read deny, the names of the ancestors of every granted place, the base
    /// set of the macOS version, the run's places, the pinned trees, the Xcode and the
    /// kinds, then a deny of each setuid file; the read denies and everything after
    /// them are as in the open model.
    ///
    /// # Errors
    /// [`BuildError::AllowWithoutGrants`] for an allow-model policy that was not given
    /// its grants.
    pub fn for_policy(policy: &Policy) -> Result<Self, BuildError> {
        if policy.read_model() == ReadModel::Allow && policy.allow().is_none() {
            return Err(BuildError::AllowWithoutGrants);
        }
        Ok(Self::render(policy))
    }

    /// The profile of `policy`, which the caller has checked: a policy of the allow model
    /// has its grants.
    pub(super) fn render(policy: &Policy) -> Self {
        let mut lines: Vec<String> = Vec::new();
        let mut params: Vec<(String, String)> = Vec::new();
        push_all(&mut lines, &HEAD);

        let mut grant = String::from("(allow file-write* (subpath (param \"RUN\"))");
        params.push(("RUN".to_owned(), policy.run().as_str().to_owned()));
        if let Some(entry) = policy.uid_entry() {
            grant.push_str(" (subpath (param \"UENTRY\"))");
            params.push(("UENTRY".to_owned(), entry.as_str().to_owned()));
        }
        grant.push_str(" (literal \"/dev/null\") (subpath \"/dev/fd\"))");
        lines.push(grant);
        if let Some(allow) = policy.allow() {
            allow::reads(allow, policy.uid_entry(), &mut lines, &mut params);
        }

        for (i, path) in policy.read_denies().iter().enumerate() {
            lines.push(format!("(deny file-read* (subpath (param \"RD_{i}\")))"));
            lines.push(format!("(deny file-write* (subpath (param \"RD_{i}\")))"));
            params.push((format!("RD_{i}"), path.as_str().to_owned()));
        }
        for (i, path) in policy.inputs().iter().enumerate() {
            lines.push(format!("(deny file-write* (subpath (param \"IN_{i}\")))"));
            params.push((format!("IN_{i}"), path.as_str().to_owned()));
        }

        net_rules(policy, &mut lines);
        for i in 0..policy.read_denies().len() {
            lines.push(format!(
                "(deny network-outbound (remote unix-socket (subpath (param \"RD_{i}\"))))"
            ));
            lines.push(format!(
                "(deny network-bind (local unix-socket (subpath (param \"RD_{i}\"))))"
            ));
        }
        push_all(&mut lines, &PROCESS_INFO);
        push_all(&mut lines, &FLOOR);

        let mut text = lines.join("\n");
        text.push('\n');
        Self { text, params }
    }

    /// The profile text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The `-D` parameters, name and value, in the order they are passed.
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }

    /// The full command: `sandbox-exec`, a `-D NAME=value` pair per parameter, `-p` and
    /// the profile, then `program` and its `args`. `program` is absolute (an [`AbsPath`](super::AbsPath)
    /// or a [`ToolPath`](super::ToolPath)'s path), so `sandbox-exec` does not search `PATH`.
    #[must_use]
    pub fn argv(&self, program: impl AsRef<str>, args: &[OsString]) -> Vec<OsString> {
        let mut argv = vec![OsString::from(SANDBOX_EXEC)];
        for (name, value) in &self.params {
            argv.push(OsString::from("-D"));
            argv.push(OsString::from(format!("{name}={value}")));
        }
        argv.push(OsString::from("-p"));
        argv.push(OsString::from(self.text.as_str()));
        argv.push(OsString::from(program.as_ref()));
        argv.extend(args.iter().cloned());
        argv
    }
}

fn push_all(lines: &mut Vec<String>, rules: &[&str]) {
    for rule in rules {
        lines.push((*rule).to_owned());
    }
}

fn net_rules(policy: &Policy, lines: &mut Vec<String>) {
    match policy.net() {
        Net::On => {}
        Net::Off => push_all(lines, &OFF_RULES),
        Net::Ports(ports) => {
            push_all(lines, &OFF_RULES);
            for port in ports.iter() {
                lines.push(format!("(allow network* (remote ip \"localhost:{port}\"))"));
            }
        }
        Net::Loopback => {
            push_all(lines, &OFF_RULES);
            lines.push("(allow network-inbound (local ip \"localhost:*\"))".to_owned());
            lines.push("(allow network* (remote ip \"localhost:*\"))".to_owned());
        }
    }
}
