//! The flags that configure containers, checked when the agent starts.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::subid::{JobUsers, Mapping, RANGE_LEN};
use crate::lifecycle::SLOTS;

/// The base user and group of the slots when `--container-user` is not given. The user of
/// slot `k` is this plus `k`, for the uid and the gid: `1000:1000` for slot 0, `1001:1001`
/// for slot 1. These are ids of the job's user namespace, which the agent maps onto the
/// subordinate range (`--container-subuid`), not users of the host.
pub const DEFAULT_USER: &str = "1000:1000";

/// The default base as numbers; [`DEFAULT_USER`] is its text.
const DEFAULT_IDS: (u32, u32) = (1000, 1000);

/// The network containers get when `--container-network` is not given: none at all.
pub const DEFAULT_NETWORK: &str = "none";

/// The size of a container's `/tmp` when `--container-tmp-size` is not given.
pub const DEFAULT_TMP_SIZE: &str = "64m";

/// Where the cgroup version 2 file system is mounted. `--actions-cgroup` lies below it.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The longest `--actions-cgroup` accepted.
const CGROUP_MAX: usize = 256;

/// The longest network name accepted.
const NETWORK_NAME_MAX: usize = 64;

/// The longest engine host accepted.
const HOST_MAX: usize = 256;

/// The only scheme of an engine host (`--container-host`): a socket on this machine. The
/// post-job checks read this machine's `/proc`, mounts and users, so a job that runs on
/// another machine (`tcp://`, `ssh://`) would pass every one of them.
const HOST_SCHEME: &str = "unix:///";

/// Why a container flag is wrong. The text names the flag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// `--container-bin` is relative or not UTF-8: it is started by exactly this path,
    /// never looked up on the `PATH`.
    #[error("--container-bin: not an absolute UTF-8 path")]
    BinPath,
    /// `--container-bin` is not an executable regular file.
    #[error("--container-bin: not an executable file")]
    BinMissing,
    /// `--container-network` names no network the agent will use: `host` (it would put
    /// the job on the machine's own network), or text that is not a network name (which
    /// would let `container:<id>` or an option pass as one).
    #[error("--container-network: not a network name the agent will use")]
    Network,
    /// `--container-user` is not `<uid>:<gid>` with both numbers above zero.
    #[error("--container-user: not <uid>:<gid> of a user that is not root")]
    User,
    /// The agent does not run as root and `--container-user` (or its default) is not the
    /// agent's own user: the run's scratch directory is private to the agent (`0700`), so
    /// a container of another user could not write it, and the agent cannot hand it over.
    #[error(
        "--container-user: must be {agent} (the agent's own uid:gid) unless the agent runs as root"
    )]
    UserNotAgent {
        /// The agent's own `<uid>:<gid>`.
        agent: String,
    },
    /// `--container-host` is not `unix:///<path>`: an engine on another machine runs jobs
    /// where the agent's own `/proc`, mounts and users show nothing of them.
    #[error(
        "--container-host: only a unix:///<path> socket on this host is accepted: the post-job checks read this host, and a job on another one would pass them all"
    )]
    Host,
    /// `--container-pull` is neither `never` nor `missing`.
    #[error("--container-pull: not never or missing")]
    Pull,
    /// `--container-tmp-size` is not a number followed by `k`, `m` or `g`.
    #[error("--container-tmp-size: not a size such as 64m")]
    TmpSize,
    /// `--actions-cgroup` is not a path of plain components strictly below `/sys/fs/cgroup`.
    #[error(
        "--actions-cgroup: not an absolute path of letters, digits, '.', '_', '-' and '@' components below /sys/fs/cgroup"
    )]
    ActionsCgroup,
    /// `--container-subuid` is missing: every contained job runs in a user namespace mapped
    /// onto the subordinate range of the user it names.
    #[error(
        "--container-subuid: required with --container-bin: every job runs in a user namespace mapped onto that user's subordinate ids"
    )]
    NoSubuid,
    /// `--container-subuid` is not a user name the id files can hold.
    #[error("--container-subuid: not a user name")]
    Subuid,
    /// `--container-user` is a base from which the last slot's ids would leave a job's user
    /// namespace: a namespace holds the ids 0 to 65535, and the slots take the ids from
    /// the base up, one for each slot.
    #[error(
        "--container-user: the uid and the gid are the first slot's ids, and each of the {slots} slots takes the next ids, so both must be at most {last} to stay below 65536, the ids of a job's user namespace",
        slots = SLOTS,
        last = RANGE_LEN - SLOTS
    )]
    UserOutsideMap,
    /// A container flag without `--container-bin`: they configure containers, which the
    /// agent runs only with a runtime.
    #[error("the container flags need --container-bin")]
    NoBin,
}

/// When a container's image is pulled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// Never: the image must be on the host already, so a job cannot make the host reach
    /// out to a registry of its choosing.
    Never,
    /// When the host does not have the image yet.
    Missing,
}

impl Pull {
    /// The value of the runtime's `--pull`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Missing => "missing",
        }
    }

    fn parse(text: &str) -> Result<Self, SettingsError> {
        match text {
            "never" => Ok(Self::Never),
            "missing" => Ok(Self::Missing),
            _ => Err(SettingsError::Pull),
        }
    }
}

/// The container flags as the command line gave them, before they are checked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags<'a> {
    /// `--container-bin`.
    pub bin: Option<&'a Path>,
    /// `--container-network`.
    pub network: Option<&'a str>,
    /// `--container-user`.
    pub user: Option<&'a str>,
    /// `--container-host`.
    pub host: Option<&'a str>,
    /// `--container-pull`.
    pub pull: Option<&'a str>,
    /// `--container-tmp-size`.
    pub tmp_size: Option<&'a str>,
    /// `--actions-cgroup`.
    pub actions_cgroup: Option<&'a Path>,
    /// `--container-subuid`.
    pub subuid: Option<&'a str>,
}

/// The checked container flags: where the runtime is, which network jobs get, which user
/// they run as, which engine host the agent talks to, when images are pulled and how big
/// `/tmp` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    bin: String,
    network: String,
    user: String,
    ids: (u32, u32),
    host: Option<String>,
    pull: Pull,
    tmp_size: String,
    actions: Option<ActionsCgroup>,
    subuid: Option<String>,
    /// The mapping the runtime resolved when it opened; shared by the clones.
    resolved: Arc<OnceLock<Mapping>>,
    chown: Option<(u32, u32)>,
}

/// The checked `--actions-cgroup`: the directory, and the `--cgroup-parent` that puts a
/// container in it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionsCgroup {
    dir: String,
    parent: String,
}

impl Settings {
    /// Checks the flags. No `--container-bin` means containers are not enabled, and then
    /// no other container flag may be given. `agent` is the agent's own `(uid, gid)`; see
    /// [`Settings::with_agent`].
    ///
    /// # Errors
    ///
    /// The flag at fault; see [`SettingsError`].
    pub fn from_flags(flags: &Flags<'_>, agent: (u32, u32)) -> Result<Option<Self>, SettingsError> {
        let Some(bin) = flags.bin else {
            let others = [
                flags.network,
                flags.user,
                flags.host,
                flags.pull,
                flags.tmp_size,
                flags.subuid,
            ];
            let alone = others.iter().any(Option::is_some) || flags.actions_cgroup.is_some();
            return match alone {
                true => Err(SettingsError::NoBin),
                false => Ok(None),
            };
        };
        Self::new(bin, flags.network, flags.user)?
            .with_host(flags.host)?
            .with_pull(flags.pull)?
            .with_tmp_size(flags.tmp_size)?
            .with_actions_cgroup(flags.actions_cgroup)?
            .with_subuid(flags.subuid.ok_or(SettingsError::NoSubuid)?)?
            .with_agent(agent.0, agent.1)
            .map(Some)
    }

    /// Checks `bin` (an absolute path to an executable file) and the optional `network`
    /// and `user`. The other settings take their defaults: no engine host (the engine's
    /// own default), no pull, a `/tmp` of [`DEFAULT_TMP_SIZE`]; and the agent's own user
    /// is not looked at (see [`Settings::with_agent`]).
    ///
    /// # Errors
    ///
    /// The flag at fault; see [`SettingsError`].
    pub fn new(
        bin: &Path,
        network: Option<&str>,
        user: Option<&str>,
    ) -> Result<Self, SettingsError> {
        let bin = bin
            .to_str()
            .filter(|text| text.starts_with('/'))
            .ok_or(SettingsError::BinPath)?;
        fs::metadata(bin)
            .ok()
            .filter(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .ok_or(SettingsError::BinMissing)?;
        let network = network.map_or(Ok(DEFAULT_NETWORK), network_name)?;
        let ids = user.map_or(Ok(DEFAULT_IDS), non_root)?;
        Ok(Self {
            bin: bin.to_owned(),
            network: network.to_owned(),
            user: format!("{}:{}", ids.0, ids.1),
            ids,
            host: None,
            pull: Pull::Never,
            tmp_size: DEFAULT_TMP_SIZE.to_owned(),
            actions: None,
            subuid: None,
            resolved: Arc::new(OnceLock::new()),
            chown: None,
        })
    }

    /// The engine host the runtime is told to use, when one is given.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Host`].
    pub fn with_host(self, host: Option<&str>) -> Result<Self, SettingsError> {
        let host = host.map(engine_host).transpose()?;
        Ok(Self { host, ..self })
    }

    /// The pull policy, when one is given.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Pull`].
    pub fn with_pull(self, pull: Option<&str>) -> Result<Self, SettingsError> {
        let pull = pull.map_or(Ok(self.pull), Pull::parse)?;
        Ok(Self { pull, ..self })
    }

    /// The size of `/tmp`, when one is given.
    ///
    /// # Errors
    ///
    /// [`SettingsError::TmpSize`].
    pub fn with_tmp_size(self, size: Option<&str>) -> Result<Self, SettingsError> {
        let tmp_size = size.map_or(Ok(self.tmp_size.clone()), tmp_size)?;
        Ok(Self { tmp_size, ..self })
    }

    /// The control group that holds the containers, when one is given. It is checked as
    /// text only: that the directory exists and is readable shows when the agent samples it.
    ///
    /// # Errors
    ///
    /// [`SettingsError::ActionsCgroup`].
    pub fn with_actions_cgroup(self, dir: Option<&Path>) -> Result<Self, SettingsError> {
        let actions = dir.map(actions_cgroup).transpose()?;
        Ok(Self { actions, ..self })
    }

    /// Runs every job in a user namespace mapped onto the subordinate ids of `user`
    /// (`--container-subuid`). The range itself is read when the runtime opens
    /// ([`CliRuntime::open`](super::CliRuntime::open)). Settings built without this call
    /// run containers in the host's user namespace: only a test does that.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Subuid`] when `user` cannot be the first field of a line of
    /// `/etc/subuid`; [`SettingsError::UserOutsideMap`] when the container user has an id
    /// of 65536 or more.
    pub fn with_subuid(self, user: &str) -> Result<Self, SettingsError> {
        let plain = |c: char| c.is_ascii_graphic() && c != ':';
        let named = !user.is_empty() && user.chars().all(plain);
        let room = RANGE_LEN - SLOTS;
        let inside = self.ids.0 <= room && self.ids.1 <= room;
        match (named, inside) {
            (false, _) => Err(SettingsError::Subuid),
            (true, false) => Err(SettingsError::UserOutsideMap),
            (true, true) => Ok(Self {
                subuid: Some(user.to_owned()),
                ..self
            }),
        }
    }

    /// Settles who owns the run's scratch directory. It is private to the agent (`0700`),
    /// and the container's user must be able to write it. An agent that runs as root
    /// (`uid` 0) hands the directory to the container's user when it starts a job
    /// ([`Settings::chown_scratch`]); any other agent cannot give a file away, so the
    /// container's user must be the agent's own. (Jobs mapped onto a subordinate range, see
    /// [`Settings::with_subuid`], are host users the agent never is: the runtime refuses
    /// to open for an agent that is not root, see
    /// [`CliRuntime::open_in`](super::CliRuntime::open_in).)
    ///
    /// # Errors
    ///
    /// [`SettingsError::UserNotAgent`] when the agent is not root and its `uid:gid` is not
    /// the container user.
    pub fn with_agent(self, uid: u32, gid: u32) -> Result<Self, SettingsError> {
        match (uid, (uid, gid) == self.ids) {
            (0, _) => Ok(Self {
                chown: Some(self.ids),
                ..self
            }),
            (_, true) => Ok(Self {
                chown: None,
                ..self
            }),
            (_, false) => Err(SettingsError::UserNotAgent {
                agent: format!("{uid}:{gid}"),
            }),
        }
    }

    /// The runtime's absolute path.
    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// The network the containers join.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// The `<uid>:<gid>` of the first slot, the base of `--container-user`; see
    /// [`Settings::slot_user`] for the others.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The container ids `(uid, gid)` of slot `slot`: the base ids plus the slot number.
    /// The ids of two slots never meet, and below 65536 for every slot when the jobs are
    /// mapped ([`Settings::with_subuid`] checks the base against [`SLOTS`]).
    pub fn slot_ids(&self, slot: u32) -> (u32, u32) {
        (
            self.ids.0.saturating_add(slot),
            self.ids.1.saturating_add(slot),
        )
    }

    /// The `<uid>:<gid>` containers of slot `slot` run as.
    pub fn slot_user(&self, slot: u32) -> String {
        let (uid, gid) = self.slot_ids(slot);
        format!("{uid}:{gid}")
    }

    /// Who the processes of the containers are on the host: the range the runtime resolved
    /// from `--container-subuid` when it opened (not read again), or the one uid of [`Settings::user`] for settings built
    /// without it. The post-job checks look for leftover processes of these users.
    pub fn uid(&self) -> JobUsers {
        self.subuid.as_ref().map_or(JobUsers::Uid(self.ids.0), |_| {
            JobUsers::Mapped(self.resolved.clone())
        })
    }

    /// Records the mapping the runtime resolved, for [`Settings::uid`] (the settings and
    /// their clones share it; the first one recorded stays).
    pub fn resolve(&self, mapping: Mapping) {
        let _ = self.resolved.set(mapping);
    }

    /// The user whose subordinate ids the jobs are mapped onto (`--container-subuid`), if
    /// the settings map them.
    pub fn subuid(&self) -> Option<&str> {
        self.subuid.as_deref()
    }

    /// The engine host (`unix:///<path>`: a socket on this machine) the runtime is told to use,
    /// when one is given; otherwise the engine uses its own default.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// When images are pulled.
    pub fn pull(&self) -> Pull {
        self.pull
    }

    /// The size limit of the containers' `/tmp`, as the tmpfs option takes it.
    pub fn tmp_size(&self) -> &str {
        &self.tmp_size
    }

    /// The control group directory that holds the containers (`--actions-cgroup`), when one
    /// is given.
    pub fn actions_cgroup(&self) -> Option<&str> {
        self.actions.as_ref().map(|actions| actions.dir.as_str())
    }

    /// The value of `--cgroup-parent` that puts a container in the actions control group:
    /// the slice name when the path is one slice directly below `/sys/fs/cgroup` (the
    /// systemd cgroup driver), the path below it with a leading `/` otherwise (the cgroupfs
    /// driver). None without `--actions-cgroup`.
    pub fn cgroup_parent(&self) -> Option<&str> {
        self.actions.as_ref().map(|actions| actions.parent.as_str())
    }

    /// Whether, and with which base ids, the scratch directory of a run is given before its
    /// container is created: set when the agent runs as root, none when the container's
    /// user is the agent's own. The ids it names are the first slot's; a run is given its
    /// own slot's ([`Settings::slot_ids`]) as ids of the container, and a mapped job's host
    /// ids are the start of the range plus those.
    pub fn chown_scratch(&self) -> Option<(u32, u32)> {
        self.chown
    }

    /// What the operator should know about this configuration, one sentence each, for the
    /// agent to print when it starts.
    pub fn warnings(&self) -> Vec<String> {
        let reach = self.network != DEFAULT_NETWORK;
        reach
            .then(|| {
                format!(
                    "--container-network {}: a job can reach whatever that network reaches, the host's metadata endpoint and the internal network included, unless the network or the host's firewall stops it",
                    self.network
                )
            })
            .into_iter()
            .collect()
    }
}

/// `name` when it is a plain network name other than `host`.
fn network_name(name: &str) -> Result<&str, SettingsError> {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let shaped = (1..=NETWORK_NAME_MAX).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(word);
    Some(name)
        .filter(|name| shaped && !name.eq_ignore_ascii_case("host"))
        .ok_or(SettingsError::Network)
}

/// `<uid>:<gid>` of numbers above zero.
fn non_root(text: &str) -> Result<(u32, u32), SettingsError> {
    let number = |part: &str| part.parse::<u32>().ok().filter(|n| *n > 0);
    text.split_once(':')
        .and_then(|(uid, gid)| number(uid).zip(number(gid)))
        .ok_or(SettingsError::User)
}

/// `text` when it is a socket on this machine as the engine's client takes it: printable
/// ASCII without a space, `unix:///` and something after it.
fn engine_host(text: &str) -> Result<String, SettingsError> {
    let path = text
        .strip_prefix(HOST_SCHEME)
        .filter(|rest| !rest.is_empty());
    let shaped =
        text.len() <= HOST_MAX && text.bytes().all(|b| b.is_ascii_graphic()) && path.is_some();
    shaped.then(|| text.to_owned()).ok_or(SettingsError::Host)
}

/// `text` when it is a size the tmpfs option takes: one to nine digits, the first not
/// zero, and `k`, `m` or `g`.
fn tmp_size(text: &str) -> Result<String, SettingsError> {
    let digits = text.strip_suffix(['k', 'm', 'g']).unwrap_or("");
    let shaped = (1..=9).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0');
    shaped
        .then(|| text.to_owned())
        .ok_or(SettingsError::TmpSize)
}

/// `path` when it is a control group directory the agent will use: UTF-8, strictly below
/// [`CGROUP_ROOT`], with components of letters, digits and `. _ - @` only (no `..`, no
/// empty component, none that starts with `-`: nothing a runtime would read as another
/// flag or a list).
fn actions_cgroup(path: &Path) -> Result<ActionsCgroup, SettingsError> {
    let text = path.to_str().filter(|text| text.len() <= CGROUP_MAX);
    let below = text
        .and_then(|text| text.strip_prefix(CGROUP_ROOT))
        .and_then(|rest| rest.strip_prefix('/'));
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@');
    let plain = |part: &str| {
        !matches!(part, "" | "." | "..") && !part.starts_with('-') && part.chars().all(word)
    };
    match (text, below) {
        (Some(dir), Some(below)) if below.split('/').all(plain) => {
            let slice = !below.contains('/') && below.ends_with(".slice");
            let parent = match slice {
                true => below.to_owned(),
                false => format!("/{below}"),
            };
            Ok(ActionsCgroup {
                dir: dir.to_owned(),
                parent,
            })
        }
        _ => Err(SettingsError::ActionsCgroup),
    }
}
