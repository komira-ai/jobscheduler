//! The privileged half of egress: what the agent, as root in the host's initial network
//! namespace, does for a job's container that was started with a network namespace of its
//! own. Every call here is made by the agent process (nothing runs as the job user, and
//! nothing runs in the container), and the programs it starts are the three the operator
//! configured: `ip`, `nft` and `nsenter`.
//!
//! The container is made by `crun` as the job user, which creates the container's user and
//! network namespaces together; a network namespace that root made beforehand cannot be
//! joined by that user (`setns` needs `CAP_SYS_ADMIN` over the namespace's owner), so the
//! agent wires the namespace after the container is up, from outside. The container's init
//! holds the job until the wiring is done (`js-init --wait-route`): the job's first
//! instruction runs after the last step below, the default route.
//!
//! For a run whose container's first process is `pid` and whose job asked for `allow-list` or
//! `open` ([`Egress::attach`]; a `none` job is not wired at all):
//!
//! 1. take a /30 of the pool, and the names `jsvN` (host end) and `jspN` (job end);
//! 2. open `/proc/<pid>/ns/net` once, keep the descriptor, and refuse the host's own
//!    namespace (compared by device and inode); the descriptor pins that namespace, so both
//!    later steps name it as `/proc/<agent pid>/fd/<n>`, never by the container's pid again;
//! 3. in the initial namespace: make the veth pair, turn IPv6 off on the host end, address the
//!    host end and set it up;
//! 4. add `jsvN` to the firewall's set of wired interfaces of its level: `jobifs` for
//!    `allow-list`, `openifs` for `open`;
//! 5. move the job end into the namespace, named `eth0`;
//! 6. enter that namespace with `nsenter` and, in one `ip -batch`: raise `lo`, give `eth0` no
//!    IPv6 link-local address, address and raise it, and add the default route last.
//!
//! Releasing a run removes the set member and the host end (which takes the job end with
//! it); the container's namespace is the kernel's to free when its last process ends, so
//! nothing of it outlives the run. A start under the ceiling `allow-list` or `open` begins by
//! removing every interface named `jsv<digits>` or `jsp<digits>` (exactly; no other name) and
//! the two tables, because the keeper kills the containers with the agent: nothing a previous
//! process wired is in use; and only one agent at a time holds the host's egress
//! ([`HOST_LOCK`]), so the sweep never removes the live wiring of another agent. Mode none runs
//! no program and touches nothing.
//!
//! Everything the agent does to the machine goes through [`Sys`], so the unit tests script
//! the whole sequence, including a failure at each step, and the tests that need the real
//! kernel are in `tests/egress`.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use js_mac_session::disclaim::Program;
use js_types::JobNetwork;
use serde_json::Value;

use super::config::{Config, Mode};
use super::ruleset::{self, IF_PREFIX, IFACE_SET, NAT_TABLE, OPEN_SET, TABLE};

/// The name of the job's end of a pair before it is moved.
const PEER_PREFIX: &str = "jsp";

/// The name every job's interface has inside its container.
const INSIDE_NAME: &str = "eth0";

/// The abstract socket a wired ceiling (`allow-list` or `open`) holds bound while its egress
/// lives: one per network namespace (the host's, for the agent), freed by the kernel when the
/// process ends however it ends. The interfaces and tables it makes and removes are named for
/// the host, not for an agent, so one agent at a time may hold them.
pub const HOST_LOCK: &str = "js-agent-egress";

/// What holds the host's egress for an agent: the bound [`HOST_LOCK`].
pub(crate) type Held = Arc<dyn std::fmt::Debug + Send + Sync>;

/// Why egress could not be set up, checked or taken down. The text stays on the host: it
/// names what failed and may hold the output of `ip` or `nft`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressError {
    /// A program the agent runs failed, or could not be started.
    #[error("egress: {what}: {why}")]
    Command {
        /// What the agent was doing.
        what: String,
        /// The program's output or the reason it did not run.
        why: String,
    },
    /// Every block of the pool is in use.
    #[error("egress: every address block of the pool is in use")]
    Exhausted,
    /// A run was to be wired at a level above the ceiling the host was set up for; the
    /// admission ([`super::admit`]) refuses such a job before anything is made.
    #[error("egress: a {asked} job cannot be wired under the ceiling {ceiling}")]
    AboveCeiling {
        /// The level the run asked for.
        asked: JobNetwork,
        /// The ceiling.
        ceiling: JobNetwork,
    },
    /// The container is not in a network namespace of its own, or is gone.
    #[error("egress: container {pid}: {why}")]
    Container {
        /// The pid the agent was given.
        pid: u32,
        /// What is wrong.
        why: String,
    },
    /// IPv4 forwarding is off, so nothing a job opens would leave the host.
    #[error("egress: net.ipv4.ip_forward is not 1")]
    Forwarding,
    /// The tables in the kernel are not what the agent loaded.
    #[error("egress: the firewall tables are not what the agent loaded")]
    Rules,
    /// Another process of this host holds its egress ([`HOST_LOCK`]): starting would remove
    /// that agent's live interfaces and tables.
    #[error("egress: another agent on this host holds its egress (the socket @{HOST_LOCK})")]
    Held,
}

/// What the host side does to the machine; the real one is [`System`].
pub(crate) trait Sys: std::fmt::Debug + Send + Sync {
    /// Runs `program args`, with `stdin` as its standard input (empty when `None`), and
    /// returns its standard output; an error is the program's standard error, or why it did
    /// not run.
    fn run(&self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String>;
    /// Writes `text` to the file `path`.
    fn write(&self, path: &str, text: &str) -> std::io::Result<()>;
    /// Reads the file `path`.
    fn read(&self, path: &str) -> std::io::Result<String>;
    /// The device and inode of what `path` names, followed: for `/proc/self/ns/net` the
    /// identity of the agent's own network namespace.
    fn identity(&self, path: &str) -> std::io::Result<(u64, u64)>;
    /// Opens the network namespace of process `pid` and keeps it open as long as the
    /// result lives.
    fn open_netns(&self, pid: u32) -> std::io::Result<Netns>;
    /// Binds the abstract socket `name` ([`HOST_LOCK`] for an agent) and holds it as long as
    /// the result lives; `None` when another process holds it.
    fn lock_host(&self, name: &str) -> std::io::Result<Option<Held>>;
}

/// An open network namespace: its identity, and a path any process of the host can open to
/// get the same namespace (as long as this is alive).
#[derive(Debug)]
pub(crate) struct Netns {
    pub(crate) identity: (u64, u64),
    pub(crate) path: String,
    /// The descriptor that pins it.
    pub(crate) _held: Option<std::fs::File>,
}

/// The machine itself.
#[derive(Debug)]
pub(crate) struct System;

impl Sys for System {
    fn run(&self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
        let output = spawn(program, args, stdin.unwrap_or_default())
            .map_err(|why| format!("{program}: {why}"))?;
        match output.status.success() {
            true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            false => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        }
    }

    fn write(&self, path: &str, text: &str) -> std::io::Result<()> {
        std::fs::write(path, text)
    }

    fn read(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn identity(&self, path: &str) -> std::io::Result<(u64, u64)> {
        let meta = std::fs::metadata(path)?;
        Ok((meta.dev(), meta.ino()))
    }

    fn open_netns(&self, pid: u32) -> std::io::Result<Netns> {
        let file = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
        let meta = file.metadata()?;
        Ok(Netns {
            identity: (meta.dev(), meta.ino()),
            path: format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd()),
            _held: Some(file),
        })
    }

    fn lock_host(&self, name: &str) -> std::io::Result<Option<Held>> {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::{SocketAddr, UnixListener};
        let name = SocketAddr::from_abstract_name(name)?;
        let in_use = |why: std::io::Error| {
            (why.kind() == std::io::ErrorKind::AddrInUse)
                .then_some(None)
                .ok_or(why)
        };
        UnixListener::bind_addr(&name)
            .map(|bound| Some(Arc::new(bound) as Held))
            .or_else(in_use)
    }
}

/// Runs `program args` with `stdin` as its input to its end.
fn spawn(program: &str, args: &[&str], stdin: &str) -> std::io::Result<std::process::Output> {
    let mut child = Program::new(program, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // A program that stops reading early says why by its status and its words.
    let mut pipe = child.stdin.take().expect("the standard input is piped");
    let _ = pipe.write_all(stdin.as_bytes());
    drop(pipe);
    child.wait_with_output()
}

/// The host side of the egress ceiling: the pool of address blocks and the firewall. Cheap to
/// clone; every clone is the same pool, and two are equal when they are the same pool. A wired
/// ceiling (`allow-list` or `open`) holds the host's egress ([`HOST_LOCK`]) until its last
/// clone and its last [`Attachment`] are dropped: a run still wired keeps another start from
/// sweeping it.
#[derive(Debug, Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

impl PartialEq for Egress {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for Egress {}

#[derive(Debug)]
struct Inner {
    /// The hold on the host's egress; none in mode none.
    _host: Option<Held>,
    config: Config,
    sys: Arc<dyn Sys>,
    /// The indexes of the pool's blocks in use.
    used: Mutex<BTreeSet<u32>>,
    /// The tables as the agent loaded them, normalized; none in mode none.
    baseline: Option<Value>,
}

/// One run's wiring. Dropping it (or [`Attachment::release`]) takes it down.
#[derive(Debug)]
pub struct Attachment {
    inner: Arc<Inner>,
    /// The block's index, the host end's name and the set it is in; none for a run that is
    /// not wired (mode none, a `none` job) or once taken down.
    wired: Option<(u32, String, &'static str)>,
}

fn command(what: impl Into<String>, why: impl std::fmt::Display) -> EgressError {
    EgressError::Command {
        what: what.into(),
        why: why.to_string(),
    }
}

/// `sys.run` with the failure turned into an [`EgressError`] that says what was being done.
fn call(
    sys: &dyn Sys,
    what: &str,
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String, EgressError> {
    sys.run(program, args, stdin)
        .map_err(|why| command(what, format!("{program} {args:?}: {why}")))
}

/// Whether `error` says that the thing it tried to remove was not there.
fn gone(error: &EgressError) -> bool {
    let text = error.to_string();
    text.contains("No such file or directory") || text.contains("Cannot find device")
}

/// Writes `value` to the sysctl `path` (below `/proc/sys`). A host booted without IPv6 has
/// no switch for it: IPv6 is off already.
fn sysctl(sys: &dyn Sys, path: &str, value: &str) -> Result<(), EgressError> {
    match sys.write(&format!("/proc/sys/{path}"), value) {
        Err(why) if why.kind() == std::io::ErrorKind::NotFound && path.starts_with("net/ipv6/") => {
            Ok(())
        }
        other => other.map_err(|why| command(format!("setting {path}"), why)),
    }
}

impl Egress {
    /// Starts the host side for `config`. Mode none runs nothing. A wired ceiling
    /// (`allow-list` or `open`) first holds the host's egress ([`HOST_LOCK`]: another agent's
    /// live wiring is never swept), then removes what an earlier agent left (the interfaces
    /// `jsv<digits>` and `jsp<digits>`, both tables), switches IPv4 forwarding on (a host-wide
    /// switch, which stays on when the agent ends: other users of the host may rely on it, and
    /// the agent cannot know) and loads the firewall.
    ///
    /// # Errors
    ///
    /// [`EgressError::Held`], or a program could not run or refused what it was given.
    pub fn start(config: Config) -> Result<Self, EgressError> {
        Self::start_on(config, Arc::new(System))
    }

    fn start_on(config: Config, sys: Arc<dyn Sys>) -> Result<Self, EgressError> {
        Self::start_holding(config, sys, HOST_LOCK)
    }

    /// [`Egress::start_on`] holding the abstract socket `lock` in place of [`HOST_LOCK`]: a
    /// test on the real machine holds a name of its own, never the host's.
    fn start_holding(config: Config, sys: Arc<dyn Sys>, lock: &str) -> Result<Self, EgressError> {
        let (mut baseline, mut host) = (None, None);
        if config.mode() != Mode::None {
            let held = sys
                .lock_host(lock)
                .map_err(|why| command("holding the host's egress", why))?;
            host = Some(held.ok_or(EgressError::Held)?);
            sweep(&*sys, &config)?;
            sysctl(&*sys, "net/ipv4/ip_forward", "1\n")?;
            call(
                &*sys,
                "loading the firewall",
                config.nft(),
                &["-f", "-"],
                Some(&ruleset::render(&config)),
            )?;
            baseline = Some(listing(&*sys, &config)?);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                _host: host,
                config,
                sys,
                used: Mutex::new(BTreeSet::new()),
                baseline,
            }),
        })
    }

    /// The configuration this was started with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Whether the host is as the agent set it up: under a wired ceiling (`allow-list` or
    /// `open`) IPv4 forwarding is on and both tables are, rule for rule, what [`Egress::start`]
    /// loaded (a rule changed, added or removed, a chain unhooked, a table or a set gone).
    /// Counters and the members of the sets of wired interfaces are not compared. Mode none
    /// has nothing to check. The lifecycle asks before every create.
    ///
    /// # Errors
    ///
    /// [`EgressError::Forwarding`] or [`EgressError::Rules`], or a program that failed.
    pub fn check(&self) -> Result<(), EgressError> {
        let Some(baseline) = &self.inner.baseline else {
            return Ok(());
        };
        let forwarding = self.inner.sys.read("/proc/sys/net/ipv4/ip_forward");
        if !forwarding.is_ok_and(|text| text.trim() == "1") {
            return Err(EgressError::Forwarding);
        }
        let now = listing(&*self.inner.sys, &self.inner.config)?;
        match now == *baseline {
            true => Ok(()),
            false => Err(EgressError::Rules),
        }
    }

    /// Wires the network namespace of the container whose first process is `pid`, for run
    /// `run` (used in errors only), whose job asked for `level`: its interface goes in the set
    /// of that level (`jobifs` for `allow-list`, `openifs` for `open`). For a `none` job, and in
    /// mode none, there is nothing to wire and the result is an attachment that does nothing.
    ///
    /// The container's init must be waiting for its route (`OciConfig::route_wait`): the
    /// default route is the last thing made.
    ///
    /// # Errors
    ///
    /// [`EgressError::AboveCeiling`] for a level above the ceiling (nothing is made); the
    /// container is gone or shares the host's network namespace, the pool is full, or a
    /// program failed, and whatever was made is taken down again.
    pub fn attach(
        &self,
        run: &str,
        pid: u32,
        level: JobNetwork,
    ) -> Result<Attachment, EgressError> {
        let inner = &self.inner;
        let ceiling = inner.config.mode().level();
        if level > ceiling {
            return Err(EgressError::AboveCeiling {
                asked: level,
                ceiling,
            });
        }
        let set = match level {
            JobNetwork::None => {
                return Ok(Attachment {
                    inner: inner.clone(),
                    wired: None,
                });
            }
            JobNetwork::AllowList => IFACE_SET,
            JobNetwork::Open => OPEN_SET,
        };
        let index = take(inner)?;
        let attachment = Attachment {
            inner: inner.clone(),
            wired: Some((index, format!("{IF_PREFIX}{index}"), set)),
        };
        // On failure the attachment drops here and takes down whatever was made.
        wire(&*inner.sys, &inner.config, (index, set), pid).map_err(|why| match why {
            container @ EgressError::Container { .. } => container,
            other => command(format!("wiring run {run}"), other),
        })?;
        Ok(attachment)
    }
}

impl Attachment {
    /// The name of the host end of the run's veth pair, none for a run that is not wired.
    #[must_use]
    pub fn interface(&self) -> Option<&str> {
        self.wired.as_ref().map(|(_, name, _)| name.as_str())
    }

    /// Takes the run's wiring down: the set member and the host end. Nothing is left to do
    /// when the container already ended (the kernel removed the pair with its namespace).
    ///
    /// # Errors
    ///
    /// `nft` or `ip` failed for a reason other than the thing being gone already.
    pub fn release(mut self) -> Result<(), EgressError> {
        self.take_down()
    }

    fn take_down(&mut self) -> Result<(), EgressError> {
        let Some((index, name, set)) = self.wired.take() else {
            return Ok(());
        };
        let (config, sys) = (&self.inner.config, &*self.inner.sys);
        let element = format!("{{ \"{name}\" }}");
        let removed = call(
            sys,
            "removing the interface from the firewall set",
            config.nft(),
            &["delete", "element", "inet", TABLE, set, &element],
            None,
        );
        let deleted = call(
            sys,
            "deleting the veth",
            config.ip(),
            &["link", "del", &name],
            None,
        );
        self.inner
            .used
            .lock()
            .expect("the pool lock is never poisoned")
            .remove(&index);
        let mut result = Ok(());
        for step in [removed, deleted] {
            if let Err(error) = step
                && !gone(&error)
            {
                result = Err(error);
            }
        }
        result
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let _ = self.take_down();
    }
}

/// The lowest free block of the pool.
fn take(inner: &Inner) -> Result<u32, EgressError> {
    let mut used = inner.used.lock().expect("the pool lock is never poisoned");
    let index = (0..inner.config.pool().capacity())
        .find(|n| !used.contains(n))
        .ok_or(EgressError::Exhausted)?;
    used.insert(index);
    Ok(index)
}

/// The steps of the module documentation, for block `index`, whose interface goes in the set
/// `set`, and container `pid`.
fn wire(
    sys: &dyn Sys,
    config: &Config,
    (index, set): (u32, &str),
    pid: u32,
) -> Result<(), EgressError> {
    let (host_ip, job_ip) = config.pool().block(index).ok_or(EgressError::Exhausted)?;
    let (host, peer) = (
        format!("{IF_PREFIX}{index}"),
        format!("{PEER_PREFIX}{index}"),
    );
    let (ip, nft) = (config.ip(), config.nft());
    let container = |why: String| EgressError::Container { pid, why };

    // The namespace is opened once and held: what is wired is the namespace seen now and
    // not whatever a reused pid names later.
    let netns = sys
        .open_netns(pid)
        .map_err(|why| container(format!("/proc/{pid}/ns/net: {why}")))?;
    let own = sys
        .identity("/proc/self/ns/net")
        .map_err(|why| command("reading the agent's own network namespace", why))?;
    if own == netns.identity {
        return Err(container(
            "it shares the host's network namespace".to_owned(),
        ));
    }

    let step = |what: &str, program: &str, args: &[&str]| call(sys, what, program, args, None);
    step(
        "making the veth pair",
        ip,
        &["link", "add", &host, "type", "veth", "peer", "name", &peer],
    )?;
    sysctl(sys, &format!("net/ipv6/conf/{host}/disable_ipv6"), "1")?;
    step(
        "addressing the host end",
        ip,
        &["addr", "add", &format!("{host_ip}/30"), "dev", &host],
    )?;
    step("raising the host end", ip, &["link", "set", &host, "up"])?;
    step(
        "adding the interface to the firewall set",
        nft,
        &[
            "add",
            "element",
            "inet",
            TABLE,
            set,
            &format!("{{ \"{host}\" }}"),
        ],
    )?;

    step(
        "moving the job end into the container",
        ip,
        &[
            "link",
            "set",
            &peer,
            "netns",
            &netns.path,
            "name",
            INSIDE_NAME,
        ],
    )?;

    // No IPv6 link-local address is made for `eth0`, and the default route comes last.
    let batch = format!(
        "link set lo up\nlink set {INSIDE_NAME} addrgenmode none\n\
         addr add {job_ip}/30 dev {INSIDE_NAME}\nlink set {INSIDE_NAME} up\n\
         route add default via {host_ip}\n"
    );
    call(
        sys,
        "configuring the job end",
        config.nsenter(),
        &[&format!("--net={}", netns.path), "--", ip, "-batch", "-"],
        Some(&batch),
    )
    .map(drop)
}

/// Both tables as the kernel holds them, normalized.
fn listing(sys: &dyn Sys, config: &Config) -> Result<Value, EgressError> {
    let mut tables = Vec::new();
    for (family, table) in [("inet", TABLE), ("ip", NAT_TABLE)] {
        let text = call(
            sys,
            "reading the firewall back",
            config.nft(),
            &["-j", "list", "table", family, table],
            None,
        )?;
        tables.push(
            ruleset::normalize(&text).map_err(|why| command("reading the firewall back", why))?,
        );
    }
    Ok(Value::Array(tables))
}

/// Whether `name` is `prefix` and then digits, nothing else (`jsv12`, not `jsvx` or `jspare0`).
fn numbered(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Removes every interface an earlier wired ceiling made and both tables.
fn sweep(sys: &dyn Sys, config: &Config) -> Result<(), EgressError> {
    let text = call(
        sys,
        "listing the interfaces",
        config.ip(),
        &["-j", "link", "show"],
        None,
    )?;
    let links: Vec<Value> =
        serde_json::from_str(&text).map_err(|why| command("listing the interfaces", why))?;
    let leftovers = links
        .iter()
        .filter_map(|link| link.get("ifname").and_then(Value::as_str))
        .filter(|name| numbered(name, IF_PREFIX) || numbered(name, PEER_PREFIX));
    for name in leftovers {
        // Deleting one end of a pair deletes the other, which may be later in the list.
        let deleted = call(
            sys,
            "deleting a leftover veth",
            config.ip(),
            &["link", "del", name],
            None,
        );
        if let Err(error) = deleted
            && !gone(&error)
        {
            return Err(error);
        }
    }
    // Add then delete makes the removal work whether or not the tables exist.
    call(
        sys,
        "removing the earlier tables",
        config.nft(),
        &["-f", "-"],
        Some(&format!(
            "add table inet {TABLE}\ndelete table inet {TABLE}\n\
             add table ip {NAT_TABLE}\ndelete table ip {NAT_TABLE}\n"
        )),
    )
    .map(drop)
}

#[cfg(test)]
mod tests;
