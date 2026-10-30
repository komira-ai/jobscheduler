//! What the tests share on the host side: tools, the lock, namespaces entered by a thread,
//! the far host and its listeners, the holder, and the readings of the firewall.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use js_agent::egress::{Config, Flags};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};
use serde_json::Value;

static SERIAL: Mutex<()> = Mutex::new(());

/// Held by a test for its whole run.
pub fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The absolute path of `name`; a tool that is not installed fails the test.
pub fn tool(name: &str) -> String {
    for dir in ["/usr/sbin", "/usr/bin", "/sbin", "/bin"] {
        let path = PathBuf::from(dir).join(name);
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    panic!("{name} is not installed");
}

/// The environment variable `name`, which the CI job sets.
pub fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}: see the module docs"))
}

/// `program args...`, whatever it did.
pub fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|why| panic!("cannot start {program}: {why}"))
}

/// What `output` printed, standard output then standard error.
pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `program args...`, which must succeed; returns what it printed.
pub fn must(program: &str, args: &[&str]) -> String {
    let output = run(program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        text(&output)
    );
    text(&output)
}

/// `ip args...`, which must succeed.
pub fn ip(args: &[&str]) -> String {
    must(&tool("ip"), args)
}

/// `nft args...`, which must succeed.
pub fn nft(args: &[&str]) -> String {
    must(&tool("nft"), args)
}

/// Writes a sysctl of the host.
pub fn sysctl(path: &str, value: &str) {
    std::fs::write(format!("/proc/sys/{path}"), value)
        .unwrap_or_else(|why| panic!("cannot write {path}: {why}"));
}

/// Reads a sysctl of the host.
pub fn sysctl_get(path: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/{path}"))
        .unwrap_or_else(|why| panic!("cannot read {path}: {why}"))
        .trim()
        .to_owned()
}

/// The allow-list configuration of a test: the endpoints and resolvers given, the default
/// pool and programs.
pub fn allow_list(allow: &[&str], dns: &[&str]) -> Config {
    let allow: Vec<String> = allow.iter().map(|s| (*s).to_owned()).collect();
    let dns: Vec<String> = dns.iter().map(|s| (*s).to_owned()).collect();
    Config::from_flags(&Flags {
        mode: Some("allow-list"),
        allow: &allow,
        dns: &dns,
        ..Flags::default()
    })
    .expect("a sound configuration")
}

/// Runs `body` on a thread that has joined `netns`: its sockets, and the programs it starts,
/// are in that namespace.
pub fn in_netns<T: Send>(netns: &File, body: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                move_into_link_name_space(netns.as_fd(), Some(LinkNameSpaceType::Network))
                    .expect("setns into the network namespace");
                body()
            })
            .join()
            .expect("the namespace thread panicked")
    })
}

/// `ip args...` in `netns`.
pub fn ip_in(netns: &File, args: &[&str]) -> Output {
    let program = tool("ip");
    in_netns(netns, || run(&program, args))
}

/// What a connection from a namespace came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// It connected and the server said this.
    Connected(String),
    /// It did not, with the error.
    Failed(String),
}

impl Reach {
    /// Whether the connection was made and answered.
    pub fn connected(&self) -> bool {
        matches!(self, Self::Connected(banner) if banner.contains("pong"))
    }
}

/// A TCP connection from `netns` to `to`, and what the server says.
pub fn tcp(netns: &File, to: SocketAddr) -> Reach {
    in_netns(netns, move || {
        match TcpStream::connect_timeout(&to, Duration::from_secs(3)) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut said = String::new();
                let _ = stream.read_to_string(&mut said);
                Reach::Connected(said)
            }
            Err(why) => Reach::Failed(format!("{:?}: {why}", why.kind())),
        }
    })
}

/// Whether a datagram from `netns` to `to` was answered.
pub fn udp(netns: &File, to: SocketAddr) -> bool {
    in_netns(netns, move || {
        let bind = if to.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let Ok(socket) = UdpSocket::bind(bind) else {
            return false;
        };
        let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buffer = [0u8; 16];
        socket.connect(to).is_ok()
            && socket.send(b"query").is_ok()
            && socket.recv(&mut buffer).is_ok()
    })
}

/// A listener in a namespace that counts what it accepts, says `pong` and closes.
pub struct Server {
    accepted: Arc<AtomicU64>,
    peers: Arc<Mutex<Vec<IpAddr>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(
        netns: &File,
        body: impl FnOnce(&Arc<AtomicU64>, &Arc<Mutex<Vec<IpAddr>>>, &AtomicBool) + Send + 'static,
    ) -> Self {
        let netns = netns.try_clone().expect("clone the namespace file");
        let accepted = Arc::new(AtomicU64::new(0));
        let peers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, seen, flag) = (accepted.clone(), peers.clone(), stop.clone());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            move_into_link_name_space(netns.as_fd(), Some(LinkNameSpaceType::Network))
                .expect("setns");
            let _ = ready_tx.send(());
            body(&count, &seen, &flag);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the server thread joined its namespace");
        // The body binds first thing; give it a moment so a connect never beats the bind.
        std::thread::sleep(Duration::from_millis(200));
        Self {
            accepted,
            peers,
            stop,
            thread: Some(thread),
        }
    }

    /// A TCP listener on `addr` in `netns`.
    pub fn tcp(netns: &File, addr: SocketAddr) -> Self {
        Self::start(netns, move |count, peers, stop| {
            let listener = TcpListener::bind(addr)
                .unwrap_or_else(|why| panic!("cannot listen on {addr}: {why}"));
            listener.set_nonblocking(true).expect("nonblocking");
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        count.fetch_add(1, Ordering::Relaxed);
                        peers.lock().unwrap().push(peer.ip());
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.write_all(b"pong\n");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
    }

    /// A UDP responder on `addr` in `netns`: it answers every datagram.
    pub fn udp(netns: &File, addr: SocketAddr) -> Self {
        Self::start(netns, move |count, peers, stop| {
            let socket =
                UdpSocket::bind(addr).unwrap_or_else(|why| panic!("cannot bind {addr}: {why}"));
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .expect("timeout");
            let mut buffer = [0u8; 64];
            while !stop.load(Ordering::Relaxed) {
                if let Ok((_, peer)) = socket.recv_from(&mut buffer) {
                    count.fetch_add(1, Ordering::Relaxed);
                    peers.lock().unwrap().push(peer.ip());
                    let _ = socket.send_to(b"answer", peer);
                }
            }
        })
    }

    /// How many connections (or datagrams) reached it.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// The source address of each of them, as the server saw it.
    pub fn peers(&self) -> Vec<IpAddr> {
        self.peers.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The addresses of the far host and what is on them.
pub mod far {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    /// The host's own end of the link to the far host: what the far host sees when the
    /// host masquerades.
    pub const HOST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    /// An allowed endpoint (`--egress-allow 192.0.2.10:9300`).
    pub const ALLOWED: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 10), 9300));
    /// The same host, another port.
    pub const OTHER_PORT: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 10), 9301));
    /// The same port, another host.
    pub const OTHER_HOST: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 40), 9300));
    /// The metadata address, routed to the far host so that it is forwarded and never local.
    pub const METADATA: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(169, 254, 169, 254), 80));
    /// The configured resolver.
    pub const DNS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 53), 53));
    /// A resolver that is not configured.
    pub const DNS_OTHER: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 54), 53));
    /// An IPv6 endpoint on the far host.
    pub const V6: &str = "[fd00:f::2]:9300";
}

/// A second network namespace behind the host's routing: another host. It has one link to
/// the host (`vfar0` on the host, 192.0.2.1/24 and fd00:f::1/64), the addresses of [`far`]
/// on its loopback, no route back to anything else, and nothing in it is local to the host.
pub struct Far {
    /// The far namespace.
    pub ns: File,
}

const FAR_NS: &str = "js-far";

impl Far {
    /// Makes it.
    pub fn new() -> Self {
        let ip = tool("ip");
        let _ = run(&ip, &["netns", "del", FAR_NS]);
        let _ = run(&ip, &["link", "del", "vfar0"]);
        ip_args(&["netns", "add", FAR_NS]);
        ip_args(&[
            "link", "add", "vfar0", "type", "veth", "peer", "name", "vfar1",
        ]);
        ip_args(&["link", "set", "vfar1", "netns", FAR_NS]);
        ip_args(&["addr", "add", "192.0.2.1/24", "dev", "vfar0"]);
        ip_args(&["-6", "addr", "add", "fd00:f::1/64", "dev", "vfar0", "nodad"]);
        ip_args(&["link", "set", "vfar0", "up"]);
        let inside = |args: &[&str]| {
            let mut all = vec!["-n", FAR_NS];
            all.extend_from_slice(args);
            ip_args(&all);
        };
        inside(&["link", "set", "lo", "up"]);
        inside(&["addr", "add", "192.0.2.2/24", "dev", "vfar1"]);
        inside(&["-6", "addr", "add", "fd00:f::2/64", "dev", "vfar1", "nodad"]);
        inside(&["link", "set", "vfar1", "up"]);
        for addr in [
            "192.0.2.10/32",
            "192.0.2.40/32",
            "192.0.2.53/32",
            "192.0.2.54/32",
            "169.254.169.254/32",
        ] {
            inside(&["addr", "add", addr, "dev", "lo"]);
        }
        // The far host has no route to the jobs' addresses (the host masquerades) nor to
        // the IPv6 networks the jobs invent, except what the test adds.
        // Only what comes in from a job's link goes there: the runner's own agents talk to the
        // real metadata address, and a route for them would count as a connection of a job.
        let _ = run(&ip, &["rule", "del", "pref", "4242"]);
        ip_args(&[
            "rule",
            "add",
            "pref",
            "4242",
            "iif",
            "jsv0",
            "to",
            "169.254.169.254/32",
            "lookup",
            "4242",
        ]);
        ip_args(&[
            "route",
            "add",
            "169.254.169.254/32",
            "via",
            "192.0.2.2",
            "table",
            "4242",
        ]);
        sysctl("net/ipv6/conf/all/forwarding", "1");
        let ns = File::open(format!("/run/netns/{FAR_NS}")).expect("open the far namespace");
        Self { ns }
    }

    /// `ip args...` in the far namespace.
    pub fn ip(&self, args: &[&str]) -> String {
        let output = ip_in(&self.ns, args);
        assert!(output.status.success(), "ip {args:?}: {}", text(&output));
        text(&output)
    }
}

fn ip_args(args: &[&str]) {
    ip(args);
}

impl Drop for Far {
    fn drop(&mut self) {
        let ip = tool("ip");
        let _ = run(&ip, &["netns", "del", FAR_NS]);
        let _ = run(&ip, &["link", "del", "vfar0"]);
        let _ = run(&ip, &["rule", "del", "pref", "4242"]);
        let _ = run(&ip, &["route", "flush", "table", "4242"]);
    }
}

/// A process in a network namespace of its own that nothing else is in: a stand-in for a
/// container's first process.
pub struct Holder {
    child: Child,
    /// An open handle on the namespace.
    pub ns: File,
}

impl Holder {
    /// Starts it and waits until the namespace exists.
    pub fn new() -> Self {
        let child = Command::new(tool("unshare"))
            .args(["--net", "--", &tool("sleep"), "600"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("unshare");
        let own = std::fs::metadata("/proc/self/ns/net").expect("own namespace");
        let path = format!("/proc/{}/ns/net", child.id());
        let started = Instant::now();
        loop {
            use std::os::unix::fs::MetadataExt as _;
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.ino() != own.ino()
            {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no namespace");
            std::thread::sleep(Duration::from_millis(10));
        }
        let ns = File::open(&path).expect("open the namespace");
        Self { child, ns }
    }

    /// The pid of the process in the namespace.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Ends the process; the namespace goes with it once the handle is dropped too.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The JSON of `nft -j list table inet js_crun_egress`.
fn table() -> Value {
    serde_json::from_str(&nft(&["-j", "list", "table", "inet", "js_crun_egress"]))
        .expect("nft json")
}

/// The packets that the rule commented `comment` of the table `js_crun_egress` has counted.
pub fn packets(comment: &str) -> u64 {
    counted(&table(), comment)
}

/// The packets of the masquerade rule.
pub fn masquerade_packets() -> u64 {
    let json: Value =
        serde_json::from_str(&nft(&["-j", "list", "table", "ip", "js_crun_egress_nat"]))
            .expect("nft json");
    counted(&json, "masquerade")
}

fn counted(listing: &Value, comment: &str) -> u64 {
    listing["nftables"]
        .as_array()
        .expect("nftables")
        .iter()
        .filter_map(|item| item.get("rule"))
        .filter(|rule| rule["comment"] == comment)
        .flat_map(|rule| rule["expr"].as_array().expect("expr").iter())
        .find_map(|expr| expr["counter"]["packets"].as_u64())
        .unwrap_or_else(|| panic!("no counted rule {comment:?}"))
}

/// Removes both tables of the agent, as a fresh host has none (mode none leaves whatever an
/// earlier test loaded alone).
pub fn clear_tables() {
    let nft = tool("nft");
    let _ = run(&nft, &["delete", "table", "inet", "js_crun_egress"]);
    let _ = run(&nft, &["delete", "table", "ip", "js_crun_egress_nat"]);
}

/// Deletes the rule commented `comment` from the forward chain.
pub fn delete_rule(comment: &str) {
    let listing: Value = serde_json::from_str(&nft(&[
        "-j",
        "list",
        "chain",
        "inet",
        "js_crun_egress",
        "forward",
    ]))
    .expect("nft json");
    let handle = listing["nftables"]
        .as_array()
        .expect("nftables")
        .iter()
        .filter_map(|item| item.get("rule"))
        .find(|rule| rule["comment"] == comment)
        .map(|rule| rule["handle"].as_u64().expect("a handle"))
        .expect("the rule is there");
    nft(&[
        "delete",
        "rule",
        "inet",
        "js_crun_egress",
        "forward",
        "handle",
        &handle.to_string(),
    ]);
}

/// The members of the set of wired interfaces, sorted; empty when the set (or the table)
/// does not exist.
pub fn wired() -> Vec<String> {
    let output = run(
        &tool("nft"),
        &["-j", "list", "set", "inet", "js_crun_egress", "jobifs"],
    );
    if !output.status.success() {
        return Vec::new();
    }
    let json: Value = serde_json::from_slice(&output.stdout).expect("nft json");
    let mut names: Vec<String> = json["nftables"]
        .as_array()
        .expect("nftables")
        .iter()
        .filter_map(|item| item.get("set"))
        .flat_map(|set| set["elem"].as_array().cloned().unwrap_or_default())
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// Whether a table of this name is loaded.
pub fn table_loaded(family: &str, name: &str) -> bool {
    run(&tool("nft"), &["list", "table", family, name])
        .status
        .success()
}

/// The names of the host's interfaces that start with `prefix`, sorted.
pub fn links(prefix: &str) -> Vec<String> {
    let json: Vec<Value> = serde_json::from_str(&ip(&["-j", "link", "show"])).expect("ip json");
    let mut names: Vec<String> = json
        .iter()
        .filter_map(|link| link["ifname"].as_str())
        .filter(|name| name.starts_with(prefix))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The entries of `/run/netns`, sorted.
pub fn named_namespaces() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("/run/netns")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Polls `condition` until it holds, for at most `limit`.
pub fn eventually(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < limit {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}
