//! The profile text and the `sandbox-exec` argument vector.

use std::ffi::OsString;

use super::net::Net;
use super::path::AbsPath;
use super::policy::{Policy, ReadModel};

/// The program that applies a profile.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Lines before every grant: allow everything, then take away what a run gets back only
/// narrowly. `job-creation` and `lsopen` are also in the floor: launchd refuses a job from
/// a sandboxed process regardless, and Launch Services would start an application outside
/// the sandbox.
const HEAD: [&str; 5] = [
    "(version 1)",
    "(allow default)",
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
    /// The allow-list read model has no builder yet; a run that asks for it is refused
    /// rather than run under the open one.
    #[error("the allow read model has no profile builder yet")]
    ReadModelUnavailable,
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
    /// write denies; the write grants (the run directory, the user's folder entry and
    /// `/dev`); the read denies; the write protection of inputs; the network; process
    /// info; the floor.
    ///
    /// A later rule wins, so the floor beats everything before it.
    ///
    /// # Errors
    /// [`BuildError::ReadModelUnavailable`] for [`ReadModel::Allow`].
    pub fn for_policy(policy: &Policy) -> Result<Self, BuildError> {
        if policy.read_model() == ReadModel::Allow {
            return Err(BuildError::ReadModelUnavailable);
        }
        let mut lines: Vec<String> = Vec::new();
        let mut params: Vec<(String, String)> = Vec::new();
        push_all(&mut lines, &HEAD);

        let mut grant = String::from("(allow file-write* (subpath (param \"RUN\"))");
        params.push(("RUN".to_owned(), policy.run().as_str().to_owned()));
        if let Some(entry) = policy.uid_entry() {
            grant.push_str(" (subpath (param \"UENTRY\"))");
            params.push(("UENTRY".to_owned(), entry.as_str().to_owned()));
        }
        grant.push_str(" (subpath \"/dev\"))");
        lines.push(grant);

        for (i, path) in policy.read_denies().iter().enumerate() {
            lines.push(format!("(deny file-read* (subpath (param \"RD_{i}\")))"));
            params.push((format!("RD_{i}"), path.as_str().to_owned()));
        }
        for (i, path) in policy.inputs().iter().enumerate() {
            lines.push(format!("(deny file-write* (subpath (param \"IN_{i}\")))"));
            params.push((format!("IN_{i}"), path.as_str().to_owned()));
        }

        net_rules(policy, &mut lines);
        push_all(&mut lines, &PROCESS_INFO);
        push_all(&mut lines, &FLOOR);

        let mut text = lines.join("\n");
        text.push('\n');
        Ok(Self { text, params })
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
    /// the profile, then `program` and its `args`. `program` is absolute, so
    /// `sandbox-exec` does not search `PATH`.
    #[must_use]
    pub fn argv(&self, program: &AbsPath, args: &[OsString]) -> Vec<OsString> {
        let mut argv = vec![OsString::from(SANDBOX_EXEC)];
        for (name, value) in &self.params {
            argv.push(OsString::from("-D"));
            argv.push(OsString::from(format!("{name}={value}")));
        }
        argv.push(OsString::from("-p"));
        argv.push(OsString::from(self.text.as_str()));
        argv.push(OsString::from(program.as_str()));
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
