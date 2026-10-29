//! The flags that configure containers, checked when the agent starts.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::egress::{ALLOW_MAX as EGRESS_MAX, Allow, DEFAULT_NFT, Egress};

/// The user and group containers run as when `--container-user` is not given: `nobody`
/// and `nogroup` on the common Linux distributions.
pub const DEFAULT_USER: &str = "65534:65534";

/// The network containers get when `--container-network` is not given: none at all.
pub const DEFAULT_NETWORK: &str = "none";

/// The size of a container's `/tmp` when `--container-tmp-size` is not given.
pub const DEFAULT_TMP_SIZE: &str = "64m";

/// The longest network name accepted.
const NETWORK_NAME_MAX: usize = 64;

/// The most `--container-add-host` entries accepted.
pub const ADD_HOST_MAX: usize = 16;

/// The longest host name accepted in `--container-add-host` (a DNS name).
const DNS_NAME_MAX: usize = 253;

/// The longest label of a DNS name.
const DNS_LABEL_MAX: usize = 63;

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
    /// A `--container-add-host` is not `<name>:<ip>` with a DNS name and an IPv4 or IPv6
    /// address (`host-gateway` and any other word is refused), or names a host twice.
    #[error(
        "--container-add-host: not <dns-name>:<ip> (an IPv4 or IPv6 address), or a name given twice"
    )]
    AddHost,
    /// More than [`ADD_HOST_MAX`] `--container-add-host` entries.
    #[error("--container-add-host: at most 16 entries")]
    AddHostCount,
    /// A `--container-egress-allow` is not `IPv4[/24..32]:port` (no host bits, a port above
    /// zero, nothing in 169.254.0.0/16).
    #[error("--container-egress-allow: not <ipv4>[/24-32]:<port> outside 169.254.0.0/16")]
    EgressAllow,
    /// More than the most `--container-egress-allow` entries.
    #[error("--container-egress-allow: at most 16 entries")]
    EgressCount,
    /// A named `--container-network` without any `--container-egress-allow`: a named network
    /// is never run unchecked.
    #[error("--container-network: a named network needs at least one --container-egress-allow")]
    EgressMissing,
    /// `--container-egress-allow` or `--container-nft` without a named `--container-network`.
    #[error("--container-egress-allow and --container-nft: need a named --container-network")]
    EgressNoNetwork,
    /// `--container-nft` is not an absolute path to an executable file.
    #[error("--container-nft: not an absolute UTF-8 path to an executable file")]
    Nft,
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
    /// `--container-add-host`, each `<name>:<ip>`.
    pub add_hosts: &'a [String],
    /// `--container-egress-allow`, each `IP:PORT` or `CIDR:PORT`.
    pub egress_allow: &'a [String],
    /// `--container-nft`.
    pub nft: Option<&'a str>,
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
    add_hosts: Vec<String>,
    egress: Option<Egress>,
    chown: Option<(u32, u32)>,
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
            ];
            let given = others.iter().any(Option::is_some)
                || !flags.add_hosts.is_empty()
                || !flags.egress_allow.is_empty()
                || flags.nft.is_some();
            return match given {
                true => Err(SettingsError::NoBin),
                false => Ok(None),
            };
        };
        Self::new(bin, flags.network, flags.user)?
            .with_host(flags.host)?
            .with_pull(flags.pull)?
            .with_tmp_size(flags.tmp_size)?
            .with_add_hosts(flags.add_hosts)?
            .with_egress(flags.egress_allow, flags.nft)?
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
        let ids = user.map_or(Ok((65534, 65534)), non_root)?;
        Ok(Self {
            bin: bin.to_owned(),
            network: network.to_owned(),
            user: format!("{}:{}", ids.0, ids.1),
            ids,
            host: None,
            pull: Pull::Never,
            tmp_size: DEFAULT_TMP_SIZE.to_owned(),
            add_hosts: Vec::new(),
            egress: None,
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

    /// The static name-to-address entries every container gets (`--add-host`), each
    /// `<name>:<ip>`. A network without DNS resolves nothing else.
    ///
    /// # Errors
    ///
    /// [`SettingsError::AddHostCount`] for more than [`ADD_HOST_MAX`];
    /// [`SettingsError::AddHost`] for an entry that is not a DNS name and an IP address,
    /// or a name given twice.
    pub fn with_add_hosts(self, entries: &[String]) -> Result<Self, SettingsError> {
        if entries.len() > ADD_HOST_MAX {
            return Err(SettingsError::AddHostCount);
        }
        let add_hosts = entries
            .iter()
            .map(|entry| add_host(entry))
            .collect::<Result<Vec<_>, _>>()?;
        let mut names: Vec<String> = add_hosts
            .iter()
            .filter_map(|entry| entry.split_once(':').map(|(n, _)| n.to_owned()))
            .collect();
        names.sort_unstable();
        names.dedup();
        match names.len() == add_hosts.len() {
            true => Ok(Self { add_hosts, ..self }),
            false => Err(SettingsError::AddHost),
        }
    }

    /// The endpoints a named network is limited to (`--container-egress-allow`) and the
    /// `nft` program that reads the firewall back (`--container-nft`, by default
    /// [`DEFAULT_NFT`]). With a named network at least one endpoint is required, so such a
    /// network never runs unchecked; with the default network there is nothing to check
    /// and neither flag may be given. [`Settings::new`] leaves the check off: a test's
    /// builder.
    ///
    /// # Errors
    ///
    /// [`SettingsError::EgressMissing`], [`SettingsError::EgressNoNetwork`],
    /// [`SettingsError::EgressAllow`], [`SettingsError::EgressCount`] or
    /// [`SettingsError::Nft`].
    pub fn with_egress(self, allow: &[String], nft: Option<&str>) -> Result<Self, SettingsError> {
        let named = self.network != DEFAULT_NETWORK;
        match (named, allow.is_empty(), nft.is_some()) {
            (false, true, false) => return Ok(self),
            (false, ..) => return Err(SettingsError::EgressNoNetwork),
            (true, true, _) => return Err(SettingsError::EgressMissing),
            (true, false, _) => {}
        }
        if allow.len() > EGRESS_MAX {
            return Err(SettingsError::EgressCount);
        }
        let allow = allow
            .iter()
            .map(|text| Allow::parse(text).ok_or(SettingsError::EgressAllow))
            .collect::<Result<Vec<_>, _>>()?;
        let nft = nft.unwrap_or(DEFAULT_NFT);
        fs::metadata(nft)
            .ok()
            .filter(|meta| nft.starts_with('/') && meta.is_file())
            .filter(|meta| meta.permissions().mode() & 0o111 != 0)
            .ok_or(SettingsError::Nft)?;
        Ok(Self {
            egress: Some(Egress::new(allow, nft)),
            ..self
        })
    }

    /// Settles who owns the run's scratch directory. It is private to the agent (`0700`),
    /// and the container's user must be able to write it. An agent that runs as root
    /// (`uid` 0) hands the directory to the container's user when it starts a job
    /// ([`Settings::chown_scratch`]); any other agent cannot give a file away, so the
    /// container's user must be the agent's own.
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

    /// The `<uid>:<gid>` the containers run as.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The uid the containers run as (the first half of [`Settings::user`]).
    pub fn uid(&self) -> u32 {
        self.ids.0
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

    /// The `<name>:<ip>` entries of `--container-add-host`, lower-cased names.
    pub fn add_hosts(&self) -> &[String] {
        &self.add_hosts
    }

    /// The egress check, when the network is a named one.
    pub fn egress(&self) -> Option<&Egress> {
        self.egress.as_ref()
    }

    /// The `(uid, gid)` to give the scratch directory of a run before its container is
    /// created: set when the agent runs as root, none when the container's user is the
    /// agent's own.
    pub fn chown_scratch(&self) -> Option<(u32, u32)> {
        self.chown
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

/// `<name>:<ip>` with a DNS name (labels of letters, digits and inner hyphens) and an
/// IPv4 or IPv6 address; the name is lower-cased, the address is written back in its
/// canonical form. `host-gateway` is an IP position word of the engines and is refused
/// because it is not an address.
fn add_host(text: &str) -> Result<String, SettingsError> {
    let (name, ip) = text.split_once(':').ok_or(SettingsError::AddHost)?;
    let label = |label: &str| {
        (1..=DNS_LABEL_MAX).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    let sound = name.len() <= DNS_NAME_MAX
        && name.split('.').all(label)
        && !name.eq_ignore_ascii_case("host-gateway");
    let ip: std::net::IpAddr = ip.parse().map_err(|_| SettingsError::AddHost)?;
    sound
        .then(|| format!("{}:{ip}", name.to_ascii_lowercase()))
        .ok_or(SettingsError::AddHost)
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
