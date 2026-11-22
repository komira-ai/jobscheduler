//! The mount providers an agent was given (`--mount-provider NAME=SOCKET`,
//! `--mount-provider-user NAME=USER` and `--mount-provider-wait SECS`,
//! `docs/design/mount-providers.md`), checked when the agent starts, and the step that gives
//! them to the lifecycle once it is built.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use super::{Event, Lifecycle};
use crate::system::Machine;

/// How many seconds a start waits on its providers when `--mount-provider-wait` is not given.
pub const DEFAULT_PROVIDER_WAIT_SECS: u64 = 120;

/// The most seconds `--mount-provider-wait` takes.
pub const MAX_PROVIDER_WAIT_SECS: u64 = 900;

/// The flags of the node-local mount providers, which both builds take: a Linux agent with the
/// crun runtime and a macOS agent with the native runtime.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ProviderFlags {
    /// A node-local mount provider, `NAME=SOCKET`: a process on this host that makes the tree
    /// a job's layer or mount names as `{provider, ref}` (`docs/design/mount-providers.md`),
    /// serving the protocol `jobscheduler.provider.v1.MountProvider` on the Unix socket
    /// `SOCKET`. The name is 1 to 32 bytes of `a-z`, `0-9` and `-`, the first a letter, and
    /// is given once; the directory of the socket must exist (the agent does not wait for the
    /// provider to start). The socket must be as `--mount-provider-user` says: the agent
    /// looks before each connect. The hello reports
    /// `mount-provider.NAME` where it reports `layered-rootfs`. The directory a provider
    /// mounts trees in is listed as a `--mount-root-ro` (a `--mount-root` for a provider whose
    /// trees a job may write). Repeatable. On Linux it needs the crun runtime; on macOS a
    /// job's provider mount is read-only and its directory must be on a local volume.
    #[arg(long = "mount-provider", value_name = "NAME=SOCKET", value_parser = crate::cli::named_path)]
    pub mount_providers: Vec<(String, PathBuf)>,
    /// The user a `--mount-provider` runs as, as `NAME=USER` (a user name or a uid; root when
    /// not given); repeatable, once for each provider. A name is looked up in `/etc/passwd` on
    /// Linux and in the directory service on macOS. Before each connect the agent checks
    /// that the provider's socket belongs to that user or root and that no other user can
    /// write it (its group may: the agent reaches it through the socket's group), and that its
    /// directory belongs to root or that user and that no one else can write it.
    #[arg(long = "mount-provider-user", value_name = "NAME=USER", value_parser = crate::cli::key_value)]
    pub mount_provider_users: Vec<(String, String)>,
    /// How many seconds a start may wait on its mount providers, from its admission (every
    /// call to prepare and mount a source is inside it); one that has waited this long
    /// fails `input-unavailable`. From 1 to 900; by default 120. Needs `--mount-provider`.
    #[arg(long, value_name = "SECS")]
    pub mount_provider_wait: Option<u64>,
}

impl ProviderFlags {
    /// The three flags with whether each was given: an agent that runs no job refuses them,
    /// and so does a Linux agent that runs the container engine.
    #[must_use]
    pub fn given(&self) -> [(&'static str, bool); 3] {
        [
            ("--mount-provider", !self.mount_providers.is_empty()),
            (
                "--mount-provider-user",
                !self.mount_provider_users.is_empty(),
            ),
            ("--mount-provider-wait", self.mount_provider_wait.is_some()),
        ]
    }
}

/// Where a `--mount-provider-user` given by name is looked up.
#[derive(Clone, Copy)]
pub enum UserNames<'a> {
    /// A passwd-format file, read once for every name (Linux: `/etc/passwd`).
    Passwd(&'a Path),
    /// The system's directory of users (macOS: Directory Services, whose service accounts are
    /// not in `/etc/passwd`), asked once for each name: the uid of a name, or `None` when no
    /// user has it.
    Directory(&'a dyn Fn(&str) -> io::Result<Option<u32>>),
}

/// The providers the flags name, each with the socket it serves on and the uid of the user it
/// runs as, and how long a start may wait on them. No provider is the agent's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountProviders {
    sockets: Vec<(String, PathBuf, u32)>,
    wait_secs: u64,
}

impl MountProviders {
    /// Checks the flags: each name is a provider's name ([`js_agent_wire::is_provider_name`])
    /// and given once, the directory of each socket exists (a provider that is not running
    /// yet is not waited for: the socket is looked for when a call is made), each provider's
    /// user (`users`, root when not given) names a provider given once and is a uid or a
    /// user that `names_of` knows (asked only when a user is named by name), and
    /// the wait is 1 to [`MAX_PROVIDER_WAIT_SECS`] seconds and given only with a provider.
    ///
    /// # Errors
    ///
    /// The text names the flag at fault.
    pub fn from_flags(
        given: &[(String, PathBuf)],
        users: &[(String, String)],
        wait_secs: Option<u64>,
        names_of: UserNames<'_>,
    ) -> Result<Self, String> {
        let mut names = BTreeSet::new();
        for (name, socket) in given {
            if !js_agent_wire::is_provider_name(name) {
                return Err(format!(
                    "--mount-provider {name:?}: not a usable provider name"
                ));
            }
            if !names.insert(name.as_str()) {
                return Err(format!("--mount-provider {name}: given twice"));
            }
            // A bare file name is in the directory the agent runs in.
            let dir = socket
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !dir.is_dir() {
                return Err(format!(
                    "--mount-provider {name}: {} is not an existing directory",
                    dir.display()
                ));
            }
        }
        let uids = uids(&names, users, names_of)?;
        let wait_secs = match (wait_secs, given.is_empty()) {
            (Some(_), true) => {
                return Err("--mount-provider-wait: needs --mount-provider".to_owned());
            }
            (Some(secs), false) if !(1..=MAX_PROVIDER_WAIT_SECS).contains(&secs) => {
                return Err(format!(
                    "--mount-provider-wait: {secs} is not from 1 to {MAX_PROVIDER_WAIT_SECS} seconds"
                ));
            }
            (Some(secs), false) => secs,
            (None, _) => DEFAULT_PROVIDER_WAIT_SECS,
        };
        let sockets = given
            .iter()
            .map(|(name, socket)| {
                let uid = uids.get(name.as_str()).copied().unwrap_or(0);
                (name.clone(), socket.clone(), uid)
            })
            .collect();
        Ok(Self { sockets, wait_secs })
    }

    /// The providers' names and sockets, each with the uid of the user the provider runs as,
    /// in the order given.
    #[must_use]
    pub fn sockets(&self) -> &[(String, PathBuf, u32)] {
        &self.sockets
    }

    /// How long a start may wait on its providers, from its admission, in milliseconds.
    #[must_use]
    pub fn wait_ms(&self) -> u64 {
        self.wait_secs.saturating_mul(1000)
    }

    /// Whether the agent was given no provider.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }

    /// Gives `life` the providers, as the `machine` makes their client for the agent that
    /// registered as `host_id`; nothing when the agent was given none. The events are what
    /// attaching found of an earlier process.
    ///
    /// # Errors
    ///
    /// The machine could not make the client; the text says why.
    pub fn attach(
        &self,
        life: &mut Lifecycle,
        machine: &dyn Machine,
        host_id: &str,
    ) -> Result<Vec<Event>, String> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let client = machine
            .providers(host_id, &self.sockets)
            .map_err(|why| format!("--mount-provider: {why}"))?;
        Ok(life.attach_providers(client, self.wait_ms()))
    }
}

/// The uid of each provider `users` names, by name: each names one of `names`, once, and is a
/// number or a user that `names_of` knows.
fn uids<'a>(
    names: &BTreeSet<&str>,
    users: &'a [(String, String)],
    names_of: UserNames<'_>,
) -> Result<BTreeMap<&'a str, u32>, String> {
    let mut uids = BTreeMap::new();
    let mut text = None;
    for (name, user) in users {
        if !names.contains(name.as_str()) {
            return Err(format!(
                "--mount-provider-user {name}: names no --mount-provider"
            ));
        }
        let uid = match (user.parse::<u32>(), names_of) {
            (Ok(uid), _) => uid,
            (Err(_), UserNames::Passwd(passwd)) => {
                let read = match text.take() {
                    Some(read) => read,
                    None => std::fs::read_to_string(passwd).map_err(|why| {
                        format!("--mount-provider-user {name}: {}: {why}", passwd.display())
                    })?,
                };
                let found = passwd_uid(&read, user);
                text = Some(read);
                found.ok_or_else(|| {
                    format!(
                        "--mount-provider-user {name}: no user {user:?} in {}",
                        passwd.display()
                    )
                })?
            }
            (Err(_), UserNames::Directory(lookup)) => lookup(user)
                .map_err(|why| {
                    format!("--mount-provider-user {name}: the directory service: {why}")
                })?
                .ok_or_else(|| {
                    format!(
                        "--mount-provider-user {name}: no user {user:?} in the directory service"
                    )
                })?,
        };
        if uids.insert(name.as_str(), uid).is_some() {
            return Err(format!("--mount-provider-user {name}: given twice"));
        }
    }
    Ok(uids)
}

/// The uid of `user` in passwd-format `text`: the third field of the line whose first field
/// is `user`.
fn passwd_uid(text: &str, user: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let uid = fields.nth(1)?;
        (name == user).then(|| uid.parse().ok()).flatten()
    })
}
