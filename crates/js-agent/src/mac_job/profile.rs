//! The Seatbelt profile a native job runs under, which the job's child applies itself.
//!
//! Two kinds ([`Kind`]):
//!
//! - **Confined writes** (the default): allow by default, then take away every file write but the
//!   job folder's, preference writes, launchd jobs, Launch Services, Mach name registration,
//!   mounts, POSIX shared memory creation, the network unless the job names it, and what reaches
//!   an attached device. The rules are those of the agent's present profile (`js-mac-pool`'s
//!   `seatbelt/profile.rs`, its head and its floor), with the job folder as the one writable
//!   place. Real builds (Swift, Python) pass under this kind; another job's files are kept from
//!   the job by Unix permissions, each job running as its own user.
//! - **Deny default**: deny by default, then allow back what the measured builds needed: start
//!   programs, signal and inspect the job's own processes, read the system (`/usr`, `/bin`,
//!   `/sbin`, `/System`, `/Library`, which holds the Command Line Tools, `/private/etc` and a
//!   few system files), read and write the job folder, and the system services every program
//!   uses. Kept for measurement.
//!
//! The job folder is the parameter `JOB`, given to `sandbox_init_with_parameters` beside the
//! text, so no path is written into the text. A job with trees also gets [`mount_rules`]: no
//! read below the mount provider's mount root but in the job's own trees (the owner's decision:
//! a job started as an ordinary child holds the agent's Full Disk Access, which would let it read
//! every tree on the mount). Rules a run adds for measurement (`--profile-add`) come last.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::ValueEnum;

use super::spec::Network;

/// The name of the profile's one parameter: the job folder, absolute and resolved.
pub const JOB_PARAM: &str = "JOB";

/// Which profile a job runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// Allow by default; writes only in the job folder (see the module documentation).
    ConfinedWrites,
    /// Deny by default.
    DenyDefault,
}

/// The confined-writes kind, before the network rules.
const CONFINED: &str = r#"(version 1)
(allow default)
; files: write only the job folder and the devices
(deny file-write*)
(allow file-write* (subpath (param "JOB"))
    (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty") (literal "/dev/dtracehelper")
    (subpath "/dev/fd"))
; nothing that outlives the job or changes the user's settings
(deny user-preference-write)
(deny job-creation)
(deny lsopen)
(deny mach-register)
(deny file-mount)
(deny ipc-posix-shm-write-create)
; processes: see only the job's own
(deny process-info* (target others))
(allow process-info* (target self) (target same-sandbox))
; the OS-owned protected stores, and what reaches an attached device
(deny file-read* file-write* (subpath "/private/var/protected/sfanalytics") (subpath "/private/var/protected/trustd/private"))
(deny mach-lookup (global-name-prefix "com.apple.CoreDevice."))
(deny mach-lookup (global-name-prefix "com.apple.remoted"))
; network: none but the system log, unless the job names more below
(deny network*)
(allow network-outbound (literal "/private/var/run/syslog"))
(deny network-outbound (literal "/private/var/run/usbmuxd"))
"#;

/// The deny-default kind, before the network rules.
const DENY_DEFAULT: &str = r#"(version 1)
(deny default)
; processes: start programs; see and signal only this job's own sandbox
(allow process-fork)
(allow process-exec*)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
; files: metadata anywhere (path walks), read the system and the job folder, write only the job folder
(allow file-read-metadata)
(allow file-read* file-map-executable
    (literal "/")
    (subpath "/usr") (subpath "/bin") (subpath "/sbin") (subpath "/System") (subpath "/Library")
    (subpath "/private/etc") (subpath "/private/var/db/timezone") (subpath "/private/var/db/dyld")
    (literal "/private/var/db/xcode_select_link") (subpath "/private/var/select")
    (subpath "/dev")
    (subpath (param "JOB")))
(allow file-write* (subpath (param "JOB"))
    (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty") (subpath "/dev/fd"))
(allow file-ioctl (literal "/dev/null") (literal "/dev/tty") (subpath "/dev/fd"))
; system services every program uses
(allow mach-lookup
    (global-name "com.apple.system.logger")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.opendirectoryd.membership")
    (global-name "com.apple.system.DirectoryService.libinfo_v1")
    (global-name "com.apple.logd")
    (global-name "com.apple.diagnosticd")
    (global-name "com.apple.cfprefsd.daemon")
    (global-name "com.apple.cfprefsd.agent")
    (global-name "com.apple.trustd")
    (global-name "com.apple.trustd.agent")
    (global-name "com.apple.analyticsd"))
(allow user-preference-read)
(allow ipc-posix-shm-read* (ipc-posix-name "apple.shm.notification_center"))
(allow network-outbound (literal "/private/var/run/syslog"))
(allow system-socket)
"#;

/// The rules of the loopback network: a test server of the build, and its clients.
const LOOPBACK: &str = r#"; network: the loopback address
(allow network-bind network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"#;

/// The profile text of `kind` for a job with `network`, then `mount` (the text of
/// [`mount_rules`], possibly empty), then `added` (rules a run adds, possibly empty).
#[must_use]
pub fn render(kind: Kind, network: &Network, mount: &str, added: &str) -> String {
    let mut text = String::from(match kind {
        Kind::ConfinedWrites => CONFINED,
        Kind::DenyDefault => DENY_DEFAULT,
    });
    if network.loopback {
        text.push_str(LOOPBACK);
    }
    if !network.outbound_ports.is_empty() {
        // A name to connect to is resolved by mDNSResponder, by Mach or by its socket.
        text.push_str("; network: outbound TCP to the named ports, and name lookups\n");
        text.push_str("(allow mach-lookup (global-name \"com.apple.dnssd.service\"))\n");
        text.push_str("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n");
    }
    for port in &network.outbound_ports {
        // Seatbelt matches a remote address only as `*` or `localhost`: the rule names the port.
        text.push_str(&format!(
            "(allow network-outbound (remote tcp \"*:{port}\"))\n"
        ));
    }
    text.push_str(mount);
    if !added.is_empty() {
        text.push_str("; added for this run\n");
        text.push_str(added);
        if !added.ends_with('\n') {
            text.push('\n');
        }
    }
    text
}

/// The rules that keep a job on the mount to its own trees, and the parameters they name: no
/// read below `mount` (parameter `MOUNT`), but reads anywhere in each of `trees` (`TREE0`, ...),
/// and the metadata (not the listing) of each folder between `mount` and a tree (`UP0`, ...),
/// which a path walk to the tree needs. Every path is absolute and resolved; a tree not strictly
/// beneath `mount` is refused, with its path.
pub fn mount_rules(
    mount: &Path,
    trees: &[PathBuf],
) -> Result<(String, Vec<(String, String)>), PathBuf> {
    let text_of = |p: &Path| p.to_string_lossy().into_owned();
    let mut params = vec![("MOUNT".to_owned(), text_of(mount))];
    let mut ups: BTreeSet<PathBuf> = BTreeSet::new();
    let mut text = String::from(
        "; the mount: the job's own trees only\n(deny file-read* (subpath (param \"MOUNT\")))\n",
    );
    for (i, tree) in trees.iter().enumerate() {
        let Ok(below) = tree.strip_prefix(mount) else {
            return Err(tree.clone());
        };
        if below.as_os_str().is_empty() {
            return Err(tree.clone());
        }
        let mut up = tree.parent();
        while let Some(dir) = up.filter(|dir| *dir != mount) {
            ups.insert(dir.to_owned());
            up = dir.parent();
        }
        let name = format!("TREE{i}");
        text.push_str(&format!(
            "(allow file-read* (subpath (param \"{name}\")))\n"
        ));
        params.push((name, text_of(tree)));
    }
    text.push_str("(allow file-read-metadata (literal (param \"MOUNT\")))\n");
    for (i, dir) in ups.iter().enumerate() {
        let name = format!("UP{i}");
        text.push_str(&format!(
            "(allow file-read-metadata (literal (param \"{name}\")))\n"
        ));
        params.push((name, text_of(dir)));
    }
    Ok((text, params))
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
