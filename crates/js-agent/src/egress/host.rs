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
//! For a run whose container's first process is `pid` ([`Egress::attach`]):
//!
//! 1. take a /30 of the pool, and the names `jsvN` (host end) and `jspN` (job end);
//! 2. read the identity of `/proc/<pid>/ns/net` and refuse the host's own;
//! 3. in the initial namespace: make the veth pair, turn IPv6 off on the host end and
//!    reverse-path filtering on, address the host end and set it up;
//! 4. add `jsvN` to the firewall's set of wired interfaces;
//! 5. read the identity again (the namespace must be the same one), and move the job end
//!    into the namespace, named `eth0`;
//! 6. enter that namespace with `nsenter` and, in one `ip -batch`: raise `lo`, give `eth0` no
//!    IPv6 link-local address, address and raise it, and add the default route last.
//!
//! Releasing a run removes the set member and the host end (which takes the job end with
//! it); the container's namespace is the kernel's to free when its last process ends, so
//! nothing of it outlives the run. A restart of the agent starts by removing every `jsv*`
//! and `jsp*` interface and the two tables, because the keeper kills the containers with
//! the agent: nothing a previous process wired is in use.
//!
//! Everything the agent does to the machine goes through [`Sys`], so the unit tests script
//! the whole sequence, including a failure at each step, and the tests that need the real
//! kernel are in `tests/egress`.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::config::{Config, Mode};
use super::ruleset::{self, IF_PREFIX, IFACE_SET, NAT_TABLE, TABLE};

/// The name of the job's end of a pair before it is moved.
const PEER_PREFIX: &str = "jsp";

/// The name every job's interface has inside its container.
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
    /// The device and inode of what `path` names, followed: for `/proc/<pid>/ns/net` the
    /// identity of that network namespace.
    fn identity(&self, path: &str) -> std::io::Result<(u64, u64)>;
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
}

/// Runs `program args` with `stdin` as its input to its end.
fn spawn(program: &str, args: &[&str], stdin: &str) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(program)
        .args(args)
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

/// The host side of the egress mode: the pool of address blocks and the firewall. Cheap to
/// clone; every clone is the same pool.
#[derive(Debug, Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
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
    /// The block's index and the host end's name, none in mode none or once taken down.
    wired: Option<(u32, String)>,
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
    /// Starts the host side for `config`: removes what an earlier agent left (every `jsv*`
    /// and `jsp*` interface, both tables), and in allow-list mode switches IPv4 forwarding
    /// on and loads the firewall.
    ///
    /// # Errors
    ///
    /// A program could not run or refused what it was given.
    pub fn start(config: Config) -> Result<Self, EgressError> {
        Self::start_on(config, Arc::new(System))
    }

    fn start_on(config: Config, sys: Arc<dyn Sys>) -> Result<Self, EgressError> {
        sweep(&*sys, &config)?;
        let mut baseline = None;
        if config.mode() == Mode::AllowList {
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
        let attachment = Attachment {
            inner: inner.clone(),
            wired: Some((index, format!("{IF_PREFIX}{index}"))),
        };
        // On failure the attachment drops here and takes down whatever was made.
        wire(&*inner.sys, &inner.config, index, pid).map_err(|why| match why {
            container @ EgressError::Container { .. } => container,
            other => command(format!("wiring run {run}"), other),
        })?;
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
        let (config, sys) = (&self.inner.config, &*self.inner.sys);
        let element = format!("{{ \"{name}\" }}");
        let removed = call(
            sys,
            "removing the interface from the firewall set",
            config.nft(),
            &["delete", "element", "inet", TABLE, IFACE_SET, &element],
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

/// The steps of the module documentation, for block `index` and container `pid`.
fn wire(sys: &dyn Sys, config: &Config, index: u32, pid: u32) -> Result<(), EgressError> {
    let (host_ip, job_ip) = config.pool().block(index).ok_or(EgressError::Exhausted)?;
    let (host, peer) = (
        format!("{IF_PREFIX}{index}"),
        format!("{PEER_PREFIX}{index}"),
    );
    let (ip, nft) = (config.ip(), config.nft());
    let container = |why: String| EgressError::Container { pid, why };

    // The namespace is identified before anything is made, and again before the job end is
    // moved into it, so what is wired is the namespace seen now and not whatever a reused
    // pid names later.
    let path = format!("/proc/{pid}/ns/net");
    let theirs = sys
        .identity(&path)
        .map_err(|why| container(format!("{path}: {why}")))?;
    let own = sys
        .identity("/proc/self/ns/net")
        .map_err(|why| command("reading the agent's own network namespace", why))?;
    if own == theirs {
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
    sysctl(sys, &format!("net/ipv4/conf/{host}/rp_filter"), "1")?;
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
            IFACE_SET,
            &format!("{{ \"{host}\" }}"),
        ],
    )?;

    let again = sys
        .identity(&path)
        .map_err(|why| container(format!("{path}: {why}")))?;
    if again != theirs {
        return Err(container("its network namespace changed".to_owned()));
    }
    step(
        "moving the job end into the container",
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
        &[&format!("--net={path}"), "--", ip, "-batch", "-"],
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

/// Removes every interface an earlier agent made and both tables.
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
        .filter(|name| name.starts_with(IF_PREFIX) || name.starts_with(PEER_PREFIX));
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
