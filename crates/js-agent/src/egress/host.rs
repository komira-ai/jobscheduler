//! The privileged half of egress: what the agent, as root in the host's initial network
//! namespace, does for a job's container that was started with a network namespace of its
//! own. Every call here is made by the agent process (nothing runs as the job user, and
//! nothing runs in the container), and the programs it starts are the two the operator
//! configured: `ip` and `nft`.
//!
//! The container is made by `crun` as the job user, which creates the container's user and
//! network namespaces together; a network namespace that root made beforehand cannot be
//! joined by that user (`setns` needs `CAP_SYS_ADMIN` over the namespace's owner), so the
//! agent wires the namespace after the container is up, from the outside. The container's
//! init holds the job until the wiring is done (`js-init --wait-route`): the job's first
//! instruction runs after the last step below, the default route.
//!
//! For a run whose container's first process is `pid` ([`Egress::attach`]):
//!
//! 1. take a /30 from the pool, and the names `jsvN` (host end) and `jspN` (job end);
//! 2. in the initial namespace: make the veth pair, turn IPv6 off on the host end, switch
//!    reverse-path filtering on, address the host end and set it up;
//! 3. add `jsvN` to the firewall's set of wired interfaces;
//! 4. move the job end into the container's namespace (through the open
//!    `/proc/<pid>/ns/net`, not the pid alone), named `eth0`;
//! 5. in that namespace: turn IPv6 off, address and raise `eth0`, and add the default route
//!    last.
//!
//! Releasing a run removes the set member and the host end (which takes the job end with
//! it); the container's namespace is the kernel's to free when its last process ends, so
//! nothing of it outlives the run. A restart of the agent starts by removing every `jsv*`
//! and `jsp*` interface and the two tables, because the keeper kills the containers with
//! the agent: nothing a previous process wired is in use.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};
use serde_json::Value;

use super::config::{Config, Mode};
use super::ruleset::{self, IF_PREFIX, IFACE_SET, NAT_TABLE, TABLE};

/// The name of the job's end of a pair before it is moved, and the name every job's
/// interface has inside its container.
const PEER_PREFIX: &str = "jsp";
const INSIDE_NAME: &str = "eth0";

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
}

fn command(what: impl Into<String>, why: impl std::fmt::Display) -> EgressError {
    EgressError::Command {
        what: what.into(),
        why: why.to_string(),
    }
}

/// The host side of the egress mode: the pool of address blocks and the firewall. Cheap to
/// clone; every clone is the same pool.
#[derive(Debug, Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: Config,
    /// The indexes of the pool's blocks in use.
    used: Mutex<BTreeSet<u32>>,
    /// The tables as the agent loaded them, normalized; none in mode none.
    baseline: Option<Value>,
}

/// One run's wiring. Dropping it (or [`Attachment::release`]) takes it down.
#[derive(Debug)]
pub struct Attachment {
    inner: Arc<Inner>,
    /// The block's index and the host end's name, none in mode none.
    wired: Option<(u32, String)>,
}

impl Egress {
    /// Starts the host side for `config`: removes what an earlier agent left (every `jsv*`
    /// and `jsp*` interface, both tables), and in allow-list mode switches IPv4 forwarding
    /// on and loads the firewall.
    ///
    /// # Errors
    ///
    /// A program could not run or refused what it was given.
    pub fn start(config: Config) -> Result<Self, EgressError> {
        sweep(&config)?;
        let mut baseline = None;
        if config.mode() == Mode::AllowList {
            std::fs::write("/proc/sys/net/ipv4/ip_forward", "1\n")
                .map_err(|why| command("switching IPv4 forwarding on", why))?;
            run(
                config.nft(),
                &["-f", "-"],
                Some(&ruleset::render(&config)),
                "loading the firewall",
            )?;
            baseline = Some(listing(&config)?);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
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

    /// Whether the host is as the agent set it up: in allow-list mode IPv4 forwarding is on
    /// and both tables are, rule for rule, what [`Egress::start`] loaded (a rule changed,
    /// added or removed, a chain unhooked, a table gone). Counters and the members of the
    /// set of wired interfaces are not compared. Mode none has nothing to check. The lifecycle
    /// asks before every create.
    ///
    /// # Errors
    ///
    /// [`EgressError::Forwarding`] or [`EgressError::Rules`], or a program that failed.
    pub fn check(&self) -> Result<(), EgressError> {
        let Some(baseline) = &self.inner.baseline else {
            return Ok(());
        };
        let forwarding = std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward");
        if !forwarding.is_ok_and(|text| text.trim() == "1") {
            return Err(EgressError::Forwarding);
        }
        if listing(&self.inner.config)? == *baseline {
            Ok(())
        } else {
            Err(EgressError::Rules)
        }
    }

    /// Wires the network namespace of the container whose first process is `pid`, for run
    /// `run` (used in errors only). In mode none there is nothing to wire and the result is
    /// an attachment that does nothing.
    ///
    /// The container's init must be waiting for its route (`OciConfig::route_wait`): the
    /// default route is the last thing made.
    ///
    /// # Errors
    ///
    /// The container is gone or shares the host's network namespace, the pool is full, or a
    /// program failed; whatever was made is taken down again.
    pub fn attach(&self, run: &str, pid: u32) -> Result<Attachment, EgressError> {
        let inner = &self.inner;
        if inner.config.mode() == Mode::None {
            return Ok(Attachment {
                inner: inner.clone(),
                wired: None,
            });
        }
        let index = take(inner)?;
        let name = format!("{IF_PREFIX}{index}");
        let attachment = Attachment {
            inner: inner.clone(),
            wired: Some((index, name)),
        };
        // On failure the attachment drops here and takes down whatever was made.
        wire(&inner.config, index, pid).map_err(|why| command(format!("wiring run {run}"), why))?;
        Ok(attachment)
    }
}

impl Attachment {
    /// The name of the host end of the run's veth pair, none in mode none.
    #[must_use]
    pub fn interface(&self) -> Option<&str> {
        self.wired.as_ref().map(|(_, name)| name.as_str())
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
        let Some((index, name)) = self.wired.take() else {
            return Ok(());
        };
        let config = &self.inner.config;
        let element = format!("{{ \"{name}\" }}");
        let removed = run(
            config.nft(),
            &["delete", "element", "inet", TABLE, IFACE_SET, &element],
            None,
            "removing the interface from the firewall set",
        );
        let deleted = run(
            config.ip(),
            &["link", "del", &name],
            None,
            "deleting the veth",
        );
        self.inner
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&index);
        for result in [removed, deleted] {
            match result {
                Err(EgressError::Command { why, .. })
                    if why.contains("No such file or directory")
                        || why.contains("Cannot find device") => {}
                other => other.map(drop)?,
            }
        }
        Ok(())
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let _ = self.take_down();
    }
}

/// The lowest free block of the pool.
fn take(inner: &Inner) -> Result<u32, EgressError> {
    let mut used = inner.used.lock().unwrap_or_else(PoisonError::into_inner);
    let capacity = inner.config.pool().capacity();
    let index = (0..capacity)
        .find(|n| !used.contains(n))
        .ok_or(EgressError::Exhausted)?;
    used.insert(index);
    Ok(index)
}

/// The steps of the module documentation, for block `index` and container `pid`.
fn wire(config: &Config, index: u32, pid: u32) -> Result<(), EgressError> {
    let (host_ip, job_ip) = config.pool().block(index).ok_or(EgressError::Exhausted)?;
    let (host, peer) = (
        format!("{IF_PREFIX}{index}"),
        format!("{PEER_PREFIX}{index}"),
    );
    let ip = config.ip();
    let container = |why: &str| EgressError::Container {
        pid,
        why: why.to_owned(),
    };

    // The namespace is held open from here, so what is wired is the namespace seen now and
    // not whatever a reused pid names later.
    let netns_path = format!("/proc/{pid}/ns/net");
    let netns =
        File::open(&netns_path).map_err(|why| container(&format!("{netns_path}: {why}")))?;
    let own = std::fs::metadata("/proc/self/ns/net")
        .map_err(|why| command("reading the agent's own network namespace", why))?;
    let theirs = netns
        .metadata()
        .map_err(|why| container(&format!("{netns_path}: {why}")))?;
    if (own.dev(), own.ino()) == (theirs.dev(), theirs.ino()) {
        return Err(container("it shares the host's network namespace"));
    }

    run(
        ip,
        &["link", "add", &host, "type", "veth", "peer", "name", &peer],
        None,
        "making the veth pair",
    )?;
    sysctl(&format!("net/ipv6/conf/{host}/disable_ipv6"), "1")?;
    sysctl(&format!("net/ipv4/conf/{host}/rp_filter"), "1")?;
    run(
        ip,
        &["addr", "add", &format!("{host_ip}/30"), "dev", &host],
        None,
        "addressing the host end",
    )?;
    run(
        ip,
        &["link", "set", &host, "up"],
        None,
        "raising the host end",
    )?;
    run(
        config.nft(),
        &[
            "add",
            "element",
            "inet",
            TABLE,
            IFACE_SET,
            &format!("{{ \"{host}\" }}"),
        ],
        None,
        "adding the interface to the firewall set",
    )?;

    // The container must still be the one whose namespace is held.
    let again =
        std::fs::metadata(&netns_path).map_err(|why| container(&format!("{netns_path}: {why}")))?;
    if (again.dev(), again.ino()) != (theirs.dev(), theirs.ino()) {
        return Err(container("its network namespace changed"));
    }
    run(
        ip,
        &[
            "link",
            "set",
            &peer,
            "netns",
            &pid.to_string(),
            "name",
            INSIDE_NAME,
        ],
        None,
        "moving the job end into the container",
    )?;

    let batch = format!(
        "link set lo up\naddr add {job_ip}/30 dev {INSIDE_NAME}\nlink set {INSIDE_NAME} up\n\
         route add default via {host_ip}\n"
    );
    in_netns(&netns, || -> Result<(), EgressError> {
        // IPv6 is off before the interface is up, so it never has an address.
        for scope in ["all", "default", INSIDE_NAME] {
            sysctl(&format!("net/ipv6/conf/{scope}/disable_ipv6"), "1")?;
        }
        run(
            ip,
            &["-batch", "-"],
            Some(&batch),
            "configuring the job end",
        )
        .map(drop)
    })?
}

/// Runs `body` on a thread of its own that has joined `netns` (a namespace is a property
/// of a thread, so no other thread of the agent is affected, and a program started by the
/// body runs in it).
fn in_netns<T: Send>(netns: &File, body: impl FnOnce() -> T + Send) -> Result<T, EgressError> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                move_into_link_name_space(netns.as_fd(), Some(LinkNameSpaceType::Network))
                    .map_err(|why| command("joining the container's network namespace", why))?;
                Ok(body())
            })
            .join()
            .unwrap_or_else(|_| {
                Err(command(
                    "joining the container's network namespace",
                    "the thread panicked",
                ))
            })
    })
}

/// Writes `value` to the sysctl `path` (below `/proc/sys`) as seen from the calling
/// thread's network namespace.
fn sysctl(path: &str, value: &str) -> Result<(), EgressError> {
    let file = format!("/proc/sys/{path}");
    match std::fs::write(&file, value) {
        // A host booted without IPv6 has no such switch: IPv6 is off already.
        Err(why) if why.kind() == std::io::ErrorKind::NotFound && path.starts_with("net/ipv6/") => {
            Ok(())
        }
        other => other.map_err(|why| command(format!("setting {path}"), why)),
    }
}

/// Runs `program args`, with `stdin` if there is any, and returns what it printed.
fn run(
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
    what: &str,
) -> Result<String, EgressError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|why| command(what, format!("{program}: {why}")))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())
            .map_err(|why| command(what, why))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|why| command(what, format!("{program}: {why}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(command(
            what,
            format!("{program} {args:?}: {}", stderr.trim()),
        ))
    }
}

/// Both tables as the kernel holds them, normalized.
fn listing(config: &Config) -> Result<Value, EgressError> {
    let mut tables = Vec::new();
    for (family, table) in [("inet", TABLE), ("ip", NAT_TABLE)] {
        let text = run(
            config.nft(),
            &["-j", "list", "table", family, table],
            None,
            "reading the firewall back",
        )?;
        tables.push(
            ruleset::normalize(&text).map_err(|why| command("reading the firewall back", why))?,
        );
    }
    Ok(Value::Array(tables))
}

/// Removes every interface an earlier agent made and both tables.
fn sweep(config: &Config) -> Result<(), EgressError> {
    let text = run(
        config.ip(),
        &["-j", "link", "show"],
        None,
        "listing the interfaces",
    )?;
    let links: Vec<Value> =
        serde_json::from_str(&text).map_err(|why| command("listing the interfaces", why))?;
    for name in links
        .iter()
        .filter_map(|link| link.get("ifname").and_then(Value::as_str))
        .filter(|name| name.starts_with(IF_PREFIX) || name.starts_with(PEER_PREFIX))
    {
        // Deleting one end of a pair deletes the other, which may be later in the list.
        match run(
            config.ip(),
            &["link", "del", name],
            None,
            "deleting a leftover veth",
        ) {
            Err(EgressError::Command { why, .. }) if why.contains("Cannot find device") => {}
            other => other.map(drop)?,
        }
    }
    // The same transaction `start` loads, without the tables: the add-then-delete pair.
    run(
        config.nft(),
        &["-f", "-"],
        Some(&format!(
            "add table inet {TABLE}\ndelete table inet {TABLE}\n\
             add table ip {NAT_TABLE}\ndelete table ip {NAT_TABLE}\n"
        )),
        "removing the earlier tables",
    )?;
    Ok(())
}
