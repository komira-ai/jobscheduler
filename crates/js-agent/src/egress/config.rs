//! The one egress setting of the agent, checked once at start: how much network a job's
//! container has. There is exactly one mode at a time.
//!
//! - `none` (the default): loopback only and no route. Nothing is set up on the host.
//! - `allow-list`: the container has one interface whose only route leads to the host,
//!   and the host forwards what the job opens to the endpoints this configuration names
//!   (`--egress-allow`, TCP, `IPV4:PORT` or `CIDR:PORT`) and the resolvers it names
//!   (`--egress-dns`, port 53), and refuses everything else, the link-local range
//!   (cloud metadata) first and IPv6 entirely.
//!
//! The flags are the only configuration: no environment variable, no file the job can reach.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use crate::cli_runtime::{Allow, EGRESS_ALLOW_MAX};

/// The addresses the per-run /30 blocks are taken from, unless `--egress-pool` names others:
/// the benchmarking range, which no real network routes.
pub const DEFAULT_POOL: &str = "198.18.0.0/16";

/// The `ip` program the agent runs, unless `--egress-ip` names another.
pub const DEFAULT_IP: &str = "/usr/sbin/ip";

/// The `nft` program the agent runs, unless `--egress-nft` names another.
pub const DEFAULT_NFT: &str = "/usr/sbin/nft";

/// The `nsenter` program the agent runs, unless `--egress-nsenter` names another.
pub const DEFAULT_NSENTER: &str = "/usr/bin/nsenter";

/// The most `--egress-dns` entries accepted.
pub const DNS_MAX: usize = 4;

/// How long a container's init waits for its network before the job is given up on.
pub const ROUTE_WAIT: Duration = Duration::from_secs(30);

/// The longest prefix a pool may have: 64 runs at a time at the most.
const POOL_PREFIX_MAX: u8 = 24;

/// The shortest prefix a pool may have: 16384 runs at a time at the most is plenty.
const POOL_PREFIX_MIN: u8 = 16;

/// How the network of a job's container is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Loopback only, no route.
    None,
    /// An interface to the host, limited to the configured endpoints.
    AllowList,
}

/// The address pool for the per-run blocks: an IPv4 network of 16 to 24 bits, cut into
/// blocks of four addresses, one per run (the host's end, the job's end, and the two a
/// /30 reserves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    base: u32,
    prefix: u8,
}

impl Pool {
    /// `a.b.c.d/len`, no host bits set, `len` from 16 to 24, outside loopback, link-local
    /// and multicast space.
    fn parse(text: &str) -> Option<Self> {
        let (addr, len) = text.split_once('/')?;
        let prefix: u8 = len.parse().ok()?;
        let base = u32::from(addr.parse::<Ipv4Addr>().ok()?);
        let host = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
        let first = Ipv4Addr::from(base);
        let sound = (POOL_PREFIX_MIN..=POOL_PREFIX_MAX).contains(&prefix)
            && base & host == 0
            && !first.is_loopback()
            && !first.is_link_local()
            && !first.is_multicast()
            && !first.is_unspecified();
        sound.then_some(Self { base, prefix })
    }

    /// The pool as `addr/len`.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}/{}", Ipv4Addr::from(self.base), self.prefix)
    }

    /// The network address and the prefix length.
    #[must_use]
    pub fn network(&self) -> (Ipv4Addr, u8) {
        (Ipv4Addr::from(self.base), self.prefix)
    }

    /// How many runs at once the pool holds.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        1 << (30 - u32::from(self.prefix))
    }

    /// The host's end of block `index` and the job's end: the second and third address of
    /// the block, as a /30 has them.
    #[must_use]
    pub fn block(&self, index: u32) -> Option<(Ipv4Addr, Ipv4Addr)> {
        (index < self.capacity()).then(|| {
            let first = self.base + index * 4;
            (Ipv4Addr::from(first + 1), Ipv4Addr::from(first + 2))
        })
    }

    fn contains(&self, addr: Ipv4Addr) -> bool {
        let host = u32::MAX.checked_shr(u32::from(self.prefix)).unwrap_or(0);
        u32::from(addr) & !host == self.base
    }
}

/// Why the egress flags were refused. Each text names the flag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `--egress` is neither `none` nor `allow-list`.
    #[error("--egress: expected none or allow-list")]
    Mode,
    /// A flag that only an allow-list takes was given with mode none.
    #[error("{0}: needs --egress allow-list")]
    NeedsAllowList(&'static str),
    /// An allow-list with nothing to allow.
    #[error("--egress allow-list: needs at least one --egress-allow")]
    Empty,
    /// An `--egress-allow` is not `<ipv4>[/24-32]:<port>` outside 169.254.0.0/16.
    #[error("--egress-allow: not <ipv4>[/24-32]:<port> outside 169.254.0.0/16")]
    Allow,
    /// More than the most `--egress-allow` entries.
    #[error("--egress-allow: at most 16 entries")]
    AllowMany,
    /// An `--egress-dns` is not a unicast IPv4 address outside 169.254.0.0/16.
    #[error("--egress-dns: not a unicast IPv4 address outside 169.254.0.0/16")]
    Dns,
    /// More than the most `--egress-dns` entries.
    #[error("--egress-dns: at most 4 entries")]
    DnsMany,
    /// `--egress-pool` is not an IPv4 network of 16 to 24 bits.
    #[error(
        "--egress-pool: not <ipv4>/<16-24> with no host bits, outside loopback, link-local and multicast space"
    )]
    Pool,
    /// An allowed endpoint or a resolver lies in the pool: a job's own address block.
    #[error("{0}: lies in the --egress-pool, which is the jobs' own address space")]
    InPool(&'static str),
    /// `--egress-ip`, `--egress-nft` or `--egress-nsenter` is not an absolute path to an
    /// executable file.
    #[error("{0}: not an absolute UTF-8 path to an executable file")]
    Program(&'static str),
    /// Egress needs the Linux host side.
    #[error("--egress allow-list: only a Linux host can make a job's network")]
    NotLinux,
}

/// The egress flags as the command line gave them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags<'a> {
    /// `--egress`.
    pub mode: Option<&'a str>,
    /// `--egress-allow`, each `IP:PORT` or `CIDR:PORT`.
    pub allow: &'a [String],
    /// `--egress-dns`, each an IPv4 address.
    pub dns: &'a [String],
    /// `--egress-pool`.
    pub pool: Option<&'a str>,
    /// `--egress-ip`.
    pub ip: Option<&'a str>,
    /// `--egress-nft`.
    pub nft: Option<&'a str>,
    /// `--egress-nsenter`.
    pub nsenter: Option<&'a str>,
}

/// The checked egress configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    mode: Mode,
    allow: Vec<Allow>,
    dns: Vec<Ipv4Addr>,
    pool: Pool,
    ip: String,
    nft: String,
    nsenter: String,
}

impl Default for Config {
    /// Mode none.
    fn default() -> Self {
        Self::none()
    }
}

impl Config {
    /// Mode none: loopback only, no route.
    #[must_use]
    pub fn none() -> Self {
        Self {
            mode: Mode::None,
            allow: Vec::new(),
            dns: Vec::new(),
            pool: Pool::parse(DEFAULT_POOL).expect("the default pool is sound"),
            ip: DEFAULT_IP.to_owned(),
            nft: DEFAULT_NFT.to_owned(),
            nsenter: DEFAULT_NSENTER.to_owned(),
        }
    }

    /// Checks the flags: the mode, the endpoints (each once, sorted), the resolvers, the
    /// pool and the two programs.
    ///
    /// # Errors
    ///
    /// The flag at fault; see [`ConfigError`].
    pub fn from_flags(flags: &Flags<'_>) -> Result<Self, ConfigError> {
        let mode = match flags.mode {
            None | Some("none") => Mode::None,
            Some("allow-list") => Mode::AllowList,
            Some(_) => return Err(ConfigError::Mode),
        };
        if mode == Mode::None {
            let given = [
                ("--egress-allow", !flags.allow.is_empty()),
                ("--egress-dns", !flags.dns.is_empty()),
                ("--egress-pool", flags.pool.is_some()),
                ("--egress-ip", flags.ip.is_some()),
                ("--egress-nft", flags.nft.is_some()),
                ("--egress-nsenter", flags.nsenter.is_some()),
            ];
            if let Some((name, _)) = given.iter().find(|(_, given)| *given) {
                return Err(ConfigError::NeedsAllowList(name));
            }
            return Ok(Self::none());
        }
        platform()?;
        let pool = Pool::parse(flags.pool.unwrap_or(DEFAULT_POOL)).ok_or(ConfigError::Pool)?;
        if flags.allow.is_empty() {
            return Err(ConfigError::Empty);
        }
        if flags.allow.len() > EGRESS_ALLOW_MAX {
            return Err(ConfigError::AllowMany);
        }
        let mut allow = flags
            .allow
            .iter()
            .map(|text| Allow::parse(text).ok_or(ConfigError::Allow))
            .collect::<Result<Vec<_>, _>>()?;
        allow.sort_by_key(|a| (u32::from(a.addr()), a.prefix_len(), a.port()));
        allow.dedup();
        if allow.iter().any(|a| a.overlaps(pool.network())) {
            return Err(ConfigError::InPool("--egress-allow"));
        }
        if flags.dns.len() > DNS_MAX {
            return Err(ConfigError::DnsMany);
        }
        let mut dns = flags
            .dns
            .iter()
            .map(|text| text.parse::<Ipv4Addr>().ok().filter(|a| resolver(*a)))
            .collect::<Option<Vec<_>>>()
            .ok_or(ConfigError::Dns)?;
        dns.sort();
        dns.dedup();
        if dns.iter().any(|a| pool.contains(*a)) {
            return Err(ConfigError::InPool("--egress-dns"));
        }
        Ok(Self {
            mode,
            allow,
            dns,
            pool,
            ip: program("--egress-ip", flags.ip, DEFAULT_IP)?,
            nft: program("--egress-nft", flags.nft, DEFAULT_NFT)?,
            nsenter: program("--egress-nsenter", flags.nsenter, DEFAULT_NSENTER)?,
        })
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The TCP endpoints jobs may open, sorted, each once.
    #[must_use]
    pub fn allow(&self) -> &[Allow] {
        &self.allow
    }

    /// The resolvers jobs may ask (UDP and TCP, port 53), sorted, each once.
    #[must_use]
    pub fn dns(&self) -> &[Ipv4Addr] {
        &self.dns
    }

    /// The pool the runs' address blocks are cut from.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.pool
    }

    /// The `ip` program.
    #[must_use]
    pub fn ip(&self) -> &str {
        &self.ip
    }

    /// The `nft` program.
    #[must_use]
    pub fn nft(&self) -> &str {
        &self.nft
    }

    /// The `nsenter` program.
    #[must_use]
    pub fn nsenter(&self) -> &str {
        &self.nsenter
    }

    /// The `/etc/resolv.conf` of a job's container when this configuration names resolvers:
    /// those, and no other. `None` when it names none (mode none, or an allow-list whose
    /// jobs resolve nothing), where the backend's own file stands: a loopback nameserver
    /// that nothing listens on.
    #[must_use]
    pub fn resolv_conf(&self) -> Option<String> {
        (!self.dns.is_empty()).then(|| {
            let mut text = String::new();
            for server in &self.dns {
                text.push_str(&format!("nameserver {server}\n"));
            }
            text.push_str("options timeout:1 attempts:1\n");
            text
        })
    }

    /// How long a container's init waits for its network: `None` in mode none, where there
    /// is nothing to wait for.
    #[must_use]
    pub fn route_wait(&self) -> Option<Duration> {
        (self.mode == Mode::AllowList).then_some(ROUTE_WAIT)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::AllowList => "allow-list",
        })
    }
}

/// Whether this host can make a job's network: Linux only.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_wraps)]
fn platform() -> Result<(), ConfigError> {
    Ok(())
}

/// Whether this host can make a job's network: Linux only.
#[cfg(not(target_os = "linux"))]
fn platform() -> Result<(), ConfigError> {
    Err(ConfigError::NotLinux)
}

/// Whether `addr` may be a resolver: unicast, and not in the metadata range.
fn resolver(addr: Ipv4Addr) -> bool {
    !(addr.is_loopback()
        || addr.is_unspecified()
        || addr.is_multicast()
        || addr.is_broadcast()
        || addr.is_link_local())
}

/// An absolute path to an executable regular file, or the default.
fn program(flag: &'static str, given: Option<&str>, default: &str) -> Result<String, ConfigError> {
    let path = given.unwrap_or(default);
    let executable = Path::new(path).is_absolute()
        && std::fs::metadata(path).is_ok_and(|meta| {
            use std::os::unix::fs::PermissionsExt as _;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        });
    executable
        .then(|| path.to_owned())
        .ok_or(ConfigError::Program(flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// Flags for an allow-list with the two programs the test host has.
    #[cfg(target_os = "linux")]
    fn allow_list<'a>(allow: &'a [String], dns: &'a [String]) -> Flags<'a> {
        Flags {
            mode: Some("allow-list"),
            allow,
            dns,
            pool: None,
            ip: Some("/bin/sh"),
            nft: Some("/bin/sh"),
            nsenter: Some("/bin/sh"),
        }
    }

    /// Catches a default that gives jobs a network, and flags of an allow-list that a mode
    /// none accepts silently: no flag at all and `--egress none` are mode none with nothing
    /// set up, and every other egress flag names the mode it needs.
    #[test]
    fn the_default_is_none_and_the_other_flags_need_an_allow_list() {
        let none = Config::from_flags(&Flags::default()).expect("none");
        assert_eq!(none, Config::none());
        assert_eq!(none.mode(), Mode::None);
        assert_eq!(none.route_wait(), None);
        assert_eq!(none.resolv_conf(), None);
        let explicit = Flags {
            mode: Some("none"),
            ..Flags::default()
        };
        assert_eq!(Config::from_flags(&explicit), Ok(Config::none()));
        let allow = strings(&["192.0.2.10:443"]);
        let dns = strings(&["192.0.2.53"]);
        let cases = [
            (
                Flags {
                    allow: &allow,
                    ..Flags::default()
                },
                "--egress-allow",
            ),
            (
                Flags {
                    dns: &dns,
                    ..Flags::default()
                },
                "--egress-dns",
            ),
            (
                Flags {
                    pool: Some("198.18.0.0/16"),
                    ..Flags::default()
                },
                "--egress-pool",
            ),
            (
                Flags {
                    ip: Some("/bin/sh"),
                    ..Flags::default()
                },
                "--egress-ip",
            ),
            (
                Flags {
                    nft: Some("/bin/sh"),
                    ..Flags::default()
                },
                "--egress-nft",
            ),
            (
                Flags {
                    nsenter: Some("/bin/sh"),
                    ..Flags::default()
                },
                "--egress-nsenter",
            ),
        ];
        for (flags, name) in cases {
            assert_eq!(
                Config::from_flags(&flags),
                Err(ConfigError::NeedsAllowList(name)),
                "{name}"
            );
        }
        let other = Flags {
            mode: Some("open"),
            ..Flags::default()
        };
        assert_eq!(Config::from_flags(&other), Err(ConfigError::Mode));
    }

    /// Catches an allow-list that accepts what it cannot enforce: nothing to allow, an
    /// endpoint that is not IPv4 CIDR and port, one that touches the metadata range, too
    /// many endpoints or resolvers, a resolver that is loopback, link-local or not an
    /// address, a pool of the wrong size, an endpoint or a resolver inside the pool, and a
    /// program that is not an absolute executable.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_allow_list_is_refused_for_each_thing_it_cannot_enforce() {
        let one = strings(&["192.0.2.10:443"]);
        let refused = |flags: Flags<'_>| Config::from_flags(&flags).expect_err("refused");
        assert_eq!(refused(allow_list(&[], &[])), ConfigError::Empty);
        for bad in [
            "192.0.2.10",
            "192.0.2.10:0",
            "192.0.2.0/16:443",
            "169.254.169.254:80",
            "[::1]:80",
            "x:80",
        ] {
            let bad = strings(&[bad]);
            assert_eq!(
                refused(allow_list(&bad, &[])),
                ConfigError::Allow,
                "{bad:?}"
            );
        }
        let many: Vec<String> = (1..=17).map(|n| format!("192.0.2.{n}:443")).collect();
        assert_eq!(refused(allow_list(&many, &[])), ConfigError::AllowMany);
        for bad in [
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "name",
        ] {
            let bad = strings(&[bad]);
            assert_eq!(refused(allow_list(&one, &bad)), ConfigError::Dns, "{bad:?}");
        }
        let many = strings(&[
            "192.0.2.1",
            "192.0.2.2",
            "192.0.2.3",
            "192.0.2.4",
            "192.0.2.5",
        ]);
        assert_eq!(refused(allow_list(&one, &many)), ConfigError::DnsMany);
        for pool in [
            "198.18.0.0/8",
            "198.18.0.0/25",
            "198.18.0.1/16",
            "127.0.0.0/16",
            "169.254.0.0/16",
            "224.0.0.0/16",
            "0.0.0.0/16",
            "198.18.0.0/x",
            "x/16",
            "x",
        ] {
            let flags = Flags {
                pool: Some(pool),
                ..allow_list(&one, &[])
            };
            assert_eq!(refused(flags), ConfigError::Pool, "{pool}");
        }
        let inside = strings(&["198.18.5.0/24:443"]);
        assert_eq!(
            refused(allow_list(&inside, &[])),
            ConfigError::InPool("--egress-allow")
        );
        let dns = strings(&["198.18.0.53"]);
        assert_eq!(
            refused(allow_list(&one, &dns)),
            ConfigError::InPool("--egress-dns")
        );
        for ip in ["ip", "/nonexistent/ip", "/etc/passwd", "/tmp"] {
            let flags = Flags {
                ip: Some(ip),
                ..allow_list(&one, &[])
            };
            assert_eq!(refused(flags), ConfigError::Program("--egress-ip"), "{ip}");
        }
        let flags = Flags {
            nft: Some("/etc/passwd"),
            ..allow_list(&one, &[])
        };
        assert_eq!(refused(flags), ConfigError::Program("--egress-nft"));
        let flags = Flags {
            nsenter: Some("/nonexistent/nsenter"),
            ..allow_list(&one, &[])
        };
        assert_eq!(refused(flags), ConfigError::Program("--egress-nsenter"));
    }

    /// Catches a configuration that keeps what was given in the order or multiplicity it
    /// came in (the firewall text and the read-back compare it), and the resolver file and
    /// the wait that an allow-list implies: the resolvers named, only them; the wait set.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_allow_list_is_sorted_once_and_names_its_resolvers_only() {
        let allow = strings(&["203.0.113.0/24:443", "192.0.2.10:9300", "192.0.2.10:9300"]);
        let dns = strings(&["192.0.2.54", "192.0.2.53", "192.0.2.53"]);
        let config = Config::from_flags(&allow_list(&allow, &dns)).expect("sound");
        let shown: Vec<String> = config.allow().iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["192.0.2.10:9300", "203.0.113.0/24:443"]);
        assert_eq!(
            config.dns(),
            [Ipv4Addr::new(192, 0, 2, 53), Ipv4Addr::new(192, 0, 2, 54)]
        );
        assert_eq!(
            config.resolv_conf().as_deref(),
            Some("nameserver 192.0.2.53\nnameserver 192.0.2.54\noptions timeout:1 attempts:1\n")
        );
        assert_eq!(config.route_wait(), Some(ROUTE_WAIT));
        let without = Config::from_flags(&allow_list(&allow, &[])).expect("sound");
        assert_eq!(without.resolv_conf(), None);
        assert_eq!(config.pool().text(), DEFAULT_POOL);
        assert_eq!(config.pool().network(), (Ipv4Addr::new(198, 18, 0, 0), 16));
        assert_eq!(config.mode(), Mode::AllowList);
        assert_eq!(
            (config.ip(), config.nft(), config.nsenter()),
            ("/bin/sh", "/bin/sh", "/bin/sh")
        );
    }

    /// Catches the defaults drifting from the documented ones and the mode's name: the
    /// default configuration is mode none with the default programs, and the two modes print
    /// as the flag spells them.
    #[test]
    fn the_defaults_are_mode_none_and_the_documented_programs() {
        let config = Config::default();
        assert_eq!(config, Config::none());
        assert_eq!(
            (config.ip(), config.nft(), config.nsenter()),
            (DEFAULT_IP, DEFAULT_NFT, DEFAULT_NSENTER)
        );
        assert_eq!(config.pool().text(), DEFAULT_POOL);
        assert_eq!(Mode::None.to_string(), "none");
        assert_eq!(Mode::AllowList.to_string(), "allow-list");
    }

    /// Catches a block arithmetic that hands two runs the same address, or an address
    /// outside the pool: block `n` is the /30 at `4n`, the host end its second address and
    /// the job's end its third, and the pool's capacity is what its prefix allows.
    #[test]
    fn the_pool_is_cut_into_thirty_bit_blocks() {
        let pool = Pool::parse("198.18.0.0/16").expect("pool");
        assert_eq!(pool.capacity(), 16384);
        assert_eq!(
            pool.block(0),
            Some((Ipv4Addr::new(198, 18, 0, 1), Ipv4Addr::new(198, 18, 0, 2)))
        );
        assert_eq!(
            pool.block(1),
            Some((Ipv4Addr::new(198, 18, 0, 5), Ipv4Addr::new(198, 18, 0, 6)))
        );
        assert_eq!(
            pool.block(16383),
            Some((
                Ipv4Addr::new(198, 18, 255, 253),
                Ipv4Addr::new(198, 18, 255, 254)
            ))
        );
        assert_eq!(pool.block(16384), None);
        assert_eq!(Pool::parse("10.1.2.0/24").expect("pool").capacity(), 64);
    }
}
